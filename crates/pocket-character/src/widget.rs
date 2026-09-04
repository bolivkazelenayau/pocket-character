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
use crate::menu_guest::MenuGuest;
use crate::settings::{AntiAliasingPreference, AppSettings, CameraSettings};

mod aa;
mod camera;
mod controls;
mod diagnostics;

use aa::AaRuntime;
#[cfg(test)]
use camera::CameraRuntimeAdjustments;
use camera::controls::{CameraControls, CameraSnapSteps};
use camera::{
    CameraPanContext, DEFAULT_VIEWPORT_ASPECT, EffectiveCameraValues,
    resolve_camera_parameters_with_aspect,
};
use controls::{ControlAction, ControlsSnapshot};
use diagnostics::{FrameStats, RenderFps};

pub struct WidgetConfig {
    pub model_path: PathBuf,
    pub vrma_path: PathBuf,
    pub bundle_path: PathBuf,
    /// Generated PocketUI menu bundle (`dist/menu.js`).
    pub menu_bundle_path: PathBuf,
    /// Generated PocketUI menu pak (`dist/menu.pak`).
    pub menu_pak_path: PathBuf,
    pub size: (u32, u32),
    /// Render N frames then exit (verification runs).
    pub frames: Option<u32>,
}

pub struct Widget {
    cfg: WidgetConfig,
    guest: Option<CharacterGuest>,
    /// PocketUI overlay guest — a separate QuickJS realm from `guest`.
    /// Boots in `init` once the GPU/renderer exist; `None` until then (and
    /// in unit tests that never call `init`).
    menu: Option<MenuGuest>,

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
    camera_controls: CameraControls,
    viewport_size: Option<(u32, u32)>,
    settings: AppSettings,
    settings_path: Option<PathBuf>,
    #[cfg(test)]
    save_count: usize,

