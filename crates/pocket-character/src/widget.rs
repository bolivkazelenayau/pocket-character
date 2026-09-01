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
use glam::{Mat4, Vec2, Vec3};
use pocket_character_core::{CharacterSim, TrackingMode};
use pocket_vrm::{SpringSolver, VrmDoc};
use pocket3d::anim::NodeTrs;
use pocket3d::app::Game;
use pocket3d::camera::Camera;
use pocket3d::gpu::Gpu;
use pocket3d::hud::Hud;
use pocket3d::input::Input;
use pocket3d::model::{ModelAsset, ModelInstance, ModelLoadOptions};
use pocket3d::renderer::Renderer;
use pocket3d::scene::Scene;
use pocket3d::winit::keyboard::KeyCode;

use crate::guest::{CharacterGuest, Command, TickEvent, TickState};
use crate::settings::{AntiAliasingPreference, AppSettings, CameraSettings};

mod camera;

use camera::{
    CameraRuntimeAdjustments, DEFAULT_VIEWPORT_ASPECT, EffectiveCameraValues, admit_pan_input,
    finite_clamped, finite_value, normalize_degrees, resolve_camera_parameters_with_aspect,
};

pub struct WidgetConfig {
    pub model_path: PathBuf,
    pub vrma_path: PathBuf,
    pub bundle_path: PathBuf,
    pub size: (u32, u32),
    /// Render N frames then exit (verification runs).
    pub frames: Option<u32>,
}

const CAMERA_FOV_RATE_DEG_PER_SEC: f32 = 45.0;
const CAMERA_DISTANCE_RATE_PER_SEC: f32 = 0.75;
/// Temporary keyboard tuning in rest-center NDC units per second. This
/// approximately preserves the previous manual pan feel for the default
/// character frame and is deliberately separate from the safety boundary.
const CAMERA_PAN_WITNESS_NDC_RATE_PER_SEC: f32 = 0.75;
const CAMERA_YAW_RATE_DEG_PER_SEC: f32 = 90.0;
const CAMERA_ROLL_RATE_DEG_PER_SEC: f32 = 90.0;
const ROLL_SNAP_DEG: f32 = 15.0;
const ROLL_SNAP_REPEAT_DELAY_SEC: f32 = 0.30;
const ROLL_SNAP_REPEAT_INTERVAL_SEC: f32 = 0.10;
const CAMERA_PITCH_RATE_DEG_PER_SEC: f32 = 60.0;
const CAMERA_CONTROL_HELP: [&str; 10] = [
    "Up / Down              Pan Y up / down",
    "Left / Right           Pan X left / right",
    "Shift + Up / Down      Zoom in / out",
    "Shift + Left / Right   Yaw left / right",
    "Ctrl + Up / Down       Pitch up / down",
    "Ctrl + Left/Right        Roll",
    "Ctrl + Shift + Left/Right Snap roll 15°",
    "Q / E                  Decrease / increase FOV",
    "R                      Reset runtime camera adjustments",
    "F8                     Toggle camera controls",
];

fn axis(input: &Input, positive: KeyCode, negative: KeyCode) -> f32 {
    (input.key_down(positive) as i8 - input.key_down(negative) as i8) as f32
}

