//! The widget game: owns the character's per-tick pipeline.
//!
//! Tick order mirrors airi's VRMModel update (mixer → humanoid → lookAt →
//! blink → expressions → constraints → springs), mapped onto the Pocket
//! shape: sample clip locals → eye look-at → spring bones → globals →
//! palette; blink lands as morph weights, uploaded only when it changes.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use glam::{Mat4, Vec3};
use pocket3d::anim::NodeTrs;
use pocket3d::app::Game;
use pocket3d::camera::Camera;
use pocket3d::gpu::Gpu;
use pocket3d::hud::Hud;
use pocket3d::input::Input;
use pocket3d::model::{ModelAsset, ModelInstance, ModelLoadOptions};
use pocket3d::renderer::Renderer;
use pocket3d::scene::Scene;
use pocket_character_core::{CharacterSim, TrackingMode};
use pocket_vrm::{SpringSolver, VrmDoc};

use crate::guest::{CharacterGuest, Command, TickEvent, TickState};

pub struct WidgetConfig {
    pub model_path: PathBuf,
    pub vrma_path: PathBuf,
    pub bundle_path: PathBuf,
    pub size: (u32, u32),
    /// Render N frames then exit (verification runs).
    pub frames: Option<u32>,
}

/// Character-specific camera framing policy.
///
/// `distance_scale` is relative to the model's AABB height, while
/// `headroom` is the fraction of the vertical viewport below its top edge.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CameraSettings {
    pub fov_deg: f32,
    pub distance_scale: f32,
    pub headroom: f32,
}

impl Default for CameraSettings {
    fn default() -> Self {
        Self {
            fov_deg: 40.0,
            distance_scale: 0.6,
            headroom: 0.05,
        }
    }
}

impl CameraSettings {
    /// Keep live settings in a valid range so camera math cannot produce a
    /// degenerate projection or camera distance.
    fn sanitized(self) -> Self {
        let defaults = Self::default();
        Self {
            fov_deg: if self.fov_deg.is_finite() {
                self.fov_deg.clamp(1.0, 179.0)
            } else {
                defaults.fov_deg
            },
            distance_scale: if self.distance_scale.is_finite() {
                self.distance_scale.clamp(0.1, 10.0)
            } else {
                defaults.distance_scale
            },
            // More than half a viewport of headroom would put the target
            // above the top of the frame and is not useful framing policy.
            headroom: if self.headroom.is_finite() {
                self.headroom.clamp(0.0, 0.49)
            } else {
                defaults.headroom
            },
        }
    }
}

const MIN_MODEL_HEIGHT: f32 = 0.001;
/// Rest-pose AABBs can omit a small animated excursion from hair/accessories.
const TOP_SAFETY_MARGIN: f32 = 0.02;

#[derive(Clone, Copy, Debug, PartialEq)]
struct CameraFrame {
    target: Vec3,
    distance: f32,
    fov_y: f32,
    view_height: f32,
}

/// Resolve character-owned framing settings against model bounds.
///
/// The top of the model (including the safety margin) is placed at the
/// requested normalized headroom below the top of the vertical viewport.
fn resolve_camera_frame(aabb: (Vec3, Vec3), settings: CameraSettings) -> CameraFrame {
    let settings = settings.sanitized();
    let (min, max) = aabb;
    let height = (max.y - min.y).max(MIN_MODEL_HEIGHT);
    let distance = height * settings.distance_scale;
    let fov_y = settings.fov_deg.to_radians();
    let view_height = 2.0 * distance * (fov_y * 0.5).tan();
    let framed_top = max.y + height * TOP_SAFETY_MARGIN;
    let target_y = framed_top - (0.5 - settings.headroom) * view_height;

    CameraFrame {
        target: Vec3::new((min.x + max.x) * 0.5, target_y, (min.z + max.z) * 0.5),
        distance,
        fov_y,
        view_height,
    }
}