    stats: FrameStats,
    render_fps: RenderFps,
    debug_hud_enabled: bool,
    debug_gpu_name: String,
    debug_backend: String,
    aa: AaRuntime,
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
        let mut settings = AppSettings::default();
        settings.camera = camera_settings.sanitized();
        Self::new_internal(cfg, settings, None, 1, false)
    }

    pub fn new_with_settings_path(
        cfg: WidgetConfig,
        settings: AppSettings,
        settings_path: Option<PathBuf>,
    ) -> Self {
        let settings = settings.sanitized();
        let requested_msaa = settings.rendering.msaa.samples().unwrap_or(1);
        let requested_smaa = settings.rendering.smaa_enabled;
        Self::new_internal(cfg, settings, settings_path, requested_msaa, requested_smaa)
    }

    fn new_internal(
        cfg: WidgetConfig,
        settings: AppSettings,
        settings_path: Option<PathBuf>,
        requested_msaa: u32,
        requested_smaa: bool,
    ) -> Self {
        // `AppSettings.camera` is the single canonical persisted/base camera
        // settings. `CameraControls` holds runtime/session adjustments.
        // `reapply_camera()` is the common application path.
        let settings = settings.sanitized();
        // Seed fixed for reproducible measurement runs; behavior parity is
        // distributional, not per-run.
        let sim = CharacterSim::new(0x0c9a_11e0, Vec3::ZERO);
        Self {
            cfg,
            guest: None,
            menu: None,
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
            camera_controls: CameraControls::default(),
            viewport_size: None,
            settings,
            settings_path,
            #[cfg(test)]
            save_count: 0,
            stats: FrameStats::new(),
            render_fps: RenderFps::new(),
            debug_hud_enabled: false,
            debug_gpu_name: "unknown".into(),
            debug_backend: "unknown".into(),
            aa: AaRuntime::new(requested_msaa, requested_smaa),
            tick_count: 0,
            hovered: false,
            pending_events: Vec::new(),
            exit: false,
            rendered_frames: 0,
        }
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
        let viewport_aspect = self.camera_viewport_aspect();
        self.camera_controls.validate_pan(
            CameraPanContext::new(aabb, self.settings.camera),
            viewport_aspect,
        );
        let parameters = resolve_camera_parameters_with_aspect(
            aabb,
            self.settings.camera,
            self.camera_controls.adjustments(),
            viewport_aspect,
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

    #[cfg(test)]
    fn set_camera_adjustments(&mut self, adjustments: CameraRuntimeAdjustments) {
        self.camera_controls.set_adjustments(adjustments);
        self.reapply_camera();
    }

    fn effective_camera_values(&self) -> EffectiveCameraValues {
        self.camera_controls
            .adjustments()
            .effective(self.settings.camera)
    }

    fn camera_snap_steps(&self) -> CameraSnapSteps {
        CameraSnapSteps {
            yaw_deg: self.settings.camera.yaw_snap_deg,
            roll_deg: self.settings.camera.roll_snap_deg,
            pitch_deg: self.settings.camera.pitch_snap_deg,
        }
    }

    /// Settings/action boundary for persisted/base camera changes.
    ///
    /// `AppSettings.camera` is the single canonical persisted/base camera
    /// settings. `CameraControls` holds runtime/session adjustments (including
    /// keyboard/session controls, which mutate it directly). Each
    /// `ControlAction` that touches persisted settings sanitizes through the
    /// existing settings/kernel policy, rebases runtime FOV/distance deltas
    /// where appropriate, revalidates pan through the kernel, reapplies via
    /// the common `reapply_camera()` path, and persists only accepted
    /// committed changes (once per action). No clamp/safety math is duplicated
    /// here.
    pub(crate) fn apply_control_action(&mut self, action: ControlAction) -> ControlsSnapshot {
        match action {
            ControlAction::SetBaseFov(fov_deg) => {
                let mut candidate = self.settings.camera;
                candidate.fov_deg = fov_deg;
                let sanitized = candidate.sanitized();
                if sanitized != self.settings.camera {
                    let fov_changed = sanitized.fov_deg != self.settings.camera.fov_deg;
                    self.settings.camera = sanitized;
                    // Clear only the corresponding delta on an accepted base
                    // change so no hidden keyboard delta reappears. No-ops
                    // preserve keyboard feel.
                    if fov_changed {
                        self.camera_controls.clear_fov_delta();
                    }
                    self.reapply_camera();
                    self.persist_settings();
                }
            }
            ControlAction::SetBaseDistance(distance_scale) => {
                let mut candidate = self.settings.camera;
                candidate.distance_scale = distance_scale;
                let sanitized = candidate.sanitized();
                if sanitized != self.settings.camera {
                    let distance_changed =
                        sanitized.distance_scale != self.settings.camera.distance_scale;
                    self.settings.camera = sanitized;
                    if distance_changed {
                        self.camera_controls.clear_distance_delta();
                    }
                    self.reapply_camera();
                    self.persist_settings();
                }
            }
            ControlAction::SetYaw(yaw_deg) => {
                self.camera_controls.set_yaw_deg(yaw_deg);
                self.reapply_camera();
            }
            ControlAction::SetPitch(pitch_deg) => {
                self.camera_controls.set_pitch_deg(pitch_deg);
                self.reapply_camera();
            }
            ControlAction::SetRoll(roll_deg) => {
                self.camera_controls.set_roll_deg(roll_deg);
                self.reapply_camera();
            }
            ControlAction::ResetRuntimeCamera => {
                self.camera_controls.reset_adjustments();
                self.reapply_camera();
            }
            ControlAction::SetYawSnap(snap_deg) => {
                let mut candidate = self.settings.camera;
                candidate.yaw_snap_deg = snap_deg;
                let sanitized = candidate.sanitized();
                if sanitized != self.settings.camera {
                    self.settings.camera = sanitized;
                    self.reapply_camera();
                    self.persist_settings();
                }
            }
            ControlAction::SetPitchSnap(snap_deg) => {
                let mut candidate = self.settings.camera;
                candidate.pitch_snap_deg = snap_deg;
                let sanitized = candidate.sanitized();
                if sanitized != self.settings.camera {
                    self.settings.camera = sanitized;
                    self.reapply_camera();
                    self.persist_settings();
                }
            }
            ControlAction::SetRollSnap(snap_deg) => {
                let mut candidate = self.settings.camera;
                candidate.roll_snap_deg = snap_deg;
                let sanitized = candidate.sanitized();
                if sanitized != self.settings.camera {
                    self.settings.camera = sanitized;
                    self.reapply_camera();
                    self.persist_settings();
                }
            }
            ControlAction::SetAllSnaps {
                yaw_deg,
                pitch_deg,
                roll_deg,
            } => {
                let mut candidate = self.settings.camera;
                candidate.yaw_snap_deg = yaw_deg;
                candidate.pitch_snap_deg = pitch_deg;
                candidate.roll_snap_deg = roll_deg;
                let sanitized = candidate.sanitized();
                if sanitized != self.settings.camera {
                    self.settings.camera = sanitized;
                    self.reapply_camera();
                    self.persist_settings();
                }
            }
            ControlAction::RequestMsaa(preference) => {
                self.aa
                    .request_msaa_samples(preference.samples().unwrap_or(1));
            }
            ControlAction::RequestSmaa(enabled) => {
                self.aa.request_smaa(enabled);
            }
        }
        self.controls_snapshot()
    }

    pub(crate) fn controls_snapshot(&self) -> ControlsSnapshot {
        let base = self.settings.camera.sanitized();
        let adjustments = self.camera_controls.adjustments();
        let effective = adjustments.effective(self.settings.camera);
        let requested_msaa = AntiAliasingPreference::from_samples(self.aa.requested_msaa())
            .unwrap_or(AntiAliasingPreference::Off);
        let pending = self.aa.pending_requests();
        ControlsSnapshot::new(
            base.fov_deg,
            base.distance_scale,
            adjustments.sanitized().yaw_deg,
            adjustments.sanitized().pitch_deg,
            adjustments.sanitized().roll_deg,
            base.yaw_snap_deg,
            base.pitch_snap_deg,
            base.roll_snap_deg,
            effective.settings.fov_deg,
            effective.settings.distance_scale,
            requested_msaa,
            self.aa.effective_msaa(),
            self.aa.requested_smaa(),
            self.aa.effective_smaa(),
            pending.msaa.is_some(),
            pending.smaa.is_some(),
        )
    }

    fn persist_settings(&mut self) {
        let Some(path) = self.settings_path.clone() else {
            return;
        };
        if let Err(error) = self.settings.save_to_path(&path) {
            log::warn!(
                "unable to persist settings to {}: {error:#}",
                path.display()
            );
        }
        #[cfg(test)]
        {
            self.save_count += 1;
        }
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
        self.aa.initialize_msaa_from_renderer(
            renderer.requested_sample_count(),
            renderer.effective_sample_count(),
        );
        renderer.set_smaa_enabled(gpu, self.settings.rendering.smaa_enabled);
        self.aa.initialize_smaa_from_renderer(
            self.settings.rendering.smaa_enabled,
            renderer.smaa_enabled(),
        );

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
        // model's bounds. `AppSettings.camera` is the canonical persisted/base
        // settings, `CameraControls` holds session adjustments, and
        // `reapply_camera()` is the common application path.
        self.model = Some(model.clone());
        self.reapply_camera();

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

        // The menu guest boots independently of the character guest (separate
        // QuickJS realm + ui surface), after the GPU/renderer exist so the
        // overlay pipeline can bind the render target's color format.
        let (menu_bundle, menu_pak) = crate::menu_guest::load_menu_assets(
            &self.cfg.menu_bundle_path,
            &self.cfg.menu_pak_path,
        )?;
        self.menu = Some(MenuGuest::boot(
            gpu,
            &menu_bundle,
            &menu_pak,
            (self.cfg.size.0 as f32, self.cfg.size.1 as f32),
            renderer.color_format,
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
            self.aa.request_next_msaa();
        }
        if input.key_pressed(KeyCode::F5) {
            self.aa.request_smaa_toggle();
        }
        // Temporary F8 validation controls are never written to AppSettings.
        let pan_context = self
            .model
            .as_ref()
            .map(|model| CameraPanContext::new(model.aabb, self.settings.camera));
        let camera_changed = self.camera_controls.apply_frame(
            dt,
            input,
            pan_context,
            self.camera_snap_steps(),
            self.camera_viewport_aspect(),
        );
        if camera_changed {
            self.reapply_camera();
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
            fps: self.stats.fps(),
            frame_ms: self.stats.frame_ms(),
        };
        if let Some(guest) = &self.guest {
            match guest.turn(&state, &events) {
                Ok(commands) => self.apply_commands(commands),
                Err(e) => log::error!("guest turn: {e:#}"),
            }
        }

        self.stats.record(t0.elapsed().as_secs_f32() * 1000.0);

        // --- menu (PocketUI controls bridge) ------------------------------
        // A separate concern layered after the character lifecycle above.
        // One-way facts only this pass: the authoritative snapshot is
        // queued ahead of the guest turn, so the framework frame observes
        // this tick's effective camera values (no controls wired yet).
        let snapshot = self.controls_snapshot();
        if let Some(menu) = self.menu.as_mut() {
            menu.push_state(
                snapshot.effective_fov_deg(),
                snapshot.effective_distance_scale(),
            );
            if let Err(e) = menu.step() {
                log::error!("menu frame: {e:#}");
            }
        }
    }

    fn prepare_render(&mut self, gpu: &Gpu, renderer: &mut Renderer) {
        let requests = self.aa.take_pending_requests();
        if requests.is_empty() {
            return;
        }

        let mut accepted_msaa = None;
        if let Some(requested) = requests.msaa {
            renderer.set_requested_sample_count(gpu, requested);
            accepted_msaa = requests.accepted_msaa(renderer.requested_sample_count());
            if accepted_msaa.is_none() {
                log::warn!(
                    "renderer rejected requested MSAA {}; keeping persisted preference",
                    diagnostics::format_msaa_count(requested)
                );
            }
        }
        let mut accepted_smaa = None;
        if let Some(enabled) = requests.smaa {
            renderer.set_smaa_enabled(gpu, enabled);
            accepted_smaa = requests.accepted_smaa(renderer.smaa_enabled());
            if accepted_smaa.is_none() {
                log::warn!(
                    "renderer rejected requested SMAA {}; keeping persisted preference",
                    if enabled { "on" } else { "off" }
                );
            }
        }

        self.aa.sync_after_application(
            renderer.requested_sample_count(),
            renderer.effective_sample_count(),
            renderer.smaa_enabled(),
        );
        self.commit_accepted_aa_preferences(accepted_msaa, accepted_smaa);
        let aa = self.aa.status();
        log::info!(
            "AA: requested {}, effective MSAA {}, SMAA {}",
            diagnostics::format_msaa_count(aa.requested_msaa),
            diagnostics::format_msaa_count(aa.effective_msaa),
            if aa.smaa_enabled { "on" } else { "off" }
        );
    }

    fn compose(&mut self, _alpha: f32, time: f32, size: (u32, u32)) -> (&Scene, &Camera, &Hud) {
        self.scene.time = time;
        self.update_viewport(size);
        // The menu lays out in the same pixel space the rest of the widget
        // uses (the size the app loop reports; see compose_debug_hud).
        if let Some(menu) = self.menu.as_mut()
            && let Err(e) = menu.set_viewport(size.0 as f32, size.1 as f32)
        {
            log::error!("menu resize: {e:#}");
        }
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

    /// PocketUI overlay: the menu draw list alpha-blended over the finished
    /// character frame. Runs on the logical output view the app loop hands
    /// us (transparent Windows path included); `LoadOp::Load` keeps every
    /// pixel the scene pass wrote, and the UI pipeline has no depth
    /// attachment so character depth is untouched.
    fn overlay(
        &mut self,
        gpu: &Gpu,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        format: wgpu::TextureFormat,
        size: (u32, u32),
    ) {
        if let Some(menu) = self.menu.as_mut()
            && let Err(e) = menu.render(gpu, encoder, view, format, size)
        {
            log::error!("menu overlay: {e:#}");
        }
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

        if changed {
            self.persist_settings();
        }
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

        let aa = self.aa.status();
        let text = diagnostics::format_debug_hud(
            size,
            &self.stats,
            &self.render_fps,
            &self.debug_gpu_name,
            &self.debug_backend,
            aa.requested_msaa,
            aa.effective_msaa,
            aa.smaa_enabled,
            self.effective_camera_values(),
            self.camera_controls.camera_controls_enabled(),
        );

        let body_width = text
            .lines
            .iter()
            .map(|line| Hud::text_width(line, 1.0))
            .fold(0.0, f32::max);
        let panel_width = Hud::text_width(text.title, 2.0).max(body_width) + PANEL_PADDING * 2.0;
        let panel_bottom = BODY_Y + (text.lines.len() - 1) as f32 * LINE_HEIGHT + 8.0;
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
            .text(X, TITLE_Y, 2.0, [0.86, 0.96, 1.0, 1.0], text.title);
        for (index, line) in text.lines.iter().enumerate() {
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

#[cfg(test)]
mod tests;