fn modifier_down(input: &Input, left: KeyCode, right: KeyCode) -> bool {
    input.key_down(left) || input.key_down(right)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VerticalCameraAction {
    Pan,
    Zoom,
    Pitch,
}

fn vertical_camera_action(input: &Input) -> VerticalCameraAction {
    if modifier_down(input, KeyCode::ControlLeft, KeyCode::ControlRight) {
        VerticalCameraAction::Pitch
    } else if modifier_down(input, KeyCode::ShiftLeft, KeyCode::ShiftRight) {
        VerticalCameraAction::Zoom
    } else {
        VerticalCameraAction::Pan
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HorizontalCameraAction {
    SnapRoll,
    Yaw,
    Pan,
    Roll,
}

fn horizontal_camera_action(input: &Input) -> HorizontalCameraAction {
    let ctrl = modifier_down(input, KeyCode::ControlLeft, KeyCode::ControlRight);
    let shift = modifier_down(input, KeyCode::ShiftLeft, KeyCode::ShiftRight);
    if ctrl && shift {
        HorizontalCameraAction::SnapRoll
    } else if ctrl {
        HorizontalCameraAction::Roll
    } else if shift {
        HorizontalCameraAction::Yaw
    } else {
        HorizontalCameraAction::Pan
    }
}

fn snap_roll_degrees(current_deg: f32, direction: i8) -> f32 {
    let current_deg = normalize_degrees(current_deg);
    let snapped = match direction.cmp(&0) {
        std::cmp::Ordering::Greater => (current_deg / ROLL_SNAP_DEG).floor() + 1.0,
        std::cmp::Ordering::Less => (current_deg / ROLL_SNAP_DEG).ceil() - 1.0,
        std::cmp::Ordering::Equal => return current_deg,
    } * ROLL_SNAP_DEG;
    normalize_degrees(snapped)
}

fn apply_roll_snap_steps(current_deg: f32, direction: i8, steps: u32) -> f32 {
    if steps == 0 {
        return normalize_degrees(current_deg);
    }

    let first_step = snap_roll_degrees(current_deg, direction);
    let steps_per_turn = (360.0 / ROLL_SNAP_DEG).round() as u32;
    let additional_steps = (steps - 1) % steps_per_turn;
    normalize_degrees(first_step + direction as f32 * additional_steps as f32 * ROLL_SNAP_DEG)
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct RollSnapRepeatState {
    active_direction: i8,
    held_duration_sec: f32,
    repeat_steps_emitted: u32,
}

fn scheduled_roll_snap_repeats(held_duration_sec: f32) -> u32 {
    const TIME_EPSILON_SEC: f32 = 1.0e-6;

    if held_duration_sec + TIME_EPSILON_SEC < ROLL_SNAP_REPEAT_DELAY_SEC {
        return 0;
    }

    (((held_duration_sec + TIME_EPSILON_SEC - ROLL_SNAP_REPEAT_DELAY_SEC)
        / ROLL_SNAP_REPEAT_INTERVAL_SEC)
        .floor() as u32)
        .saturating_add(1)
}

fn requested_roll_snap_steps(
    state: &mut RollSnapRepeatState,
    input: &Input,
    horizontal_action: HorizontalCameraAction,
    dt: f32,
) -> (i8, u32) {
    if horizontal_action != HorizontalCameraAction::SnapRoll {
        *state = RollSnapRepeatState::default();
        return (0, 0);
    }

    let held_direction = axis(input, KeyCode::ArrowRight, KeyCode::ArrowLeft) as i8;
    if held_direction == 0 {
        *state = RollSnapRepeatState::default();
        return (0, 0);
    }

    if state.active_direction != held_direction {
        *state = RollSnapRepeatState::default();
        state.active_direction = held_direction;
        return (held_direction, 1);
    }

    state.held_duration_sec += dt;
    let scheduled_repeats = scheduled_roll_snap_repeats(state.held_duration_sec);
    let due_repeats = scheduled_repeats.saturating_sub(state.repeat_steps_emitted);
    state.repeat_steps_emitted = scheduled_repeats;

    (held_direction, due_repeats)
}

fn requested_pan_witness_delta(
    input: &Input,
    dt: f32,
    horizontal_action: HorizontalCameraAction,
    vertical_action: VerticalCameraAction,
) -> Vec2 {
    Vec2::new(
        if horizontal_action == HorizontalCameraAction::Pan {
            axis(input, KeyCode::ArrowRight, KeyCode::ArrowLeft)
        } else {
            0.0
        },
        if vertical_action == VerticalCameraAction::Pan {
            axis(input, KeyCode::ArrowUp, KeyCode::ArrowDown)
        } else {
            0.0
        },
    ) * CAMERA_PAN_WITNESS_NDC_RATE_PER_SEC
        * dt
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

/// Rolling FPS measured at the rendered-frame cadence (`Widget::frame`).
struct RenderFps {
    frames: u32,
    elapsed: f32,
    fps: f32,
}

impl RenderFps {
    fn new() -> Self {
        Self {
            frames: 0,
            elapsed: 0.0,
            fps: 0.0,
        }
    }

    fn record(&mut self, dt: f32) {
        self.frames += 1;
        self.elapsed += if dt.is_finite() && dt >= 0.0 { dt } else { 0.0 };
        if self.elapsed >= 1.0 {
            self.fps = self.frames as f32 / self.elapsed;
            self.frames = 0;
            self.elapsed = 0.0;
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
    camera_adjustments: CameraRuntimeAdjustments,
    roll_snap_repeat: RollSnapRepeatState,
    viewport_size: Option<(u32, u32)>,
    camera_controls_enabled: bool,
    settings: AppSettings,
    settings_path: Option<PathBuf>,

    stats: FrameStats,
    render_fps: RenderFps,
    debug_hud_enabled: bool,
    debug_gpu_name: String,
    debug_backend: String,
    debug_requested_msaa: u32,
    debug_effective_msaa: u32,
    debug_smaa_enabled: bool,
    requested_msaa: u32,
    pending_msaa_request: Option<u32>,
    requested_smaa: bool,
    pending_smaa_request: Option<bool>,
    tick_count: u64,
    hovered: bool,
    pending_events: Vec<TickEvent>,
    exit: bool,
    rendered_frames: u32,
}

impl Widget {
    pub fn new(cfg: WidgetConfig) -> Self {
        Self::new_with_camera_settings(cfg, CameraSettings::default())
    }

    pub fn new_with_camera_settings(cfg: WidgetConfig, camera_settings: CameraSettings) -> Self {
        Self::new_internal(cfg, camera_settings, AppSettings::default(), None, 1, false)
    }

    pub fn new_with_settings_path(
        cfg: WidgetConfig,
        settings: AppSettings,
        settings_path: Option<PathBuf>,
    ) -> Self {
        let settings = settings.sanitized();
        let requested_msaa = settings.rendering.msaa.samples().unwrap_or(1);
        let requested_smaa = settings.rendering.smaa_enabled;
        Self::new_internal(
            cfg,
            settings.camera,
            settings,
            settings_path,
            requested_msaa,
            requested_smaa,
        )
    }

    fn new_internal(
        cfg: WidgetConfig,
        camera_settings: CameraSettings,
        settings: AppSettings,
        settings_path: Option<PathBuf>,
        requested_msaa: u32,
        requested_smaa: bool,
    ) -> Self {
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
            camera_settings: camera_settings.sanitized(),
            camera_adjustments: CameraRuntimeAdjustments::default(),
            roll_snap_repeat: RollSnapRepeatState::default(),
            viewport_size: None,
            camera_controls_enabled: false,
            settings,
            settings_path,
            stats: FrameStats::new(),
            render_fps: RenderFps::new(),
            debug_hud_enabled: false,
            debug_gpu_name: "unknown".into(),
            debug_backend: "unknown".into(),
            debug_requested_msaa: requested_msaa,
            debug_effective_msaa: 1,
            debug_smaa_enabled: requested_smaa,
            requested_msaa,
            pending_msaa_request: None,
            requested_smaa,
            pending_smaa_request: None,
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
        self.reapply_camera();
    }

    fn reapply_camera(&mut self) {
        if self.model.is_some() {
            self.apply_camera_settings();
        }
    }

    fn camera_viewport_aspect(&self) -> f32 {
        self.viewport_size
            .and_then(Camera::aspect_for_viewport)
            .or_else(|| Camera::aspect_for_viewport(self.cfg.size))
            .unwrap_or(DEFAULT_VIEWPORT_ASPECT)
    }

    fn apply_camera_settings(&mut self) {
        let Some(model) = self.model.as_ref() else {
            return;
        };
        let aabb = model.aabb;
        let parameters = resolve_camera_parameters_with_aspect(
            aabb,
            self.camera_settings,
            self.camera_adjustments,
            self.camera_viewport_aspect(),
        );
        self.anchor = parameters.baseline_target;
        self.camera.fov_y = parameters.frame.fov_y;
        self.camera.znear = 0.05;
        self.camera.yaw = parameters.yaw_deg.to_radians();
        self.camera.roll = parameters.roll_deg.to_radians();
        self.camera.pitch = parameters.pitch_deg.to_radians();
        self.camera.pos = parameters.position;
        self.sim.look_base = self.camera.pos;
        self.sim.mouse_target = self.camera.pos;
    }

    fn set_camera_adjustments(&mut self, adjustments: CameraRuntimeAdjustments) {
        self.camera_adjustments = adjustments.sanitized();
        self.reapply_camera();
    }

    fn effective_camera_values(&self) -> EffectiveCameraValues {
        self.camera_adjustments.effective(self.camera_settings)
    }

    fn reset_camera_adjustments(&mut self) {
        self.roll_snap_repeat = RollSnapRepeatState::default();
        self.set_camera_adjustments(CameraRuntimeAdjustments::default());
    }

    fn update_viewport(&mut self, size: (u32, u32)) {
        // The renderer derives the projection aspect from this same valid
        // size. Reapplying here keeps the character-owned bounds frame live
        // across a resize while leaving runtime controls untouched.
        if Camera::aspect_for_viewport(size).is_none() || self.viewport_size == Some(size) {
            return;
        }
        self.viewport_size = Some(size);
        self.reapply_camera();
    }

    fn apply_camera_keyboard_controls(&mut self, dt: f32, input: &Input) {
        let repeat_dt = finite_value(dt, 0.0).max(0.0);
        let dt = finite_clamped(dt, 0.0, 0.25, 0.0);
        let horizontal_action = horizontal_camera_action(input);
        let (roll_snap_direction, roll_snap_steps) = requested_roll_snap_steps(
            &mut self.roll_snap_repeat,
            input,
            horizontal_action,
            repeat_dt,
        );
        if dt == 0.0 && roll_snap_steps == 0 {
            return;
        }

        let mut adjustments = self.camera_adjustments;
        let action = vertical_camera_action(input);
        let requested_pitch_delta = if action == VerticalCameraAction::Pitch {
            axis(input, KeyCode::ArrowUp, KeyCode::ArrowDown) * CAMERA_PITCH_RATE_DEG_PER_SEC * dt
        } else {
            0.0
        };
        let requested_pan = requested_pan_witness_delta(input, dt, horizontal_action, action);
        adjustments.fov_delta_deg +=
            axis(input, KeyCode::KeyE, KeyCode::KeyQ) * CAMERA_FOV_RATE_DEG_PER_SEC * dt;
        match horizontal_action {
            HorizontalCameraAction::SnapRoll => {
                adjustments.roll_deg = apply_roll_snap_steps(
                    adjustments.roll_deg,
                    roll_snap_direction,
                    roll_snap_steps,
                );
            }
            HorizontalCameraAction::Yaw => {
                adjustments.yaw_deg += axis(input, KeyCode::ArrowRight, KeyCode::ArrowLeft)
                    * CAMERA_YAW_RATE_DEG_PER_SEC
                    * dt;
            }
            HorizontalCameraAction::Pan => {
                // Applied after all orientation/reframing deltas so the
                // witness is evaluated in the camera produced by this input.
            }
            HorizontalCameraAction::Roll => {
                adjustments.roll_deg += axis(input, KeyCode::ArrowRight, KeyCode::ArrowLeft)
                    * CAMERA_ROLL_RATE_DEG_PER_SEC
                    * dt;
            }
        }
        match action {
            VerticalCameraAction::Zoom => {
                // Up is zoom in (closer), down is zoom out (farther).
                adjustments.distance_scale_delta +=
                    axis(input, KeyCode::ArrowDown, KeyCode::ArrowUp)
                        * CAMERA_DISTANCE_RATE_PER_SEC
                        * dt;
            }
            VerticalCameraAction::Pan => {
                // Applied below with the horizontal pan transition.
            }
            VerticalCameraAction::Pitch => {
                adjustments.pitch_deg += requested_pitch_delta;
            }
        }

        if requested_pan != Vec2::ZERO {
            if let Some(aabb) = self.model.as_ref().map(|model| model.aabb) {
                adjustments.pan_ndc = admit_pan_input(
                    aabb,
                    self.camera_settings,
                    adjustments,
                    self.camera_viewport_aspect(),
                    requested_pan,
                );
            }
        }

        if adjustments != self.camera_adjustments {
            self.set_camera_adjustments(adjustments);
        }
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
        let adapter_info = gpu.adapter.get_info();
        self.debug_gpu_name = adapter_info.name;
        self.debug_backend = format!("{:?}", adapter_info.backend);
        self.requested_msaa = renderer.requested_sample_count();
        self.debug_requested_msaa = self.requested_msaa;
        self.debug_effective_msaa = renderer.effective_sample_count();
        renderer.set_smaa_enabled(gpu, self.settings.rendering.smaa_enabled);
        self.requested_smaa = self.settings.rendering.smaa_enabled;
        self.debug_smaa_enabled = renderer.smaa_enabled();

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
        self.springs = Some(SpringSolver::new(
            &vrm.springs,
            &model.skeleton,
            &self.locals,
        ));

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

    fn frame(&mut self, dt: f32, input: &Input) {
        self.render_fps.record(dt);
        if input.key_pressed(KeyCode::F3) {
            self.debug_hud_enabled = !self.debug_hud_enabled;
        }
        if input.key_pressed(KeyCode::F4) {
            self.requested_msaa = next_msaa_sample_count(self.requested_msaa);
            self.pending_msaa_request = Some(self.requested_msaa);
        }
        if input.key_pressed(KeyCode::F5) {
            self.requested_smaa = !self.requested_smaa;
            self.pending_smaa_request = Some(self.requested_smaa);
        }
        // Temporary F8 validation controls. They intentionally remain
        // widget-local and are never written to AppSettings.
        if input.key_pressed(KeyCode::F8) {
            self.camera_controls_enabled = !self.camera_controls_enabled;
            if !self.camera_controls_enabled {
                self.roll_snap_repeat = RollSnapRepeatState::default();
            }
        }
        if self.camera_controls_enabled && input.key_pressed(KeyCode::KeyR) {
            self.reset_camera_adjustments();
        } else if self.camera_controls_enabled {
            self.apply_camera_keyboard_controls(dt, input);
        }

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

    fn prepare_render(&mut self, gpu: &Gpu, renderer: &mut Renderer) {
        let requested_msaa = self.pending_msaa_request.take();
        let requested_smaa = self.pending_smaa_request.take();
        if requested_msaa.is_none() && requested_smaa.is_none() {
            return;
        }

        let mut accepted_msaa = None;
        if let Some(requested) = requested_msaa {
            renderer.set_requested_sample_count(gpu, requested);
            if renderer.requested_sample_count() == requested {
                accepted_msaa = Some(requested);
            } else {
                log::warn!(
                    "renderer rejected requested MSAA {}; keeping persisted preference",
                    format_msaa_count(requested)
                );
            }
        }
        let mut accepted_smaa = None;
        if let Some(enabled) = requested_smaa {
            renderer.set_smaa_enabled(gpu, enabled);
            if renderer.smaa_enabled() == enabled {
                accepted_smaa = Some(enabled);
            } else {
                log::warn!(
                    "renderer rejected requested SMAA {}; keeping persisted preference",
                    if enabled { "on" } else { "off" }
                );
            }
        }

        self.requested_msaa = renderer.requested_sample_count();
        self.debug_requested_msaa = self.requested_msaa;
        self.debug_effective_msaa = renderer.effective_sample_count();
        self.requested_smaa = renderer.smaa_enabled();
        self.debug_smaa_enabled = self.requested_smaa;
        self.commit_accepted_aa_preferences(accepted_msaa, accepted_smaa);
        log::info!(
            "AA: requested {}, effective MSAA {}, SMAA {}",
            format_msaa_count(self.debug_requested_msaa),
            format_msaa_count(self.debug_effective_msaa),
            if self.debug_smaa_enabled { "on" } else { "off" }
        );
    }

    fn compose(&mut self, _alpha: f32, time: f32, size: (u32, u32)) -> (&Scene, &Camera, &Hud) {
        self.scene.time = time;
        self.update_viewport(size);
        self.rendered_frames += 1;
        if let Some(n) = self.cfg.frames
            && self.rendered_frames >= n
        {
            self.exit = true;
        }
        self.hud.clear();
        if self.debug_hud_enabled {
            self.compose_debug_hud(size);
        }
        (&self.scene, &self.camera, &self.hud)
    }

    fn wants_exit(&self) -> bool {
        self.exit
    }
}

impl Widget {
    fn commit_accepted_aa_preferences(
        &mut self,
        requested_msaa: Option<u32>,
        smaa_enabled: Option<bool>,
    ) {
        let mut changed = false;
        if let Some(requested) = requested_msaa {
            if let Some(preference) = AntiAliasingPreference::from_samples(requested)
                && self.settings.rendering.msaa != preference
            {
                self.settings.rendering.msaa = preference;
                changed = true;
            }
        }
        if let Some(enabled) = smaa_enabled
            && self.settings.rendering.smaa_enabled != enabled
        {
            self.settings.rendering.smaa_enabled = enabled;
            changed = true;
        }

        if changed
            && let Some(path) = self.settings_path.as_deref()
            && let Err(error) = self.settings.save_to_path(path)
        {
            log::warn!(
                "unable to persist AA settings to {}: {error:#}",
                path.display()
            );
        }
    }
}

fn next_msaa_sample_count(requested: u32) -> u32 {
    match requested {
        1 => 2,
        2 => 4,
        4 => 8,
        8 => 1,
        _ => 2,
    }
}

impl Widget {
    fn compose_debug_hud(&mut self, size: (u32, u32)) {
        const X: f32 = 14.0;
        const TITLE_Y: f32 = 14.0;
        const BODY_Y: f32 = 38.0;
        const LINE_HEIGHT: f32 = 10.0;
        const PANEL_TOP: f32 = 8.0;
        const PANEL_PADDING: f32 = 8.0;

        let title = "Pocket3D HUD";
        let fps = format!("FPS: {:.1}", self.render_fps.fps);
        let tick_cpu = format!("Tick CPU: {:.2} ms", self.stats.frame_ms);
        let gpu = format!("GPU: {}", self.debug_gpu_name);
        let backend = format!("Backend: {}", self.debug_backend);
        let msaa = format_msaa_hud_line(self.debug_requested_msaa, self.debug_effective_msaa);
        let smaa = format_smaa_hud_line(self.debug_smaa_enabled);
        let camera = self.effective_camera_values();
        let camera_fov = format!("Cam FOV: {:.1} deg", camera.settings.fov_deg);
        let camera_distance = format!(
            "Cam distance: {:.3}x height",
            camera.settings.distance_scale
        );
        let camera_headroom = format!("Cam headroom: {:.3}", camera.settings.headroom);
        let camera_framing_x = format!("Cam pan X: {:.3} NDC", camera.pan_ndc.x);
        let camera_framing_y = format!("Cam pan Y: {:.3} NDC", camera.pan_ndc.y);
        let camera_yaw = format!("Cam yaw: {:.1} deg", camera.yaw_deg);
        let camera_roll = format!("Cam roll: {:.1} deg", camera.roll_deg);
        let camera_pitch = format!("Cam pitch: {:.1} deg", camera.pitch_deg);
        let camera_controls = if self.camera_controls_enabled {
            "Camera controls: on"
        } else {
            "Camera controls: off"
        };
        let frame = format!("Frame: {}x{}", size.0, size.1);
        let body = [
            fps.as_str(),
            tick_cpu.as_str(),
            gpu.as_str(),
            backend.as_str(),
            msaa.as_str(),
            smaa.as_str(),
            camera_fov.as_str(),
            camera_distance.as_str(),
            camera_headroom.as_str(),
            camera_framing_x.as_str(),
            camera_framing_y.as_str(),
            camera_yaw.as_str(),
            camera_roll.as_str(),
            camera_pitch.as_str(),
            camera_controls,
            CAMERA_CONTROL_HELP[0],
            CAMERA_CONTROL_HELP[1],
            CAMERA_CONTROL_HELP[2],
            CAMERA_CONTROL_HELP[3],
            CAMERA_CONTROL_HELP[4],
            CAMERA_CONTROL_HELP[5],
            CAMERA_CONTROL_HELP[6],
            CAMERA_CONTROL_HELP[7],
            CAMERA_CONTROL_HELP[8],
            CAMERA_CONTROL_HELP[9],
            frame.as_str(),
        ];

        let body_width = body
            .iter()
            .map(|line| Hud::text_width(line, 1.0))
            .fold(0.0, f32::max);
        let panel_width = Hud::text_width(title, 2.0).max(body_width) + PANEL_PADDING * 2.0;
        let panel_bottom = BODY_Y + (body.len() - 1) as f32 * LINE_HEIGHT + 8.0;
        let panel_height = panel_bottom - PANEL_TOP + PANEL_PADDING;

        self.hud.rect(
            X - PANEL_PADDING,
            PANEL_TOP,
            panel_width,
            panel_height,
            [0.01, 0.02, 0.03, 0.78],
        );
        self.hud.rect(
            X - 1.0,
            TITLE_Y + 20.0,
            panel_width - PANEL_PADDING,
            1.0,
            [0.20, 0.72, 1.0, 0.95],
        );
        self.hud
            .text(X, TITLE_Y, 2.0, [0.86, 0.96, 1.0, 1.0], title);
        for (index, line) in body.iter().enumerate() {
            self.hud.text(
                X,
                BODY_Y + index as f32 * LINE_HEIGHT,
                1.0,
                [0.92, 0.94, 0.96, 1.0],
                line,
            );
        }
    }
}

fn format_msaa_count(sample_count: u32) -> String {
    match sample_count {
        1 => "off".into(),
        sample_count => format!("{sample_count}x"),
    }
}

fn format_msaa_hud_line(requested: u32, effective: u32) -> String {
    format!(
        "MSAA: requested {} / effective {}",
        format_msaa_count(requested),
        format_msaa_count(effective)
    )
}

fn format_smaa_hud_line(enabled: bool) -> String {
    (if enabled { "SMAA: on" } else { "SMAA: off" }).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{AntiAliasingPreference, AppSettings, RenderSettings};
    use tempfile::tempdir;

    fn test_widget() -> Widget {
        Widget::new(test_config())
    }

    fn test_config() -> WidgetConfig {
        WidgetConfig {
            model_path: PathBuf::new(),
            vrma_path: PathBuf::new(),
            bundle_path: PathBuf::new(),
            size: (450, 600),
            frames: None,
        }
    }

    fn approx_eq(actual: f32, expected: f32) {
        assert!((actual - expected).abs() < 1.0e-5, "{actual} != {expected}");
    }

    fn camera_adjustments_after_keys(keys: &[KeyCode]) -> CameraRuntimeAdjustments {
        let mut widget = test_widget();
        let mut input = Input::default();
        for &key in keys {
            input.inject_key(key, true);
        }
        widget.apply_camera_keyboard_controls(1.0 / 60.0, &input);
        widget.camera_adjustments
    }

    fn camera_adjustments_after_repeated_keys(
        keys: &[KeyCode],
        steps: usize,
        dt: f32,
    ) -> CameraRuntimeAdjustments {
        let mut widget = test_widget();
        let mut input = Input::default();
        for &key in keys {
            input.inject_key(key, true);
        }
        for _ in 0..steps {
            widget.apply_camera_keyboard_controls(dt, &input);
        }
        widget.camera_adjustments
    }

    fn pan_witness_delta_after_keys(keys: &[KeyCode], dt: f32) -> Vec2 {
        let mut input = Input::default();
        for &key in keys {
            input.inject_key(key, true);
        }
        requested_pan_witness_delta(
            &input,
            dt,
            horizontal_camera_action(&input),
            vertical_camera_action(&input),
        )
    }

    fn roll_snap_test_input(arrow: KeyCode) -> Input {
        let mut input = Input::default();
        input.inject_key(KeyCode::ControlLeft, true);
        input.inject_key(KeyCode::ShiftLeft, true);
        input.inject_key(arrow, true);
        input
    }

    #[test]
    fn invalid_live_settings_are_sanitized() {
        let settings = CameraSettings {
            fov_deg: f32::NAN,
            distance_scale: -1.0,
            headroom: 1.0,
        };
        assert_eq!(
            settings.sanitized(),
            CameraSettings {
                fov_deg: 40.0,
                distance_scale: 0.1,
                headroom: 0.49,
            }
        );

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
        let mut widget = test_widget();
        widget.set_camera_settings(CameraSettings {
            fov_deg: 35.0,
            distance_scale: 0.75,
            headroom: 0.08,
        });

        assert_eq!(widget.camera_settings.fov_deg, 35.0);
        assert_eq!(widget.camera_settings.distance_scale, 0.75);
        assert_eq!(widget.camera_settings.headroom, 0.08);
    }

    #[test]
    fn f8_toggles_live_camera_controls_without_touching_base_settings() {
        let mut widget = test_widget();
        let mut input = Input::default();

        input.inject_key(KeyCode::F8, true);
        widget.frame(0.0, &input);
        assert!(widget.camera_controls_enabled);
        assert_eq!(widget.camera_settings, CameraSettings::default());

        input.inject_key(KeyCode::F8, false);
        input.end_frame();
        input.inject_key(KeyCode::KeyE, true);
        widget.frame(1.0 / 60.0, &input);
        assert!(widget.effective_camera_values().settings.fov_deg > 40.0);
        assert_eq!(widget.camera_settings, CameraSettings::default());
    }

    #[test]
    fn f8_plain_horizontal_arrows_select_pan_x_only() {
        let dt = 1.0 / 60.0;
        let mut input = Input::default();
        input.inject_key(KeyCode::ArrowLeft, true);
        assert_eq!(
            horizontal_camera_action(&input),
            HorizontalCameraAction::Pan
        );
        let left_delta = pan_witness_delta_after_keys(&[KeyCode::ArrowLeft], dt);
        assert!(left_delta.x < 0.0);
        assert_eq!(left_delta.y, 0.0);
        assert_eq!(
            camera_adjustments_after_keys(&[KeyCode::ArrowLeft]),
            CameraRuntimeAdjustments::default()
        );

        let mut input = Input::default();
        input.inject_key(KeyCode::ArrowRight, true);
        assert_eq!(
            horizontal_camera_action(&input),
            HorizontalCameraAction::Pan
        );
        let right_delta = pan_witness_delta_after_keys(&[KeyCode::ArrowRight], dt);
        assert!(right_delta.x > 0.0);
        assert_eq!(right_delta.y, 0.0);
        assert_eq!(
            camera_adjustments_after_keys(&[KeyCode::ArrowRight]),
            CameraRuntimeAdjustments::default()
        );
    }

    #[test]
    fn f8_plain_vertical_arrows_select_pan_y_only() {
        let dt = 1.0 / 60.0;
        let mut input = Input::default();
        input.inject_key(KeyCode::ArrowUp, true);
        assert_eq!(vertical_camera_action(&input), VerticalCameraAction::Pan);
        let up_delta = pan_witness_delta_after_keys(&[KeyCode::ArrowUp], dt);
        assert_eq!(up_delta.x, 0.0);
        assert!(up_delta.y > 0.0);
        assert_eq!(
            camera_adjustments_after_keys(&[KeyCode::ArrowUp]),
            CameraRuntimeAdjustments::default()
        );

        let mut input = Input::default();
        input.inject_key(KeyCode::ArrowDown, true);
        assert_eq!(vertical_camera_action(&input), VerticalCameraAction::Pan);
        let down_delta = pan_witness_delta_after_keys(&[KeyCode::ArrowDown], dt);
        assert_eq!(down_delta.x, 0.0);
        assert!(down_delta.y < 0.0);
        assert_eq!(
            camera_adjustments_after_keys(&[KeyCode::ArrowDown]),
            CameraRuntimeAdjustments::default()
        );
    }

    #[test]
    fn f8_shift_arrows_select_yaw_or_zoom_only() {
        let mut input = Input::default();
        input.inject_key(KeyCode::ShiftLeft, true);
        input.inject_key(KeyCode::ArrowLeft, true);
        assert_eq!(
            horizontal_camera_action(&input),
            HorizontalCameraAction::Yaw
        );
        let shift_left = camera_adjustments_after_keys(&[KeyCode::ShiftLeft, KeyCode::ArrowLeft]);
        assert!(shift_left.yaw_deg < 0.0);
        assert_eq!(shift_left.roll_deg, 0.0);
        assert_eq!(shift_left.pitch_deg, 0.0);
        assert_eq!(shift_left.distance_scale_delta, 0.0);
        assert_eq!(shift_left.pan_ndc, Vec2::ZERO);

        let mut input = Input::default();
        input.inject_key(KeyCode::ShiftLeft, true);
        input.inject_key(KeyCode::ArrowUp, true);
        assert_eq!(vertical_camera_action(&input), VerticalCameraAction::Zoom);
        let shift_up = camera_adjustments_after_keys(&[KeyCode::ShiftLeft, KeyCode::ArrowUp]);
        assert!(shift_up.distance_scale_delta < 0.0);
        assert_eq!(shift_up.yaw_deg, 0.0);
        assert_eq!(shift_up.roll_deg, 0.0);
        assert_eq!(shift_up.pitch_deg, 0.0);
        assert_eq!(shift_up.pan_ndc, Vec2::ZERO);

        let shift_down = camera_adjustments_after_keys(&[KeyCode::ShiftLeft, KeyCode::ArrowDown]);
        assert!(shift_down.distance_scale_delta > 0.0);
        assert_eq!(shift_down.yaw_deg, 0.0);
        assert_eq!(shift_down.pan_ndc, Vec2::ZERO);
    }

    #[test]
    fn f8_ctrl_arrows_select_roll_or_pitch_only() {
        let mut input = Input::default();
        input.inject_key(KeyCode::ControlLeft, true);
        input.inject_key(KeyCode::ArrowLeft, true);
        assert_eq!(
            horizontal_camera_action(&input),
            HorizontalCameraAction::Roll
        );
        let ctrl_left = camera_adjustments_after_keys(&[KeyCode::ControlLeft, KeyCode::ArrowLeft]);
        assert!(ctrl_left.roll_deg < 0.0);
        assert_eq!(ctrl_left.yaw_deg, 0.0);
        assert_eq!(ctrl_left.pitch_deg, 0.0);
        assert_eq!(ctrl_left.distance_scale_delta, 0.0);
        assert_eq!(ctrl_left.pan_ndc, Vec2::ZERO);

        let mut input = Input::default();
        input.inject_key(KeyCode::ControlLeft, true);
        input.inject_key(KeyCode::ArrowUp, true);
        assert_eq!(vertical_camera_action(&input), VerticalCameraAction::Pitch);
        let ctrl_up = camera_adjustments_after_keys(&[KeyCode::ControlLeft, KeyCode::ArrowUp]);
        assert!(ctrl_up.pitch_deg > 0.0);
        assert_eq!(ctrl_up.yaw_deg, 0.0);
        assert_eq!(ctrl_up.roll_deg, 0.0);
        assert_eq!(ctrl_up.distance_scale_delta, 0.0);
        assert_eq!(ctrl_up.pan_ndc, Vec2::ZERO);

        let ctrl_down = camera_adjustments_after_keys(&[KeyCode::ControlLeft, KeyCode::ArrowDown]);
        assert!(ctrl_down.pitch_deg < 0.0);
        assert_eq!(ctrl_down.yaw_deg, 0.0);
        assert_eq!(ctrl_down.distance_scale_delta, 0.0);
        assert_eq!(ctrl_down.pan_ndc, Vec2::ZERO);
    }

    #[test]
    fn f8_q_and_e_adjust_fov_without_other_camera_actions() {
        let q = camera_adjustments_after_keys(&[KeyCode::KeyQ]);
        assert!(q.fov_delta_deg < 0.0);
        assert_eq!(q.distance_scale_delta, 0.0);
        assert_eq!(q.pan_ndc, Vec2::ZERO);
        assert_eq!(q.yaw_deg, 0.0);
        assert_eq!(q.roll_deg, 0.0);
        assert_eq!(q.pitch_deg, 0.0);

        let e = camera_adjustments_after_keys(&[KeyCode::KeyE]);
        assert!(e.fov_delta_deg > 0.0);
        assert_eq!(e.distance_scale_delta, 0.0);
        assert_eq!(e.pan_ndc, Vec2::ZERO);
        assert_eq!(e.yaw_deg, 0.0);
        assert_eq!(e.roll_deg, 0.0);
        assert_eq!(e.pitch_deg, 0.0);

        assert_eq!(
            CAMERA_CONTROL_HELP,
            [
                "Up / Down              Pan Y up / down",
                "Left / Right           Pan X left / right",
                "Shift + Up / Down      Zoom in / out",
                "Shift + Left / Right   Yaw left / right",
                "Ctrl + Up / Down       Pitch up / down",
                "Ctrl + Left/Right        Roll",
                "Ctrl + Shift + Left/Right Snap roll 15°",
                "Q / E                  Decrease / increase FOV",
                "R                      Reset runtime camera adjustments",
                "F8                     Toggle camera controls",
            ]
        );
    }

    #[test]
    fn f8_ctrl_takes_precedence_over_shift_for_vertical_actions() {
        let mut input = Input::default();

        input.inject_key(KeyCode::ArrowUp, true);
        assert_eq!(vertical_camera_action(&input), VerticalCameraAction::Pan);

        input.inject_key(KeyCode::ShiftLeft, true);
        assert_eq!(vertical_camera_action(&input), VerticalCameraAction::Zoom);

        input.inject_key(KeyCode::ControlLeft, true);
        assert_eq!(vertical_camera_action(&input), VerticalCameraAction::Pitch);

        let shift_and_ctrl_up = camera_adjustments_after_keys(&[
            KeyCode::ShiftLeft,
            KeyCode::ControlLeft,
            KeyCode::ArrowUp,
        ]);
        assert_eq!(shift_and_ctrl_up.distance_scale_delta, 0.0);
        assert_eq!(shift_and_ctrl_up.pan_ndc.y, 0.0);
        assert!(shift_and_ctrl_up.pitch_deg > 0.0);
    }

    #[test]
    fn f8_horizontal_modifier_precedence_selects_snap_roll_first() {
        let mut input = Input::default();
        input.inject_key(KeyCode::ArrowLeft, true);
        assert_eq!(
            horizontal_camera_action(&input),
            HorizontalCameraAction::Pan
        );

        input.inject_key(KeyCode::ShiftLeft, true);
        assert_eq!(
            horizontal_camera_action(&input),
            HorizontalCameraAction::Yaw
        );

        input.inject_key(KeyCode::ControlLeft, true);
        assert_eq!(
            horizontal_camera_action(&input),
            HorizontalCameraAction::SnapRoll
        );
        assert_eq!(
            requested_pan_witness_delta(
                &input,
                1.0 / 60.0,
                horizontal_camera_action(&input),
                vertical_camera_action(&input),
            ),
            Vec2::ZERO
        );

        let shift_and_ctrl_left = camera_adjustments_after_keys(&[
            KeyCode::ShiftLeft,
            KeyCode::ControlLeft,
            KeyCode::ArrowLeft,
        ]);
        assert_eq!(shift_and_ctrl_left.roll_deg, -ROLL_SNAP_DEG);
        assert_eq!(shift_and_ctrl_left.pan_ndc.x, 0.0);
        assert_eq!(shift_and_ctrl_left.yaw_deg, 0.0);

        let ctrl_left = camera_adjustments_after_keys(&[KeyCode::ControlLeft, KeyCode::ArrowLeft]);
        assert!(ctrl_left.roll_deg < 0.0);
        assert_eq!(ctrl_left.yaw_deg, 0.0);
        assert_eq!(ctrl_left.pan_ndc.x, 0.0);
    }

    #[test]
    fn f8_roll_snap_selects_next_grid_point_in_requested_direction() {
        for (current, expected_right, expected_left) in [
            (7.0, 15.0, 0.0),
            (30.0, 45.0, 15.0),
            (-7.0, 0.0, -15.0),
            (-30.0, -15.0, -45.0),
            (0.0, 15.0, -15.0),
        ] {
            approx_eq(snap_roll_degrees(current, 1), expected_right);
            approx_eq(snap_roll_degrees(current, -1), expected_left);
        }
    }

    #[test]
    fn f8_roll_snap_wraps_across_180_degrees() {
        approx_eq(snap_roll_degrees(179.0, 1), -180.0);
        approx_eq(snap_roll_degrees(-179.0, -1), -180.0);
        approx_eq(snap_roll_degrees(-180.0, 1), -165.0);
        approx_eq(snap_roll_degrees(-180.0, -1), 165.0);
    }

    #[test]
    fn f8_roll_snap_is_immediate_and_waits_for_repeat_delay() {
        let mut widget = test_widget();
        widget.set_camera_adjustments(CameraRuntimeAdjustments {
            roll_deg: 7.0,
            ..CameraRuntimeAdjustments::default()
        });
        let mut input = roll_snap_test_input(KeyCode::ArrowRight);

        widget.apply_camera_keyboard_controls(1.0 / 60.0, &input);
        approx_eq(widget.camera_adjustments.roll_deg, 15.0);
        input.end_frame();
        widget.apply_camera_keyboard_controls(ROLL_SNAP_REPEAT_DELAY_SEC - 0.001, &input);
        approx_eq(widget.camera_adjustments.roll_deg, 15.0);
    }

    #[test]
    fn f8_ctrl_right_then_shift_immediately_activates_one_snap() {
        let mut widget = test_widget();
        widget.set_camera_adjustments(CameraRuntimeAdjustments {
            roll_deg: 7.0,
            ..CameraRuntimeAdjustments::default()
        });
        let mut input = Input::default();
        input.inject_key(KeyCode::ControlLeft, true);
        input.inject_key(KeyCode::ArrowRight, true);
        widget.apply_camera_keyboard_controls(0.1, &input);
        approx_eq(widget.camera_adjustments.roll_deg, 16.0);

        input.end_frame();
        input.inject_key(KeyCode::ShiftLeft, true);
        widget.apply_camera_keyboard_controls(ROLL_SNAP_REPEAT_DELAY_SEC * 2.0, &input);
        approx_eq(widget.camera_adjustments.roll_deg, 30.0);
        assert_eq!(
            widget.roll_snap_repeat,
            RollSnapRepeatState {
                active_direction: 1,
                held_duration_sec: 0.0,
                repeat_steps_emitted: 0,
            }
        );

        input.end_frame();
        widget.apply_camera_keyboard_controls(ROLL_SNAP_REPEAT_DELAY_SEC - 0.001, &input);
        approx_eq(widget.camera_adjustments.roll_deg, 30.0);
        widget.apply_camera_keyboard_controls(0.001, &input);
        approx_eq(widget.camera_adjustments.roll_deg, 45.0);
    }

    #[test]
    fn f8_ctrl_left_then_shift_immediately_activates_one_snap() {
        let mut widget = test_widget();
        widget.set_camera_adjustments(CameraRuntimeAdjustments {
            roll_deg: 7.0,
            ..CameraRuntimeAdjustments::default()
        });
        let mut input = Input::default();
        input.inject_key(KeyCode::ControlLeft, true);
        input.inject_key(KeyCode::ArrowLeft, true);
        widget.apply_camera_keyboard_controls(0.1, &input);
        approx_eq(widget.camera_adjustments.roll_deg, -2.0);

        input.end_frame();
        input.inject_key(KeyCode::ShiftLeft, true);
        widget.apply_camera_keyboard_controls(ROLL_SNAP_REPEAT_DELAY_SEC * 2.0, &input);
        approx_eq(widget.camera_adjustments.roll_deg, -15.0);
        assert_eq!(widget.roll_snap_repeat.active_direction, -1);
        assert_eq!(widget.roll_snap_repeat.repeat_steps_emitted, 0);
    }

    #[test]
    fn f8_shift_right_then_ctrl_immediately_activates_one_snap() {
        let mut widget = test_widget();
        widget.set_camera_adjustments(CameraRuntimeAdjustments {
            roll_deg: 7.0,
            ..CameraRuntimeAdjustments::default()
        });
        let mut input = Input::default();
        input.inject_key(KeyCode::ShiftLeft, true);
        input.inject_key(KeyCode::ArrowRight, true);
        widget.apply_camera_keyboard_controls(0.1, &input);
        approx_eq(widget.camera_adjustments.roll_deg, 7.0);
        approx_eq(widget.camera_adjustments.yaw_deg, 9.0);

        input.end_frame();
        input.inject_key(KeyCode::ControlLeft, true);
        widget.apply_camera_keyboard_controls(ROLL_SNAP_REPEAT_DELAY_SEC * 2.0, &input);
        approx_eq(widget.camera_adjustments.roll_deg, 15.0);
        approx_eq(widget.camera_adjustments.yaw_deg, 9.0);
        assert_eq!(widget.roll_snap_repeat.active_direction, 1);
        assert_eq!(widget.roll_snap_repeat.repeat_steps_emitted, 0);
    }

    #[test]
    fn f8_ctrl_shift_then_right_immediately_activates_one_snap() {
        let mut widget = test_widget();
        widget.set_camera_adjustments(CameraRuntimeAdjustments {
            roll_deg: 7.0,
            ..CameraRuntimeAdjustments::default()
        });
        let mut input = Input::default();
        input.inject_key(KeyCode::ControlLeft, true);
        input.inject_key(KeyCode::ShiftLeft, true);
        widget.apply_camera_keyboard_controls(0.1, &input);
        assert_eq!(widget.roll_snap_repeat, RollSnapRepeatState::default());

        input.end_frame();
        input.inject_key(KeyCode::ArrowRight, true);
        widget.apply_camera_keyboard_controls(ROLL_SNAP_REPEAT_DELAY_SEC * 2.0, &input);
        approx_eq(widget.camera_adjustments.roll_deg, 15.0);
        assert_eq!(widget.roll_snap_repeat.active_direction, 1);
        assert_eq!(widget.roll_snap_repeat.repeat_steps_emitted, 0);
    }

    #[test]
    fn f8_releasing_shift_resumes_continuous_ctrl_roll_immediately() {
        let mut widget = test_widget();
        widget.set_camera_adjustments(CameraRuntimeAdjustments {
            roll_deg: 7.0,
            ..CameraRuntimeAdjustments::default()
        });
        let mut input = Input::default();
        input.inject_key(KeyCode::ControlLeft, true);
        input.inject_key(KeyCode::ArrowRight, true);
        widget.apply_camera_keyboard_controls(0.1, &input);
        approx_eq(widget.camera_adjustments.roll_deg, 16.0);

        input.end_frame();
        input.inject_key(KeyCode::ShiftLeft, true);
        widget.apply_camera_keyboard_controls(0.0, &input);
        approx_eq(widget.camera_adjustments.roll_deg, 30.0);

        input.end_frame();
        input.inject_key(KeyCode::ShiftLeft, false);
        widget.apply_camera_keyboard_controls(0.1, &input);
        approx_eq(widget.camera_adjustments.roll_deg, 39.0);
        assert_eq!(widget.roll_snap_repeat, RollSnapRepeatState::default());
    }

    #[test]
    fn f8_roll_snap_repeats_after_delay_at_the_configured_interval() {
        let mut widget = test_widget();
        widget.set_camera_adjustments(CameraRuntimeAdjustments {
            roll_deg: 7.0,
            ..CameraRuntimeAdjustments::default()
        });
        let mut input = roll_snap_test_input(KeyCode::ArrowRight);

        widget.apply_camera_keyboard_controls(0.0, &input);
        input.end_frame();
        widget.apply_camera_keyboard_controls(ROLL_SNAP_REPEAT_DELAY_SEC, &input);
        approx_eq(widget.camera_adjustments.roll_deg, 30.0);

        widget.apply_camera_keyboard_controls(ROLL_SNAP_REPEAT_INTERVAL_SEC - 0.001, &input);
        approx_eq(widget.camera_adjustments.roll_deg, 30.0);
        widget.apply_camera_keyboard_controls(0.001, &input);
        approx_eq(widget.camera_adjustments.roll_deg, 45.0);
        widget.apply_camera_keyboard_controls(ROLL_SNAP_REPEAT_INTERVAL_SEC * 2.0, &input);
        approx_eq(widget.camera_adjustments.roll_deg, 75.0);
    }

    #[test]
    fn f8_roll_snap_quick_press_and_release_stays_one_shot() {
        let mut widget = test_widget();
        widget.set_camera_adjustments(CameraRuntimeAdjustments {
            roll_deg: 7.0,
            ..CameraRuntimeAdjustments::default()
        });
        let mut input = roll_snap_test_input(KeyCode::ArrowRight);

        widget.apply_camera_keyboard_controls(0.0, &input);
        input.end_frame();
        widget.apply_camera_keyboard_controls(ROLL_SNAP_REPEAT_DELAY_SEC * 0.5, &input);
        input.inject_key(KeyCode::ArrowRight, false);
        widget.apply_camera_keyboard_controls(0.0, &input);
        widget.apply_camera_keyboard_controls(1.0, &input);

        approx_eq(widget.camera_adjustments.roll_deg, 15.0);
        assert_eq!(widget.roll_snap_repeat, RollSnapRepeatState::default());
    }

    #[test]
    fn f8_roll_snap_release_stops_repeat_and_repress_snaps_immediately() {
        let mut widget = test_widget();
        widget.set_camera_adjustments(CameraRuntimeAdjustments {
            roll_deg: 7.0,
            ..CameraRuntimeAdjustments::default()
        });
        let mut input = roll_snap_test_input(KeyCode::ArrowRight);

        widget.apply_camera_keyboard_controls(0.0, &input);
        input.end_frame();
        widget.apply_camera_keyboard_controls(ROLL_SNAP_REPEAT_DELAY_SEC, &input);
        approx_eq(widget.camera_adjustments.roll_deg, 30.0);

        input.inject_key(KeyCode::ArrowRight, false);
        widget.apply_camera_keyboard_controls(1.0, &input);
        approx_eq(widget.camera_adjustments.roll_deg, 30.0);
        assert_eq!(widget.roll_snap_repeat, RollSnapRepeatState::default());

        input.end_frame();
        input.inject_key(KeyCode::ArrowRight, true);
        widget.apply_camera_keyboard_controls(0.0, &input);
        approx_eq(widget.camera_adjustments.roll_deg, 45.0);
    }

    #[test]
    fn f8_roll_snap_releasing_any_chord_member_cancels_repeat() {
        for released_key in [
            KeyCode::ArrowRight,
            KeyCode::ControlLeft,
            KeyCode::ShiftLeft,
        ] {
            let mut widget = test_widget();
            widget.set_camera_adjustments(CameraRuntimeAdjustments {
                roll_deg: 7.0,
                ..CameraRuntimeAdjustments::default()
            });
            let mut input = roll_snap_test_input(KeyCode::ArrowRight);
            widget.apply_camera_keyboard_controls(0.0, &input);
            input.end_frame();
            widget.apply_camera_keyboard_controls(ROLL_SNAP_REPEAT_DELAY_SEC, &input);
            approx_eq(widget.camera_adjustments.roll_deg, 30.0);

            input.inject_key(released_key, false);
            widget.apply_camera_keyboard_controls(ROLL_SNAP_REPEAT_INTERVAL_SEC * 5.0, &input);
            assert_eq!(widget.roll_snap_repeat, RollSnapRepeatState::default());
            match released_key {
                KeyCode::ArrowRight => {
                    approx_eq(widget.camera_adjustments.roll_deg, 30.0);
                    approx_eq(widget.camera_adjustments.yaw_deg, 0.0);
                }
                KeyCode::ControlLeft => {
                    approx_eq(widget.camera_adjustments.roll_deg, 30.0);
                    assert!(widget.camera_adjustments.yaw_deg > 0.0);
                }
                KeyCode::ShiftLeft => {
                    approx_eq(
                        widget.camera_adjustments.roll_deg,
                        30.0 + CAMERA_ROLL_RATE_DEG_PER_SEC * 0.25,
                    );
                    approx_eq(widget.camera_adjustments.yaw_deg, 0.0);
                }
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn f8_roll_snap_switches_direction_with_fresh_repeat_timing() {
        let mut widget = test_widget();
        widget.set_camera_adjustments(CameraRuntimeAdjustments {
            roll_deg: 7.0,
            ..CameraRuntimeAdjustments::default()
        });
        let mut input = roll_snap_test_input(KeyCode::ArrowLeft);

        widget.apply_camera_keyboard_controls(0.0, &input);
        input.end_frame();
        widget.apply_camera_keyboard_controls(ROLL_SNAP_REPEAT_DELAY_SEC, &input);
        approx_eq(widget.camera_adjustments.roll_deg, -15.0);

        input.inject_key(KeyCode::ArrowLeft, false);
        input.inject_key(KeyCode::ArrowRight, true);
        widget.apply_camera_keyboard_controls(0.0, &input);
        approx_eq(widget.camera_adjustments.roll_deg, 0.0);
        input.end_frame();
        widget.apply_camera_keyboard_controls(ROLL_SNAP_REPEAT_DELAY_SEC - 0.001, &input);
        approx_eq(widget.camera_adjustments.roll_deg, 0.0);
    }

    #[test]
    fn f8_roll_snap_wraps_while_repeating() {
        let mut widget = test_widget();
        widget.set_camera_adjustments(CameraRuntimeAdjustments {
            roll_deg: 179.0,
            ..CameraRuntimeAdjustments::default()
        });
        let mut input = roll_snap_test_input(KeyCode::ArrowRight);

        widget.apply_camera_keyboard_controls(0.0, &input);
        approx_eq(widget.camera_adjustments.roll_deg, -180.0);
        input.end_frame();
        widget.apply_camera_keyboard_controls(ROLL_SNAP_REPEAT_DELAY_SEC, &input);
        approx_eq(widget.camera_adjustments.roll_deg, -165.0);
        widget.apply_camera_keyboard_controls(ROLL_SNAP_REPEAT_INTERVAL_SEC, &input);
        approx_eq(widget.camera_adjustments.roll_deg, -150.0);
    }

    #[test]
    fn f8_disabling_camera_controls_clears_roll_snap_repeat() {
        let mut widget = test_widget();
        widget.camera_controls_enabled = true;
        let mut input = roll_snap_test_input(KeyCode::ArrowRight);

        widget.apply_camera_keyboard_controls(0.0, &input);
        assert_eq!(widget.roll_snap_repeat.active_direction, 1);
        input.end_frame();
        input.inject_key(KeyCode::F8, true);
        widget.frame(0.0, &input);

        assert!(!widget.camera_controls_enabled);
        assert_eq!(widget.roll_snap_repeat, RollSnapRepeatState::default());
    }

    #[test]
    fn f8_roll_snap_repeat_is_frame_rate_independent() {
        fn roll_after_hold(dts: &[f32]) -> (f32, u32) {
            let mut widget = test_widget();
            widget.set_camera_adjustments(CameraRuntimeAdjustments {
                roll_deg: 7.0,
                ..CameraRuntimeAdjustments::default()
            });
            let mut input = roll_snap_test_input(KeyCode::ArrowRight);
            widget.apply_camera_keyboard_controls(0.0, &input);
            input.end_frame();
            for &dt in dts {
                widget.apply_camera_keyboard_controls(dt, &input);
            }
            (
                widget.camera_adjustments.roll_deg,
                widget.roll_snap_repeat.repeat_steps_emitted,
            )
        }

        let fine = roll_after_hold(&[0.05; 15]);
        let coarse = roll_after_hold(&[0.25, 0.25, 0.25]);
        let single_frame = roll_after_hold(&[0.75]);

        assert_eq!(fine, coarse);
        assert_eq!(coarse, single_frame);
        approx_eq(fine.0, 90.0);
        assert_eq!(fine.1, 5);
    }

    #[test]
    fn f8_ctrl_roll_remains_continuous_at_the_existing_rate() {
        let mut widget = test_widget();
        widget.set_camera_adjustments(CameraRuntimeAdjustments {
            roll_deg: 7.0,
            ..CameraRuntimeAdjustments::default()
        });
        let mut input = Input::default();
        input.inject_key(KeyCode::ControlLeft, true);
        input.inject_key(KeyCode::ArrowRight, true);
        assert_eq!(
            horizontal_camera_action(&input),
            HorizontalCameraAction::Roll
        );

        let dt = 0.1;
        widget.apply_camera_keyboard_controls(dt, &input);
        approx_eq(
            widget.camera_adjustments.roll_deg,
            7.0 + CAMERA_ROLL_RATE_DEG_PER_SEC * dt,
        );
        input.end_frame();
        widget.apply_camera_keyboard_controls(dt, &input);
        approx_eq(
            widget.camera_adjustments.roll_deg,
            7.0 + CAMERA_ROLL_RATE_DEG_PER_SEC * dt * 2.0,
        );
    }

    #[test]
    fn f8_yaw_crosses_old_limit_and_wraps_after_multiple_revolutions() {
        let mut widget = test_widget();
        widget.set_camera_adjustments(CameraRuntimeAdjustments {
            yaw_deg: 179.0,
            ..CameraRuntimeAdjustments::default()
        });
        let mut input = Input::default();
        input.inject_key(KeyCode::ShiftLeft, true);
        input.inject_key(KeyCode::ArrowRight, true);
        widget.apply_camera_keyboard_controls(0.25, &input);
        assert!(widget.camera_adjustments.yaw_deg < -150.0);
        assert!(widget.camera_adjustments.yaw_deg > -180.0);

        let right = camera_adjustments_after_repeated_keys(
            &[KeyCode::ShiftLeft, KeyCode::ArrowRight],
            17,
            0.25,
        );
        let left = camera_adjustments_after_repeated_keys(
            &[KeyCode::ShiftLeft, KeyCode::ArrowLeft],
            17,
            0.25,
        );
        approx_eq(right.yaw_deg, 22.5);
        approx_eq(left.yaw_deg, -22.5);
        assert!(right.yaw_deg.abs() < 180.0);
        assert!(left.yaw_deg.abs() < 180.0);

        let right_two_revolutions = camera_adjustments_after_repeated_keys(
            &[KeyCode::ShiftLeft, KeyCode::ArrowRight],
            32,
            0.25,
        );
        let left_two_revolutions = camera_adjustments_after_repeated_keys(
            &[KeyCode::ShiftLeft, KeyCode::ArrowLeft],
            32,
            0.25,
        );
        approx_eq(right_two_revolutions.yaw_deg, 0.0);
        approx_eq(left_two_revolutions.yaw_deg, 0.0);
    }

    #[test]
    fn f8_roll_wraps_after_multiple_revolutions_in_both_directions() {
        let right = camera_adjustments_after_repeated_keys(
            &[KeyCode::ControlLeft, KeyCode::ArrowRight],
            17,
            0.25,
        );
        let left = camera_adjustments_after_repeated_keys(
            &[KeyCode::ControlLeft, KeyCode::ArrowLeft],
            17,
            0.25,
        );
        approx_eq(right.roll_deg, 22.5);
        approx_eq(left.roll_deg, -22.5);
        assert_eq!(right.yaw_deg, 0.0);
        assert_eq!(left.yaw_deg, 0.0);
        assert_eq!(right.pan_ndc.x, 0.0);
        assert_eq!(left.pan_ndc.x, 0.0);

        let right_two_revolutions = camera_adjustments_after_repeated_keys(
            &[KeyCode::ControlLeft, KeyCode::ArrowRight],
            32,
            0.25,
        );
        let left_two_revolutions = camera_adjustments_after_repeated_keys(
            &[KeyCode::ControlLeft, KeyCode::ArrowLeft],
            32,
            0.25,
        );
        approx_eq(right_two_revolutions.roll_deg, 0.0);
        approx_eq(left_two_revolutions.roll_deg, 0.0);
    }

    #[test]
    fn f8_horizontal_pan_uses_witness_ndc_rate_without_selecting_yaw() {
        let dt = 1.0 / 60.0;
        let delta = pan_witness_delta_after_keys(&[KeyCode::ArrowRight], dt);
        approx_eq(delta.x, CAMERA_PAN_WITNESS_NDC_RATE_PER_SEC * dt);
        assert_eq!(delta.y, 0.0);

        let adjustments = camera_adjustments_after_keys(&[KeyCode::ArrowRight]);
        assert_eq!(adjustments.yaw_deg, 0.0);
        assert_eq!(adjustments.roll_deg, 0.0);
        assert_eq!(adjustments.pan_ndc, Vec2::ZERO);
    }

    #[test]
    fn f8_reset_clears_runtime_camera_adjustments() {
        let mut widget = test_widget();
        widget.set_camera_adjustments(CameraRuntimeAdjustments {
            fov_delta_deg: 4.0,
            distance_scale_delta: -0.1,
            pan_ndc: Vec2::new(0.03, 0.02),
            yaw_deg: 12.0,
            roll_deg: 7.0,
            pitch_deg: -8.0,
        });
        widget.camera_controls_enabled = true;

        let mut input = roll_snap_test_input(KeyCode::ArrowRight);
        widget.apply_camera_keyboard_controls(0.0, &input);
        assert_eq!(widget.roll_snap_repeat.active_direction, 1);
        input.end_frame();
        input.inject_key(KeyCode::KeyR, true);
        widget.frame(1.0 / 60.0, &input);

        assert_eq!(
            widget.camera_adjustments,
            CameraRuntimeAdjustments::default()
        );
        assert_eq!(widget.roll_snap_repeat, RollSnapRepeatState::default());
    }

    #[test]
    fn f8_camera_controls_are_gated_and_repeat_while_enabled() {
        let mut widget = test_widget();
        let mut input = Input::default();
        input.inject_key(KeyCode::ShiftLeft, true);
        input.inject_key(KeyCode::ArrowUp, true);

        widget.frame(1.0 / 60.0, &input);
        assert_eq!(
            widget.camera_adjustments,
            CameraRuntimeAdjustments::default()
        );

        input.inject_key(KeyCode::F8, true);
        widget.frame(0.0, &input);
        input.inject_key(KeyCode::F8, false);
        input.end_frame();

        widget.frame(1.0 / 60.0, &input);
        let first = widget.camera_adjustments.distance_scale_delta;
        input.end_frame();
        widget.frame(1.0 / 60.0, &input);
        let second = widget.camera_adjustments.distance_scale_delta;

        assert!(first < 0.0);
        approx_eq(second, first * 2.0);

        input.inject_key(KeyCode::F8, true);
        widget.frame(0.0, &input);
        assert!(!widget.camera_controls_enabled);
        input.inject_key(KeyCode::F8, false);
        input.end_frame();
        widget.frame(1.0 / 60.0, &input);
        assert_eq!(widget.camera_adjustments.distance_scale_delta, second);
    }

    #[test]
    fn aa_shortcuts_do_not_reset_runtime_camera_adjustments() {
        let mut widget = test_widget();
        let mut input = Input::default();
        let adjustments = CameraRuntimeAdjustments {
            fov_delta_deg: 4.0,
            distance_scale_delta: -0.1,
            pan_ndc: Vec2::new(0.03, 0.02),
            yaw_deg: 12.0,
            roll_deg: 7.0,
            pitch_deg: -8.0,
        };
        widget.set_camera_adjustments(adjustments);

        input.inject_key(KeyCode::F4, true);
        widget.frame(0.0, &input);
        input.inject_key(KeyCode::F4, false);
        input.end_frame();
        input.inject_key(KeyCode::F5, true);
        widget.frame(0.0, &input);

        assert_eq!(widget.camera_adjustments, adjustments);
        assert_eq!(
            widget.effective_camera_values(),
            adjustments.effective(widget.camera_settings)
        );
    }

    #[test]
    fn resize_preserves_runtime_camera_values_and_last_valid_viewport() {
        let mut widget = test_widget();
        let adjustments = CameraRuntimeAdjustments {
            fov_delta_deg: 4.0,
            distance_scale_delta: -0.1,
            pan_ndc: Vec2::new(0.03, 0.02),
            yaw_deg: 12.0,
            roll_deg: 7.0,
            pitch_deg: -8.0,
        };
        widget.set_camera_adjustments(adjustments);
        let before = widget.effective_camera_values();

        widget.update_viewport((900, 450));
        assert_eq!(widget.viewport_size, Some((900, 450)));
        assert_eq!(Camera::aspect_for_viewport((900, 450)), Some(2.0));
        assert_eq!(widget.effective_camera_values(), before);

        widget.update_viewport((0, 450));
        assert_eq!(widget.viewport_size, Some((900, 450)));
        assert_eq!(widget.effective_camera_values(), before);
    }

    #[test]
    fn f3_toggles_debug_hud_once_per_key_press() {
        let mut widget = test_widget();
        let mut input = Input::default();

        assert!(!widget.debug_hud_enabled);
        let (_, _, hud) = widget.compose(0.0, 0.0, (450, 600));
        assert!(hud.verts.is_empty());

        input.inject_key(KeyCode::F3, true);
        widget.frame(0.0, &input);
        assert!(widget.debug_hud_enabled);
        let (_, _, hud) = widget.compose(0.0, 0.0, (450, 600));
        assert!(!hud.verts.is_empty());

        input.end_frame();
        widget.frame(0.0, &input);
        assert!(widget.debug_hud_enabled);

        input.inject_key(KeyCode::F3, false);
        widget.frame(0.0, &input);
        input.end_frame();
        input.inject_key(KeyCode::F3, true);
        widget.frame(0.0, &input);
        assert!(!widget.debug_hud_enabled);
        let (_, _, hud) = widget.compose(0.0, 0.0, (450, 600));
        assert!(hud.verts.is_empty());
    }

    #[test]
    fn f4_queues_one_msaa_change_per_key_press() {
        let mut widget = test_widget();
        let mut input = Input::default();

        input.inject_key(KeyCode::F4, true);
        widget.frame(0.0, &input);
        assert_eq!(widget.requested_msaa, 2);
        assert_eq!(widget.pending_msaa_request, Some(2));

        input.end_frame();
        assert!(input.key_down(KeyCode::F4));
        assert!(!input.key_pressed(KeyCode::F4));
        widget.frame(0.0, &input);
        assert_eq!(widget.requested_msaa, 2);
        assert_eq!(widget.pending_msaa_request, Some(2));

        input.inject_key(KeyCode::F4, false);
        input.end_frame();
        input.inject_key(KeyCode::F4, true);
        widget.frame(0.0, &input);
        assert_eq!(widget.requested_msaa, 4);
        assert_eq!(widget.pending_msaa_request, Some(4));
    }

    #[test]
    fn f5_queues_one_smaa_change_per_key_press() {
        let mut widget = test_widget();
        let mut input = Input::default();

        assert!(!widget.requested_smaa);
        assert_eq!(widget.pending_smaa_request, None);

        input.inject_key(KeyCode::F5, true);
        widget.frame(0.0, &input);
        assert!(widget.requested_smaa);
        assert_eq!(widget.pending_smaa_request, Some(true));

        input.end_frame();
        assert!(input.key_down(KeyCode::F5));
        assert!(!input.key_pressed(KeyCode::F5));
        widget.frame(0.0, &input);
        assert!(widget.requested_smaa);
        assert_eq!(widget.pending_smaa_request, Some(true));

        input.inject_key(KeyCode::F5, false);
        input.end_frame();
        input.inject_key(KeyCode::F5, true);
        widget.frame(0.0, &input);
        assert!(!widget.requested_smaa);
        assert_eq!(widget.pending_smaa_request, Some(false));
    }

    #[test]
    fn persisted_preferences_seed_runtime_requests_without_a_gpu() {
        let settings = AppSettings {
            rendering: RenderSettings {
                msaa: AntiAliasingPreference::X8,
                smaa_enabled: true,
                ..RenderSettings::default()
            },
            ..AppSettings::default()
        };
        let widget = Widget::new_with_settings_path(test_config(), settings, None);

        assert_eq!(widget.requested_msaa, 8);
        assert!(widget.requested_smaa);
        assert_eq!(widget.pending_msaa_request, None);
        assert_eq!(widget.pending_smaa_request, None);
    }

    #[test]
    fn headless_widget_does_not_use_desktop_settings_path() {
        let widget = test_widget();

        assert!(widget.settings_path.is_none());
        assert_eq!(widget.requested_msaa, 1);
        assert!(!widget.requested_smaa);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn f4_persists_requested_msaa_without_rewriting_effective_or_smaa() {
        let Ok(gpu) = Gpu::new_headless() else {
            return;
        };
        let mut renderer = Renderer::new_with_config(
            &gpu,
            pocket3d::gpu::OFFSCREEN_FORMAT,
            pocket3d::renderer::RendererConfig {
                requested_sample_count: 4,
            },
        )
        .unwrap();
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let settings = AppSettings {
            rendering: RenderSettings {
                msaa: AntiAliasingPreference::X4,
                smaa_enabled: true,
                ..RenderSettings::default()
            },
            ..AppSettings::default()
        };
        let mut widget =
            Widget::new_with_settings_path(test_config(), settings, Some(path.clone()));
        let mut input = Input::default();
        input.inject_key(KeyCode::F4, true);
        widget.frame(0.0, &input);
        widget.prepare_render(&gpu, &mut renderer);

        let persisted = AppSettings::load_from_path(&path);
        assert_eq!(persisted.rendering.msaa, AntiAliasingPreference::X8);
        assert!(persisted.rendering.smaa_enabled);
        assert_eq!(renderer.requested_sample_count(), 8);
        assert!(renderer.effective_sample_count() <= 8);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn f5_persists_smaa_without_rewriting_msaa() {
        let Ok(gpu) = Gpu::new_headless() else {
            return;
        };
        let mut renderer = Renderer::new_with_config(
            &gpu,
            pocket3d::gpu::OFFSCREEN_FORMAT,
            pocket3d::renderer::RendererConfig {
                requested_sample_count: 2,
            },
        )
        .unwrap();
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let settings = AppSettings {
            rendering: RenderSettings {
                msaa: AntiAliasingPreference::X8,
                ..RenderSettings::default()
            },
            ..AppSettings::default()
        };
        let mut widget =
            Widget::new_with_settings_path(test_config(), settings, Some(path.clone()));
        let mut input = Input::default();
        input.inject_key(KeyCode::F5, true);
        widget.frame(0.0, &input);
        widget.prepare_render(&gpu, &mut renderer);

        let persisted = AppSettings::load_from_path(&path);
        assert_eq!(persisted.rendering.msaa, AntiAliasingPreference::X8);
        assert!(persisted.rendering.smaa_enabled);
        assert!(renderer.smaa_enabled());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn prepare_render_applies_smaa_without_changing_msaa() {
        let Ok(gpu) = Gpu::new_headless() else {
            return;
        };
        let mut renderer = Renderer::new_with_config(
            &gpu,
            pocket3d::gpu::OFFSCREEN_FORMAT,
            pocket3d::renderer::RendererConfig {
                requested_sample_count: 2,
            },
        )
        .unwrap();
        let mut widget = test_widget();
        widget.requested_smaa = true;
        widget.pending_smaa_request = Some(true);

        let requested_msaa = renderer.requested_sample_count();
        let effective_msaa = renderer.effective_sample_count();
        widget.prepare_render(&gpu, &mut renderer);

        assert!(renderer.smaa_enabled());
        assert_eq!(renderer.requested_sample_count(), requested_msaa);
        assert_eq!(renderer.effective_sample_count(), effective_msaa);
        assert!(widget.debug_smaa_enabled);
        assert_eq!(widget.pending_smaa_request, None);

        widget.requested_smaa = false;
        widget.pending_smaa_request = Some(false);
        widget.prepare_render(&gpu, &mut renderer);

        assert!(!renderer.smaa_enabled());
        assert_eq!(renderer.requested_sample_count(), requested_msaa);
        assert_eq!(renderer.effective_sample_count(), effective_msaa);
        assert!(!widget.debug_smaa_enabled);
    }

    #[test]
    fn msaa_cycle_wraps_and_sanitizes() {
        assert_eq!(next_msaa_sample_count(1), 2);
        assert_eq!(next_msaa_sample_count(2), 4);
        assert_eq!(next_msaa_sample_count(4), 8);
        assert_eq!(next_msaa_sample_count(8), 1);
        assert_eq!(next_msaa_sample_count(16), 2);
    }

    #[test]
    fn msaa_hud_explicitly_formats_requested_and_effective_modes() {
        assert_eq!(
            format_msaa_hud_line(1, 1),
            "MSAA: requested off / effective off"
        );
        assert_eq!(
            format_msaa_hud_line(4, 4),
            "MSAA: requested 4x / effective 4x"
        );
    }

    #[test]
    fn msaa_hud_preserves_requested_effective_fallback() {
        assert_eq!(
            format_msaa_hud_line(8, 4),
            "MSAA: requested 8x / effective 4x"
        );
    }

    #[test]
    fn smaa_hud_status_remains_independent_on_or_off() {
        assert_eq!(format_smaa_hud_line(true), "SMAA: on");
        assert_eq!(format_smaa_hud_line(false), "SMAA: off");

        assert_eq!(
            format_msaa_hud_line(8, 4),
            "MSAA: requested 8x / effective 4x"
        );
    }

    #[test]
    fn render_fps_uses_render_frame_delta() {
        let mut widget = test_widget();
        let input = Input::default();

        for _ in 0..4 {
            widget.frame(0.25, &input);
        }

        approx_eq(widget.render_fps.fps, 4.0);
    }
}