/// Rolling frame stats fed to the guest and to the measurement harness.
struct FrameStats {
    frames: u32,
    cpu_ms_acc: f32,
    window_start: Instant,
    pub fps: f32,
    pub frame_ms: f32,
}

impl FrameStats {
    fn new() -> Self {
        Self {
            frames: 0,
            cpu_ms_acc: 0.0,
            window_start: Instant::now(),
            fps: 0.0,
            frame_ms: 0.0,
        }
    }

    fn record(&mut self, cpu_ms: f32) {
        self.frames += 1;
        self.cpu_ms_acc += cpu_ms;
        let elapsed = self.window_start.elapsed().as_secs_f32();
        if elapsed >= 1.0 {
            self.fps = self.frames as f32 / elapsed;
            self.frame_ms = self.cpu_ms_acc / self.frames.max(1) as f32;
            self.frames = 0;
            self.cpu_ms_acc = 0.0;
            self.window_start = Instant::now();
        }
    }
}

pub struct Widget {
    cfg: WidgetConfig,
    guest: Option<CharacterGuest>,

    // Loaded in init (needs the GPU).
    model: Option<Arc<ModelAsset>>,
    vrm: Option<VrmDoc>,
    clips: Vec<(String, pocket3d::anim::Clip)>,
    springs: Option<SpringSolver>,

    // Pose pipeline state.
    sim: CharacterSim,
    locals: Vec<NodeTrs>,
    globals: Vec<Mat4>,
    clip_index: usize,
    clip_time: f32,
    clip_looping: bool,
    blink_binds: Vec<(usize, usize, f32)>, // (morph mesh slot, target, weight)

    scene: Scene,
    camera: Camera,
    hud: Hud,
    anchor: Vec3,
    camera_settings: CameraSettings,

    stats: FrameStats,
    tick_count: u64,
    hovered: bool,
    pending_events: Vec<TickEvent>,
    exit: bool,
    rendered_frames: u32,
}

impl Widget {
    pub fn new(cfg: WidgetConfig) -> Self {
        // Seed fixed for reproducible measurement runs; behavior parity is
        // distributional, not per-run.
        let sim = CharacterSim::new(0x0c9a_11e0, Vec3::ZERO);
        Self {
            cfg,
            guest: None,
            model: None,
            vrm: None,
            clips: Vec::new(),
            springs: None,
            sim,
            locals: Vec::new(),
            globals: Vec::new(),
            clip_index: 0,
            clip_time: 0.0,
            clip_looping: true,
            blink_binds: Vec::new(),
            scene: Scene::default(),
            camera: Camera::default(),
            hud: Hud::default(),
            anchor: Vec3::ZERO,
            camera_settings: CameraSettings::default(),
            stats: FrameStats::new(),
            tick_count: 0,
            hovered: false,
            pending_events: Vec::new(),
            exit: false,
            rendered_frames: 0,
        }
    }

    /// Update character framing live. The next composed frame uses the new
    /// camera without recreating the renderer, surface, or pipelines.
    pub fn set_camera_settings(&mut self, settings: CameraSettings) {
        self.camera_settings = settings.sanitized();
        if let Some(aabb) = self.model.as_ref().map(|model| model.aabb) {
            self.apply_camera_settings(aabb);
        }
    }

    fn apply_camera_settings(&mut self, aabb: (Vec3, Vec3)) {
        let frame = resolve_camera_frame(aabb, self.camera_settings);
        self.anchor = frame.target;
        self.camera.fov_y = frame.fov_y;
        self.camera.znear = 0.05;
        self.camera.pos = frame.target + Vec3::new(0.0, 0.0, -frame.distance);
        self.camera.look_at(frame.target);
        self.sim.look_base = self.camera.pos;
        self.sim.mouse_target = self.camera.pos;
    }

    fn apply_commands(&mut self, commands: Vec<Command>) {
        for cmd in commands {
            match cmd {
                Command::SetTracking(mode) => {
                    self.sim.tracking = match mode.as_str() {
                        "mouse" => TrackingMode::Mouse,
                        _ => TrackingMode::None,
                    };
                }
                Command::SetExpression(name, w) => {
                    let Some((vrm, model)) = self.vrm.as_ref().zip(self.model.as_ref()) else {
                        continue;
                    };
                    apply_expression(vrm, model, &mut self.scene, &name, w);
                }
                Command::PlayClip { name, looping } => {
                    if let Some(i) = self.clips.iter().position(|(n, _)| *n == name) {
                        self.clip_index = i;
                        self.clip_time = 0.0;
                        self.clip_looping = looping;
                    } else {
                        log::warn!("character.playClip: unknown clip '{name}'");
                    }
                }
                Command::SetMaxFps(_fps) => {
                    // The app loop owns pacing; a runtime-adjustable cap needs
                    // an AppConfig hook (candidate follow-up).
                    log::warn!("character.setMaxFps: fixed at launch for now");
                }
                Command::Quit => self.exit = true,
            }
        }
    }
}

/// Resolve a named VRM expression to morph weights on the instance.
fn apply_expression(vrm: &VrmDoc, model: &Arc<ModelAsset>, scene: &mut Scene, name: &str, w: f32) {
    let Some(inst) = scene.models.first_mut() else {
        return;
    };
    let Some(morph) = inst.morph.as_mut() else {
        return;
    };
    for expr in &vrm.expressions {
        if expr.name == name {
            for bind in &expr.binds {
                if let Some(slot) = model.morph_mesh_slot(bind.mesh) {
                    morph.set_weight(slot, bind.target, w * bind.weight);
                }
            }
        }
    }
}

impl Game for Widget {
    fn init(&mut self, gpu: &Gpu, renderer: &mut Renderer) -> Result<()> {
        let t0 = Instant::now();
        // 2048 halves the 4096² authoring textures: invisible at 450×600,
        // and GPU texture memory is the widget's dominant footprint.
        let model = ModelAsset::load_glb_opts(
            gpu,
            &renderer.model_material_layout,
            &renderer.samplers,
            &self.cfg.model_path,
            &ModelLoadOptions {
                max_texture_dim: Some(2048),
            },
        )
        .context("loading VRM model")?;
        let vrm = VrmDoc::from_path(&self.cfg.model_path).context("parsing VRM extension")?;

        // Retarget the idle animation onto this rig.
        let vrma_bytes = std::fs::read(&self.cfg.vrma_path).context("reading vrma")?;
        let vrma = pocket_vrm::load_vrma_bytes(&vrma_bytes)?;
        let clip = pocket_vrm::retarget(&vrma, &vrm.humanoid, &model.skeleton)?;
        let clip_name = self
            .cfg
            .vrma_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "idle".into());
        self.clips = vec![(clip_name, clip)];

        // Springs seeded from the rest pose.
        model
            .skeleton
            .sample_locals(None, 0.0, false, &mut self.locals);
        self.springs = Some(SpringSolver::new(&vrm.springs, &model.skeleton, &self.locals));

        // Blink expression → morph slots.
        for expr in &vrm.expressions {
            if expr.name == "blink" {
                for b in &expr.binds {
                    if let Some(slot) = model.morph_mesh_slot(b.mesh) {
                        self.blink_binds.push((slot, b.target, b.weight));
                    }
                }
            }
        }
        if self.blink_binds.is_empty() {
            log::warn!("model has no 'blink' expression; blinking disabled");
        }

        // Scene: one instance, transparent background, near-unlit shading
        // (MToon reads mostly flat; sun/hemisphere would double-shade it).
        let mut inst = ModelInstance::new(model.clone());
        inst.morph = model.create_morph_state(gpu);
        inst.cutout = 0.5;
        inst.lit = 0.25;
        self.scene.transparent_clear = true;
        self.scene.models.push(inst);

        // Camera framing is character-owned and derived from the loaded
        // model's bounds. It remains live through set_camera_settings().
        self.model = Some(model.clone());
        self.set_camera_settings(self.camera_settings);

        // Guest boots last so its boot table reflects the loaded assets.
        let bundle = std::fs::read_to_string(&self.cfg.bundle_path)
            .with_context(|| format!("reading bundle {}", self.cfg.bundle_path.display()))?;
        let clip_names: Vec<String> = self.clips.iter().map(|(n, _)| n.clone()).collect();
        let expr_names: Vec<String> = vrm.expressions.iter().map(|e| e.name.clone()).collect();
        self.guest = Some(CharacterGuest::boot(
            &bundle,
            "AvatarSample_A",
            &clip_names,
            &expr_names,
        )?);

        self.vrm = Some(vrm);
        log::info!("init: {:.0} ms", t0.elapsed().as_secs_f32() * 1000.0);
        Ok(())
    }

    fn frame(&mut self, _dt: f32, input: &Input) {
        let hovered = input.cursor().is_some();
        if hovered != self.hovered {
            self.hovered = hovered;
            self.pending_events.push(if hovered {
                TickEvent::HoverStart
            } else {
                TickEvent::HoverEnd
            });
        }
    }

    fn tick(&mut self, dt: f32, input: &Input) {
        let t0 = Instant::now();
        let (Some(model), Some(vrm)) = (self.model.clone(), self.vrm.as_ref()) else {
            return;
        };
        self.tick_count += 1;

        // --- sim --------------------------------------------------------
        let out = self.sim.tick(dt);

        // --- clip -------------------------------------------------------
        self.clip_time += dt;
        let clip = self.clips.get(self.clip_index).map(|(_, c)| c);
        model
            .skeleton
            .sample_locals(clip, self.clip_time, self.clip_looping, &mut self.locals);

        // --- eyes -------------------------------------------------------
        // Yaw/pitch from the head toward the look target (model space).
        self.globals.resize(self.locals.len(), Mat4::IDENTITY);
        model
            .skeleton
            .globals_from_locals(&self.locals, &mut self.globals);
        let head = vrm
            .humanoid_node("head")
            .map(|n| self.globals[n].w_axis.truncate());
        if let Some(head_pos) = head {
            // Character forward is -Z; yaw > 0 = its left (-X), pitch > 0 = up.
            let d = out.look_target - head_pos;
            let yaw = (-d.x).atan2(-d.z).to_degrees();
            let pitch = d.y.atan2(Vec3::new(d.x, 0.0, d.z).length()).to_degrees();
            pocket_vrm::apply_eye_look(
                &mut self.locals,
                &model.skeleton.rest,
                vrm.humanoid_node("leftEye"),
                vrm.humanoid_node("rightEye"),
                &vrm.look_at,
                yaw,
                pitch,
            );
        }

        // --- springs ----------------------------------------------------
        if let Some(springs) = self.springs.as_mut() {
            springs.step(dt, &model.skeleton, &mut self.locals, Mat4::IDENTITY);
        }

        // --- pose + blink -----------------------------------------------
        model
            .skeleton
            .globals_from_locals(&self.locals, &mut self.globals);
        let inst = &mut self.scene.models[0];
        inst.pose = Some(self.globals.clone());
        if out.blink_changed {
            if let Some(morph) = inst.morph.as_mut() {
                for &(slot, target, w) in &self.blink_binds {
                    morph.set_weight(slot, target, out.blink * w);
                }
            }
        }

        // --- guest turn -------------------------------------------------
        let mut events: Vec<TickEvent> = std::mem::take(&mut self.pending_events);
        if input.mouse_button_pressed(pocket3d::winit::event::MouseButton::Left) {
            events.push(TickEvent::Click);
        }
        let state = TickState {
            t: self.tick_count as f64 * dt as f64,
            blink: out.blink,
            clip: self
                .clips
                .get(self.clip_index)
                .map(|(n, _)| n.clone())
                .unwrap_or_default(),
            hovered: self.hovered,
            tracking: match self.sim.tracking {
                TrackingMode::None => "none",
                TrackingMode::Mouse => "mouse",
            },
            fps: self.stats.fps,
            frame_ms: self.stats.frame_ms,
        };
        if let Some(guest) = &self.guest {
            match guest.turn(&state, &events) {
                Ok(commands) => self.apply_commands(commands),
                Err(e) => log::error!("guest turn: {e:#}"),
            }
        }

        self.stats.record(t0.elapsed().as_secs_f32() * 1000.0);
    }

    fn compose(&mut self, _alpha: f32, time: f32, _size: (u32, u32)) -> (&Scene, &Camera, &Hud) {
        self.scene.time = time;
        self.rendered_frames += 1;
        if let Some(n) = self.cfg.frames
            && self.rendered_frames >= n
        {
            self.exit = true;
        }
        (&self.scene, &self.camera, &self.hud)
    }

    fn wants_exit(&self) -> bool {
        self.exit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx_eq(actual: f32, expected: f32) {
        assert!((actual - expected).abs() < 1.0e-5, "{actual} != {expected}");
    }

    #[test]
    fn default_frame_keeps_top_bounds_inside_requested_headroom() {
        let aabb = (Vec3::new(-0.4, 0.0, -0.2), Vec3::new(0.6, 1.8, 0.4));
        let settings = CameraSettings::default();
        let frame = resolve_camera_frame(aabb, settings);
        let framed_top = aabb.1.y + (aabb.1.y - aabb.0.y) * TOP_SAFETY_MARGIN;
        let viewport_top = frame.target.y + frame.view_height * 0.5;

        approx_eq(
            viewport_top - framed_top,
            settings.headroom * frame.view_height,
        );
        assert!(viewport_top > aabb.1.y, "model top must not be cropped");
    }

    #[test]
    fn target_uses_aabb_horizontal_and_depth_center() {
        let frame = resolve_camera_frame(
            (Vec3::new(-2.0, 0.0, -4.0), Vec3::new(4.0, 1.8, 2.0)),
            CameraSettings::default(),
        );

        approx_eq(frame.target.x, 1.0);
        approx_eq(frame.target.z, -1.0);
    }

    #[test]
    fn distance_scales_with_model_height() {
        let settings = CameraSettings::default();
        let short = resolve_camera_frame(
            (Vec3::ZERO, Vec3::new(1.0, 1.0, 1.0)),
            settings,
        );
        let tall = resolve_camera_frame(
            (Vec3::ZERO, Vec3::new(1.0, 2.0, 1.0)),
            settings,
        );

        approx_eq(tall.distance / short.distance, 2.0);
        approx_eq(
            (tall.view_height / tall.distance),
            (short.view_height / short.distance),
        );
    }

    #[test]
    fn invalid_live_settings_are_sanitized() {
        let settings = CameraSettings {
            fov_deg: f32::NAN,
            distance_scale: -1.0,
            headroom: 1.0,
        };
        assert_eq!(settings.sanitized(), CameraSettings {
            fov_deg: 40.0,
            distance_scale: 0.1,
            headroom: 0.49,
        });

        assert_eq!(
            CameraSettings {
                distance_scale: 100.0,
                ..settings
            }
            .sanitized()
            .distance_scale,
            10.0
        );
    }

    #[test]
    fn live_settings_are_available_before_model_load() {
        let mut widget = Widget::new(WidgetConfig {
            model_path: PathBuf::new(),
            vrma_path: PathBuf::new(),
            bundle_path: PathBuf::new(),
            size: (450, 600),
            frames: None,
        });
        widget.set_camera_settings(CameraSettings {
            fov_deg: 35.0,
            distance_scale: 0.75,
            headroom: 0.08,
        });

        assert_eq!(widget.camera_settings.fov_deg, 35.0);
        assert_eq!(widget.camera_settings.distance_scale, 0.75);
        assert_eq!(widget.camera_settings.headroom, 0.08);
    }
}
