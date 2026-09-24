//! The widget game: owns the character's per-tick pipeline.
//!
//! Tick order mirrors airi's VRMModel update (mixer → humanoid → lookAt →
//! blink → expressions → constraints → springs), mapped onto the Pocket
//! shape: sample clip locals → eye look-at → expressions → constraints →
//! spring bones → globals → palette; blink lands as morph weights, uploaded
//! only when it changes.

use std::fmt::Display;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use glam::{Vec2, Vec3};
use pocket_character_core::{CharacterSim, TrackingMode};
use pocket3d::app::{Game, TextInputRequest, WindowRuntimeRequest, WindowRuntimeState};
use pocket3d::camera::Camera;
use pocket3d::gpu::Gpu;
use pocket3d::hud::Hud;
use pocket3d::input::Input;
use pocket3d::renderer::Renderer;
use pocket3d::scene::Scene;
use pocket3d::winit::keyboard::KeyCode;

use crate::guest::{Command, TickEvent, TickState};
use crate::menu_guest::{
    MenuAction, MenuGuest, MenuInputFrame, MenuInputModifiers, MenuTextInputCapture,
    MenuWindowState,
};
use crate::settings::{AntiAliasingPreference, AppSettings, CameraSettings};

mod aa;
mod avatar;
mod camera;
mod controls;
mod diagnostics;
mod expression;
mod node_constraint;

use aa::AaRuntime;
use avatar::{
    ActiveAvatar, AvatarCandidate, AvatarLoadErrorKind, AvatarLoadRequest, AvatarLoadStatus,
    AvatarRuntimeError, AvatarSceneSlot, avatar_request_from_picker_result, startup_avatar_request,
};
#[cfg(test)]
use camera::CameraRuntimeAdjustments;
use camera::controls::{CameraControls, CameraSnapSteps};
use camera::{
    CameraPanContext, DEFAULT_VIEWPORT_ASPECT, EffectiveCameraValues,
    resolve_camera_parameters_with_aspect,
};
use controls::{ControlAction, ControlsSnapshot};
use diagnostics::{FrameStats, RenderFps};
use expression::ResolvedExpressionRuntime;

pub struct WidgetConfig {
    pub model_path: PathBuf,
    pub vrma_path: PathBuf,
    pub bundle_path: PathBuf,
    /// Generated PocketUI menu bundle (`dist/menu.js`).
    pub menu_bundle_path: PathBuf,
    /// Generated PocketUI menu pak (`dist/menu.pak`).
    pub menu_pak_path: PathBuf,
    pub size: (u32, u32),
    /// Launch-effective CLI FPS override, if `--max-fps` was explicitly set.
    /// This is process-local and must never be persisted through AppSettings.
    pub cli_max_fps_override: Option<f32>,
    /// Render N frames then exit (verification runs).
    pub frames: Option<u32>,
}

#[derive(Debug, Default)]
struct MenuHealth {
    /// The first terminal runtime error disables the overlay for the rest of
    /// this process. Keeping the original message lets headless callers
    /// report why a capture was rejected.
    failure: Option<String>,
}

impl MenuHealth {
    fn is_healthy(&self) -> bool {
        self.failure.is_none()
    }

    fn latch(&mut self, operation: &str, error: impl Display) -> bool {
        if self.failure.is_some() {
            return false;
        }
        self.failure = Some(format!("{operation}: {error}"));
        true
    }

    fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }
}

const DEFAULT_WINDOW_SCALE_FACTOR: f64 = 1.0;
const MOUSE_RAY_PLANE_EPSILON: f32 = 1.0e-5;
/// Keep the cursor target close enough to the head for visible gaze while
/// retaining forward depth. Expressing the offset as a camera-distance ratio
/// makes zooming preserve approximately the same angular response.
const MOUSE_TARGET_DEPTH_TOWARD_CAMERA: f32 = 0.25;

/// The accepted Settings layout is authored for the default logical client
/// area. A persisted character window may be smaller, so the native client
/// area is expanded only for the live Settings presentation. This request is
/// process-local and never changes `AppSettings`.
const SETTINGS_PRESENTATION_SIZE: (u32, u32) = (450, 600);

/// A logical size request that has not yet been resolved by a changed native
/// observation. Pocket3D retains the last applied request for deduplication,
/// so parent-side pending ownership must be tracked separately.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingWindowSizeRequest {
    requested_logical_size: (u32, u32),
    observation_baseline: Option<(u32, u32)>,
    observation_generation: u64,
    /// Older requests that Pocket3D may already have submitted but whose
    /// asynchronous native observations have not reached the parent yet.
    superseded_logical_sizes: Vec<(u32, u32)>,
}

fn normalized_scale_factor(scale_factor: f64) -> f64 {
    if scale_factor.is_finite() && scale_factor > 0.0 {
        scale_factor
    } else {
        DEFAULT_WINDOW_SCALE_FACTOR
    }
}

/// Convert a physical window size to the logical pixel viewport consumed by
/// PocketUI. Rounding mirrors the note-widget host so layout stays integral
/// while a fractional-DPI window is resized.
fn logical_viewport_for(physical_size: (u32, u32), scale_factor: f64) -> (f32, f32) {
    let scale_factor = normalized_scale_factor(scale_factor);
    (
        ((physical_size.0 as f64 / scale_factor).round().max(1.0)) as f32,
        ((physical_size.1 as f64 / scale_factor).round().max(1.0)) as f32,
    )
}

fn sanitized_window_dimension(value: f32, current: u32) -> u32 {
    if value.is_finite() {
        value.max(0.0).round() as u32
    } else {
        current
    }
}

fn native_drag_allowed_for_menu_pointer(menu_pointer_owned: bool) -> bool {
    !menu_pointer_owned
}

/// Intersect a world-space ray with a plane. Invalid, parallel, and
/// behind-camera intersections are rejected so callers can choose a stable
/// fallback instead of retaining an obsolete target.
fn ray_plane_target(
    ray_origin: Vec3,
    ray_direction: Vec3,
    plane_point: Vec3,
    plane_normal: Vec3,
) -> Option<Vec3> {
    if !ray_origin.is_finite()
        || !ray_direction.is_finite()
        || !plane_point.is_finite()
        || !plane_normal.is_finite()
    {
        return None;
    }

    let denominator = ray_direction.dot(plane_normal);
    if !denominator.is_finite() || denominator.abs() <= MOUSE_RAY_PLANE_EPSILON {
        return None;
    }
    let distance = (plane_point - ray_origin).dot(plane_normal) / denominator;
    if !distance.is_finite() || distance < 0.0 {
        return None;
    }

    let target = ray_origin + ray_direction * distance;
    target.is_finite().then_some(target)
}

/// Resolve a physical-pixel cursor through the camera onto a camera-facing
/// plane just in front of the avatar head. The plane keeps pointer motion
/// independent of arbitrary ray distance while preserving forward depth and
/// screen-left/right/up/down.
fn mouse_target_from_screen(
    camera: &Camera,
    cursor: Vec2,
    viewport: (u32, u32),
    head_world: Vec3,
) -> Option<Vec3> {
    Camera::aspect_for_viewport(viewport)?;
    if !cursor.is_finite()
        || cursor.x < 0.0
        || cursor.y < 0.0
        || cursor.x > viewport.0 as f32
        || cursor.y > viewport.1 as f32
        || !head_world.is_finite()
    {
        return None;
    }

    let (origin, direction) = camera.screen_ray(cursor, (viewport.0 as f32, viewport.1 as f32));
    let plane_point = head_world.lerp(camera.pos, MOUSE_TARGET_DEPTH_TOWARD_CAMERA);
    ray_plane_target(origin, direction, plane_point, camera.forward())
}

fn resolved_mouse_target(
    camera: &Camera,
    cursor: Option<Vec2>,
    viewport: (u32, u32),
    head_world: Vec3,
    fallback: Vec3,
) -> Vec3 {
    cursor
        .and_then(|cursor| mouse_target_from_screen(camera, cursor, viewport, head_world))
        .unwrap_or(fallback)
}

fn set_tracking_mode(sim: &mut CharacterSim, mode: &str) {
    sim.tracking = if mode == "mouse" {
        TrackingMode::Mouse
    } else {
        TrackingMode::None
    };
    // Mouse mode must never revive a target from an earlier tracking session.
    // Camera/idle targeting is also the safe initial value until the next
    // valid cursor frame arrives.
    sim.mouse_target = sim.look_base;
}

/// Map the guest's logical PocketUI caret rectangle to the physical-pixel
/// contract consumed by Pocket3D's winit window loop. Invalid geometry
/// releases the native IME rather than allowing a non-finite per-frame update.
fn physical_text_input_request(
    capture: MenuTextInputCapture,
    scale_factor: f64,
) -> TextInputRequest {
    if !capture.active {
        return TextInputRequest::default();
    }
    let Some(area) = capture.cursor_area_logical_px else {
        return TextInputRequest::default();
    };
    let scale = normalized_scale_factor(scale_factor);
    if !scale.is_finite() || !area.0.is_finite() || !area.1.is_finite() {
        return TextInputRequest::default();
    }
    if !area.2.is_finite() || !area.3.is_finite() {
        return TextInputRequest::default();
    }

    let physical = (
        area.0 as f64 * scale,
        area.1 as f64 * scale,
        area.2 as f64 * scale,
        area.3 as f64 * scale,
    );
    if ![physical.0, physical.1, physical.2, physical.3]
        .iter()
        .all(|value| value.is_finite())
    {
        return TextInputRequest::default();
    }
    let physical = (
        physical.0 as f32,
        physical.1 as f32,
        physical.2 as f32,
        physical.3 as f32,
    );
    if !physical.0.is_finite()
        || !physical.1.is_finite()
        || !physical.2.is_finite()
        || !physical.3.is_finite()
    {
        return TextInputRequest::default();
    }

    TextInputRequest {
        active: true,
        cursor_area_px: Some(physical),
    }
}

#[derive(Clone, Copy, Debug)]
struct MenuPress {
    cursor: glam::Vec2,
    owned: bool,
}

#[derive(Clone, Copy, Debug)]
struct MenuPointerFrame {
    cursor: Option<glam::Vec2>,
    press_cursor: Option<glam::Vec2>,
    pressed_edge: bool,
    button_down: bool,
    cancelled: bool,
}

pub struct Widget {
    cfg: WidgetConfig,
    /// PocketUI overlay guest — a separate QuickJS realm from the avatar guest.
    /// Boots in `init` once the GPU/renderer exist; `None` until then (and
    /// in unit tests that never call `init`).
    menu: Option<MenuGuest>,
    menu_health: MenuHealth,

    /// The only avatar-specific state owned by Widget.  Its scene instance is
    /// kept in `scene.models` at `active.scene_slot`; all replacement inputs
    /// are prepared as an `AvatarCandidate` before this value changes.
    active_avatar: Option<ActiveAvatar>,
    pending_avatar_request: Option<AvatarLoadRequest>,
    avatar_load_status: AvatarLoadStatus,
    #[cfg(test)]
    avatar_prepare_count: usize,

    scene: Scene,
    camera: Camera,
    hud: Hud,
    anchor: Vec3,
    camera_controls: CameraControls,
    viewport_size: Option<(u32, u32)>,
    /// Physical window metrics supplied by the Pocket3D desktop host.
    window_physical_size: Option<(u32, u32)>,
    window_scale_factor: f64,
    window_runtime_state: Option<WindowRuntimeState>,
    window_runtime_request: WindowRuntimeRequest,
    window_observation_generation: u64,
    pending_window_size_request: Option<PendingWindowSizeRequest>,
    settings_visible: bool,
    cli_max_fps_override: Option<f32>,
    cli_max_fps_active: bool,
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
    last_blink: f32,
    hovered: bool,
    /// UI ownership is latched from the press edge until release. The same
    /// latch gates native dragging and CharacterGuest click delivery.
    menu_pointer_owned: bool,
    /// Pointer edges are captured once per rendered frame and consumed by the
    /// next menu tick. This survives zero-tick frames and prevents a catch-up
    /// loop from replaying the same edge.
    pending_menu_press: Option<MenuPress>,
    pending_menu_pointer: Vec<MenuPointerFrame>,
    observed_menu_pointer: Option<(Option<glam::Vec2>, bool)>,
    /// PocketUI owns text capture. These frames are captured once per render
    /// frame and consumed by the next menu tick, including across zero-tick
    /// frames but never more than once during catch-up.
    menu_text_input_capture: MenuTextInputCapture,
    pending_menu_input: Vec<MenuInputFrame>,
    last_menu_input_modifiers: Option<MenuInputModifiers>,
    /// After text capture ends, hold camera keyboard routing until all camera
    /// keys have been released so a key pressed during editing cannot leak.
    camera_input_blocked: bool,
    /// Accepted outside-menu presses waiting for the next character guest
    /// turn. Keep the count so multiple zero-tick frames do not coalesce.
    pending_character_clicks: usize,
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
        let cli_max_fps_override = cfg.cli_max_fps_override;
        Self {
            cfg,
            menu: None,
            menu_health: MenuHealth::default(),
            active_avatar: None,
            pending_avatar_request: None,
            avatar_load_status: AvatarLoadStatus::default(),
            #[cfg(test)]
            avatar_prepare_count: 0,
            scene: Scene::default(),
            camera: Camera::default(),
            hud: Hud::default(),
            anchor: Vec3::ZERO,
            camera_controls: CameraControls::default(),
            viewport_size: None,
            window_physical_size: None,
            window_scale_factor: DEFAULT_WINDOW_SCALE_FACTOR,
            window_runtime_state: None,
            window_runtime_request: WindowRuntimeRequest::default(),
            window_observation_generation: 0,
            pending_window_size_request: None,
            settings_visible: false,
            cli_max_fps_override,
            cli_max_fps_active: cli_max_fps_override.is_some(),
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
            last_blink: 0.0,
            hovered: false,
            menu_pointer_owned: false,
            pending_menu_press: None,
            pending_menu_pointer: Vec::new(),
            observed_menu_pointer: None,
            menu_text_input_capture: MenuTextInputCapture::default(),
            pending_menu_input: Vec::new(),
            last_menu_input_modifiers: None,
            camera_input_blocked: false,
            pending_character_clicks: 0,
            pending_events: Vec::new(),
            exit: false,
            rendered_frames: 0,
        }
    }

    fn reapply_camera(&mut self) {
        if self.active_avatar.is_some() {
            self.apply_camera_settings();
        }
    }

    fn update_mouse_tracking_target(&mut self, cursor: Option<Vec2>) {
        let Some(active) = self.active_avatar.as_ref() else {
            return;
        };
        if active.sim.tracking != TrackingMode::Mouse {
            return;
        }

        let fallback = active.sim.look_base;
        let head_world = active
            .document
            .humanoid_node("head")
            .and_then(|node| active.globals.get(node))
            .map(|global| global.w_axis.truncate())
            .filter(|position| position.is_finite())
            .map(|position| active.presentation.world_point_from_model(position))
            .filter(|position| position.is_finite())
            .unwrap_or_else(|| {
                let (min, max) = active.presentation_aabb;
                (min + max) * 0.5
            });
        // A reported zero physical size means the window is minimized. Do
        // not fall back to a stale pre-minimize viewport in that case.
        let viewport = self
            .window_physical_size
            .or(self.viewport_size)
            .unwrap_or(self.cfg.size);
        let target = resolved_mouse_target(&self.camera, cursor, viewport, head_world, fallback);

        if let Some(active) = self.active_avatar.as_mut() {
            active.sim.mouse_target = target;
        }
    }

    /// Queue a parent-side replacement.  This is intentionally an internal
    /// seam: a future file picker can provide a request without becoming part
    /// of the render or guest ownership model.
    pub(crate) fn request_avatar_replacement(&mut self, mut request: AvatarLoadRequest) {
        request.mtoon_render_mode = self.settings.rendering.mtoon_render_mode;
        self.pending_avatar_request = Some(request);
        self.avatar_load_status.begin_loading();
    }

    fn open_avatar_picker(&mut self) {
        #[cfg(windows)]
        {
            let selected = rfd::FileDialog::new()
                .set_title("Open Avatar")
                .add_filter("VRM avatar", &["vrm"])
                .pick_file();
            if let Some(request) = avatar_request_from_picker_result(selected, &self.cfg.vrma_path)
            {
                self.request_avatar_replacement(request);
            }
        }
        #[cfg(not(windows))]
        {
            log::warn!("Open Avatar is only available on Windows");
        }
    }

    #[allow(dead_code)]
    pub(crate) fn latest_avatar_load_error(&self) -> Option<&AvatarRuntimeError> {
        match &self.avatar_load_status {
            AvatarLoadStatus::Error(error) => Some(error),
            AvatarLoadStatus::Idle | AvatarLoadStatus::Loading => None,
        }
    }

    /// Move a fully prepared candidate into the single owned scene slot.  The
    /// function deliberately has no `Result` boundary: candidate preparation,
    /// guest boot, GPU allocation, and semantic parsing all happened before
    /// this point.  The remaining operations are field moves and Vec slot
    /// replacement only.
    fn commit_avatar_candidate(&mut self, candidate: AvatarCandidate) {
        let replacing = self.active_avatar.is_some();
        let scene_slot = self
            .active_avatar
            .as_ref()
            .map(|active| active.scene_slot)
            .unwrap_or_else(|| AvatarSceneSlot::new(self.scene.models.len()));
        let (instance, active) = candidate.into_parts(scene_slot);
        debug_assert_eq!(active.presentation.transform, instance.transform);
        debug_assert_eq!(active.capabilities.clip_names.len(), active.clips.len());
        debug_assert_eq!(
            active.capabilities.idle_clip.is_some(),
            active.clips.iter().any(|(name, _)| name == "idle_loop")
        );

        if replacing {
            debug_assert!(scene_slot.is_valid(&self.scene));
            let _old_instance = scene_slot.replace(&mut self.scene, instance);
        } else {
            debug_assert_eq!(scene_slot.index(), self.scene.models.len());
            self.scene.models.push(instance);
        }

        self.active_avatar = Some(active);
        self.avatar_load_status.succeed();
        self.reapply_camera();
    }

    fn process_pending_avatar_request(&mut self, gpu: &Gpu, renderer: &Renderer) {
        let Some(request) = self.pending_avatar_request.take() else {
            return;
        };

        #[cfg(test)]
        {
            self.avatar_prepare_count += 1;
        }

        match AvatarCandidate::prepare(gpu, renderer, &self.cfg.bundle_path, &request) {
            Ok(candidate) => {
                // A missing slot is an invariant failure, not a reason to
                // guess which unrelated scene model should be replaced.
                if self
                    .active_avatar
                    .as_ref()
                    .is_some_and(|active| !active.scene_slot.is_valid(&self.scene))
                {
                    let error = anyhow::anyhow!("active avatar scene slot is no longer valid");
                    self.avatar_load_status.fail(AvatarRuntimeError::new(
                        AvatarLoadErrorKind::SceneUnavailable,
                        &error,
                    ));
                    return;
                }
                self.commit_avatar_candidate(candidate);
            }
            Err(error) => {
                // `candidate` does not exist on this path, so every GPU/guest
                // resource created during preparation is dropped here while
                // the old active state remains untouched.
                log::error!("avatar replacement failed: {error:#}");
                self.avatar_load_status.fail(error);
            }
        }
    }

    fn latch_menu_failure(&mut self, operation: &str, error: anyhow::Error) {
        let message = format!("{error:#}");
        if self.menu_health.latch(operation, &message) {
            let text_capture_was_active = self.menu_text_input_capture.active;
            self.clear_menu_pointer_buffer();
            if let Some(menu) = self.menu.as_mut() {
                menu.clear_text_input_state();
            }
            self.clear_menu_input_buffer();
            if text_capture_was_active {
                self.arm_camera_input_release_barrier();
            }
            // A disabled overlay is no longer visible, so release the same
            // temporary presentation constraint as an explicit close.
            self.close_settings();
            log::error!("menu {operation} failed; disabling overlay: {message}");
        }
    }

    fn clear_menu_pointer_buffer(&mut self) {
        self.menu_pointer_owned = false;
        self.pending_menu_press = None;
        self.pending_menu_pointer.clear();
        self.observed_menu_pointer = None;
    }

    fn clear_menu_input_buffer(&mut self) {
        self.menu_text_input_capture = MenuTextInputCapture::default();
        self.pending_menu_input.clear();
        self.last_menu_input_modifiers = None;
        self.camera_controls.suspend_keyboard_input();
    }

    fn arm_camera_input_release_barrier(&mut self) {
        self.camera_input_blocked = true;
        self.camera_controls.suspend_keyboard_input();
    }

    fn menu_input_modifiers(input: &Input) -> MenuInputModifiers {
        MenuInputModifiers {
            shift: input.key_down(KeyCode::ShiftLeft) || input.key_down(KeyCode::ShiftRight),
            control: input.key_down(KeyCode::ControlLeft) || input.key_down(KeyCode::ControlRight),
            alt: input.key_down(KeyCode::AltLeft) || input.key_down(KeyCode::AltRight),
            super_key: input.super_down(),
        }
    }

    fn buffer_menu_input(&mut self, input: &Input) {
        if !self.menu_health.is_healthy() {
            self.clear_menu_input_buffer();
            return;
        }

        let modifiers = Self::menu_input_modifiers(input);
        if input.interaction_cancelled() {
            // Drop edits/composition accumulated before focus loss. Keep only
            // the cancellation edge so the guest can clear its editor too.
            self.pending_menu_input.clear();
            self.menu_text_input_capture = MenuTextInputCapture::default();
            self.arm_camera_input_release_barrier();
            self.pending_menu_input.push(MenuInputFrame {
                edits: Vec::new(),
                ime: Vec::new(),
                modifiers,
                cancelled: true,
            });
            self.last_menu_input_modifiers = Some(modifiers);
            return;
        }

        let edits = input.edits();
        let ime = input.ime_events();
        if !edits.is_empty()
            || !ime.is_empty()
            || self.menu_text_input_capture.active
            || self.last_menu_input_modifiers != Some(modifiers)
        {
            self.pending_menu_input.push(MenuInputFrame {
                edits: edits.to_vec(),
                ime: ime.to_vec(),
                modifiers,
                cancelled: false,
            });
        }
        self.last_menu_input_modifiers = Some(modifiers);
    }

    fn take_pending_menu_input(&mut self) -> Vec<MenuInputFrame> {
        std::mem::take(&mut self.pending_menu_input)
    }

    pub(crate) fn menu_failure(&self) -> Option<&str> {
        self.menu_health.failure()
    }

    fn camera_viewport_aspect(&self) -> f32 {
        self.viewport_size
            .and_then(Camera::aspect_for_viewport)
            .or_else(|| Camera::aspect_for_viewport(self.cfg.size))
            .unwrap_or(DEFAULT_VIEWPORT_ASPECT)
    }

    fn apply_camera_settings(&mut self) {
        let Some(active) = self.active_avatar.as_ref() else {
            return;
        };
        let aabb = active.presentation_aabb;
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
        if let Some(active) = self.active_avatar.as_mut() {
            active.sim.look_base = self.camera.pos;
            active.sim.mouse_target = self.camera.pos;
        }
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

    fn menu_owns_pointer(&mut self, cursor: glam::Vec2) -> bool {
        if !self.menu_health.is_healthy() {
            return false;
        }
        let scale_factor = self.window_scale_factor;
        self.menu
            .as_mut()
            .is_some_and(|menu| menu.pointer_owns(cursor, scale_factor))
    }

    fn buffer_menu_pointer(&mut self, input: &Input) {
        let left = pocket3d::winit::event::MouseButton::Left;
        let cancelled = input.interaction_cancelled();
        let pressed_edge = !cancelled && input.mouse_button_pressed(left);
        let button_down = !cancelled && input.mouse_button_down(left);
        let cursor = input.cursor();

        // A terminal menu failure disables the guest for the rest of the
        // process. Do not keep recording desktop pointer motion for a menu
        // that can no longer consume it, while still preserving accepted
        // outside-menu presses for CharacterGuest.
        if !self.menu_health.is_healthy() {
            self.clear_menu_pointer_buffer();
            if cancelled {
                self.pending_character_clicks = 0;
            } else if pressed_edge {
                self.pending_character_clicks = self.pending_character_clicks.saturating_add(1);
            }
            return;
        }

        // Input::clear() is a cancellation boundary, not a normal release.
        // Drop a not-yet-consumed press/click here; the bridge receives an
        // explicit cancel frame below so the guest cannot activate it.
        if cancelled {
            self.pending_menu_press = None;
            self.pending_character_clicks = 0;
            self.menu_pointer_owned = false;
        }

        let press = if pressed_edge {
            let press = self.pending_menu_press.take().or_else(|| {
                cursor.map(|cursor| MenuPress {
                    cursor,
                    owned: self.menu_owns_pointer(cursor),
                })
            });
            let owned = press.is_some_and(|press| press.owned);
            self.menu_pointer_owned = owned;
            if !owned {
                self.pending_character_clicks = self.pending_character_clicks.saturating_add(1);
            }
            press.map(|press| press.cursor)
        } else {
            None
        };

        let pointer_state = (cursor, button_down);
        let pointer_changed = self
            .observed_menu_pointer
            .is_some_and(|previous| previous != pointer_state)
            || (self.observed_menu_pointer.is_none() && cursor.is_some());
        self.observed_menu_pointer = Some(pointer_state);

        if pressed_edge || pointer_changed || cancelled {
            self.pending_menu_pointer.push(MenuPointerFrame {
                cursor,
                press_cursor: press,
                pressed_edge,
                button_down,
                cancelled,
            });
        }

        if !button_down && !pressed_edge && !cancelled {
            self.menu_pointer_owned = false;
        }
    }

    fn menu_control_action(&self, action: MenuAction) -> ControlAction {
        match action {
            MenuAction::DistanceDecrement => ControlAction::AdjustDistance(-1),
            MenuAction::DistanceIncrement => ControlAction::AdjustDistance(1),
            MenuAction::FovDecrement => ControlAction::AdjustFov(-1),
            MenuAction::FovIncrement => ControlAction::AdjustFov(1),
            MenuAction::SetEffectiveDistance(value) => ControlAction::SetEffectiveDistance(value),
            MenuAction::SetEffectiveFov(value) => ControlAction::SetEffectiveFov(value),
            MenuAction::SetYaw(value) => ControlAction::SetYaw(value),
            MenuAction::SetPitch(value) => ControlAction::SetPitch(value),
            MenuAction::SetRoll(value) => ControlAction::SetRoll(value),
            MenuAction::SetHeadroom(value) => ControlAction::SetHeadroom(value),
            MenuAction::SetYawSnap(value) => ControlAction::SetYawSnap(value),
            MenuAction::SetPitchSnap(value) => ControlAction::SetPitchSnap(value),
            MenuAction::SetRollSnap(value) => ControlAction::SetRollSnap(value),
            MenuAction::SaveCamera => ControlAction::SaveCamera,
            MenuAction::ResetRuntimeCamera => ControlAction::ResetRuntimeCamera,
            MenuAction::RequestMsaa(preference) => ControlAction::RequestMsaa(preference),
            MenuAction::RequestSmaa(enabled) => ControlAction::RequestSmaa(enabled),
            MenuAction::SetMtoonRenderMode(mode) => ControlAction::SetMtoonRenderMode(mode),
            MenuAction::SetWindowWidth(value) => ControlAction::SetWindowWidth(value),
            MenuAction::SetWindowHeight(value) => ControlAction::SetWindowHeight(value),
            MenuAction::SetWindowResizable(value) => ControlAction::SetWindowResizable(value),
            MenuAction::SetWindowAlwaysOnTop(value) => ControlAction::SetWindowAlwaysOnTop(value),
            MenuAction::SetMaxFps(value) => ControlAction::SetMaxFps(value),
            MenuAction::SettingsOpened => ControlAction::SettingsOpened,
            MenuAction::SettingsClosed => ControlAction::SettingsClosed,
            MenuAction::RestoreDefaults => ControlAction::RestoreDefaults,
            MenuAction::OpenAvatar => unreachable!("OpenAvatar is handled before control actions"),
        }
    }

    /// Set process-local native window/pacing changes for Pocket3D to apply.
    /// This deliberately does not touch `AppSettings`; the WINDOW page
    /// persists its preference separately and uses this as its live request.
    #[allow(dead_code)]
    pub fn set_window_runtime_request(&mut self, request: WindowRuntimeRequest) {
        self.window_runtime_request = request;
    }

    #[allow(dead_code)]
    pub fn window_runtime_request(&self) -> WindowRuntimeRequest {
        self.window_runtime_request
    }

    #[allow(dead_code)]
    pub fn observed_window_runtime_state(&self) -> Option<WindowRuntimeState> {
        self.window_runtime_state
    }

    fn observed_logical_window_size(&self) -> Option<(u32, u32)> {
        self.window_physical_size.map(|size| {
            let logical = logical_viewport_for(size, self.window_scale_factor);
            (logical.0.round() as u32, logical.1.round() as u32)
        })
    }

    fn current_logical_window_size(&self) -> (u32, u32) {
        self.observed_logical_window_size()
            .unwrap_or((self.settings.window.width, self.settings.window.height))
    }

    fn window_size_for_edit(&self) -> (u32, u32) {
        self.pending_window_size_request
            .as_ref()
            .map(|pending| pending.requested_logical_size)
            .unwrap_or_else(|| self.current_logical_window_size())
    }

    fn constrain_size_for_settings(&self, size: (u32, u32)) -> (u32, u32) {
        if self.settings_visible {
            (
                size.0.max(SETTINGS_PRESENTATION_SIZE.0),
                size.1.max(SETTINGS_PRESENTATION_SIZE.1),
            )
        } else {
            size
        }
    }

    /// Queue a new logical size transaction. Callers choose whether to derive
    /// `size` from the pending target (dimension edits) or from an independent
    /// configured/observed source (lifecycle/default operations). Every new
    /// request still retains older unresolved targets because their native
    /// observations may arrive after they have been superseded.
    fn request_window_size(&mut self, size: (u32, u32)) {
        let requested_logical_size = self.constrain_size_for_settings(size);
        let (observation_baseline, observation_generation, mut superseded_logical_sizes) = self
            .pending_window_size_request
            .take()
            .map(|pending| {
                let mut superseded = pending.superseded_logical_sizes;
                if pending.requested_logical_size != requested_logical_size {
                    superseded.push(pending.requested_logical_size);
                }
                (
                    pending.observation_baseline,
                    pending.observation_generation,
                    superseded,
                )
            })
            .unwrap_or_else(|| {
                (
                    self.observed_logical_window_size(),
                    self.window_observation_generation,
                    Vec::new(),
                )
            });
        superseded_logical_sizes.dedup();

        let has_unresolved_request = self.observed_logical_window_size()
            != Some(requested_logical_size)
            || !superseded_logical_sizes.is_empty();
        self.pending_window_size_request =
            has_unresolved_request.then_some(PendingWindowSizeRequest {
                requested_logical_size,
                observation_baseline,
                observation_generation,
                superseded_logical_sizes,
            });
        self.window_runtime_request.inner_size = Some(requested_logical_size);
    }

    fn resolve_pending_window_size_request(&mut self) {
        let observed = self.observed_logical_window_size();
        let observation_generation = self.window_observation_generation;
        let Some(pending) = self.pending_window_size_request.as_mut() else {
            return;
        };
        if observation_generation <= pending.observation_generation {
            return;
        }

        let resolved = if observed == pending.observation_baseline {
            // Pocket3D reports the current observation every frame. Do not
            // consume another equal-sized historical request merely because
            // the same native observation was polled again.
            false
        } else if observed == Some(pending.requested_logical_size) {
            true
        } else if let Some(index) = pending
            .superseded_logical_sizes
            .iter()
            .position(|size| Some(*size) == observed)
        {
            // This observation belongs to an older request. Consume it and
            // make it the new unchanged baseline, but retain ownership of the
            // newest combined request.
            pending.superseded_logical_sizes.drain(..=index);
            pending.observation_baseline = observed;
            pending.observation_generation = observation_generation;
            false
        } else {
            observed != pending.observation_baseline
        };
        if resolved {
            self.pending_window_size_request = None;
        }
    }

    fn enforce_settings_presentation_size(&mut self) {
        if !self.settings_visible {
            return;
        }
        let observed = self.current_logical_window_size();
        let target = self.constrain_size_for_settings(observed);
        if target != observed {
            self.request_window_size(target);
        }
    }

    /// Settings is mounted by the app at startup and remains safe to interact
    /// with until the guest explicitly closes it. The accommodation is a live
    /// presentation constraint only; it never changes configured settings.
    fn open_settings(&mut self) {
        if self.settings_visible {
            return;
        }
        self.settings_visible = true;
        let current = self.current_logical_window_size();
        let target = self.constrain_size_for_settings(current);
        // Always supersede a possible close-time request, even when the
        // currently observed window is already large enough.
        self.request_window_size(target);
    }

    fn close_settings(&mut self) {
        if !self.settings_visible {
            return;
        }
        self.settings_visible = false;
        self.request_window_size((self.settings.window.width, self.settings.window.height));
    }

    fn apply_menu_action(&mut self, action: MenuAction) -> ControlsSnapshot {
        if matches!(action, MenuAction::OpenAvatar) {
            self.open_avatar_picker();
            self.controls_snapshot()
        } else {
            self.apply_control_action(self.menu_control_action(action))
        }
    }

    /// Persist the live optic values that have a representation in
    /// `AppSettings.camera`. Runtime pan and orientation remain session-only:
    /// `CameraSettings` does not contain corresponding fields.
    fn save_camera(&mut self) {
        let effective = self.effective_camera_values();
        let mut saved = self.settings.camera.sanitized();
        saved.fov_deg = effective.settings.fov_deg;
        saved.distance_scale = effective.settings.distance_scale;
        self.settings.camera = saved.sanitized();
        self.camera_controls.clear_fov_delta();
        self.camera_controls.clear_distance_delta();
        // The live camera already contains the effective optics. Clearing
        // these deltas leaves the same effective settings, so Save remains
        // visually inert without another camera application.
        self.persist_settings();
    }

    /// Return the live camera to the saved camera configuration and clear all
    /// session-only adjustments without persisting anything.
    fn reset_runtime_camera(&mut self) {
        self.camera_controls.reset_runtime_camera();
        self.reapply_camera();
    }

    /// Replace every persisted preference with Rust's canonical factory state
    /// only after its one coherent settings document is safely written.
    fn restore_defaults(&mut self) {
        let previous_mtoon_mode = self.settings.rendering.mtoon_render_mode;
        let defaults = AppSettings::default().sanitized();
        if !self.persist_settings_value(&defaults) {
            return;
        }

        self.settings = defaults.clone();
        self.camera_controls.reset_runtime_camera();
        self.reapply_camera();

        self.aa
            .request_msaa_samples(defaults.rendering.msaa.samples().unwrap_or(1));
        self.aa.request_smaa(defaults.rendering.smaa_enabled);

        self.request_window_size((defaults.window.width, defaults.window.height));
        self.window_runtime_request.resizable = Some(defaults.window.resizable);
        self.window_runtime_request.always_on_top = Some(defaults.window.always_on_top);
        self.window_runtime_request.max_fps = Some(Some(defaults.rendering.max_fps));
        self.cli_max_fps_active = false;
        if previous_mtoon_mode != defaults.rendering.mtoon_render_mode {
            self.switch_mtoon_render_mode(defaults.rendering.mtoon_render_mode);
        }
    }

    fn switch_mtoon_render_mode(&mut self, mode: crate::settings::MtoonRenderMode) {
        let started = std::time::Instant::now();
        if let Some(pending) = self.pending_avatar_request.as_mut() {
            pending.mtoon_render_mode = mode;
        }
        if let Some(active) = self.active_avatar.as_mut()
            && active.scene_slot.is_valid(&self.scene)
            && active.has_route(mode)
        {
            let instance = active.scene_slot.get_mut(&mut self.scene);
            instance.mtoon_draw_route = match mode {
                crate::settings::MtoonRenderMode::Auto => pocket3d::model::MtoonRenderMode::Auto,
                crate::settings::MtoonRenderMode::Native => {
                    pocket3d::model::MtoonRenderMode::Native
                }
                crate::settings::MtoonRenderMode::Fallback => {
                    pocket3d::model::MtoonRenderMode::Fallback
                }
            };
            active.request.mtoon_render_mode = mode;
            let native_count = if mode == crate::settings::MtoonRenderMode::Fallback {
                0
            } else {
                active.asset.native_mtoon_material_count
            };
            log::info!(
                "MToon rendering: {:?} (in-place, {:?}); materials: {} native / {} glTF fallback ({} native available)",
                mode,
                started.elapsed(),
                native_count,
                active.mtoon_declared_count.saturating_sub(native_count),
                active.asset.native_mtoon_material_count
            );
            return;
        }
        log::info!(
            "MToon rendering: {:?} (asset upgrade/reload requested)",
            mode
        );
        self.reload_current_avatar();
    }

    fn reload_current_avatar(&mut self) {
        let request = self
            .active_avatar
            .as_ref()
            .map(|active| active.request.clone())
            .or_else(|| self.pending_avatar_request.clone());
        if let Some(request) = request {
            self.request_avatar_replacement(request);
        }
    }

    pub(crate) fn apply_control_action(&mut self, action: ControlAction) -> ControlsSnapshot {
        match action {
            ControlAction::AdjustFov(direction) => {
                if self.camera_controls.adjust_fov_step(direction) {
                    self.reapply_camera();
                }
            }
            ControlAction::AdjustDistance(direction) => {
                if self.camera_controls.adjust_distance_step(direction) {
                    self.reapply_camera();
                }
            }
            ControlAction::SetEffectiveFov(value) => {
                self.camera_controls
                    .set_effective_fov_deg(value, self.settings.camera);
                self.reapply_camera();
            }
            ControlAction::SetEffectiveDistance(value) => {
                self.camera_controls
                    .set_effective_distance_scale(value, self.settings.camera);
                self.reapply_camera();
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
            ControlAction::SetHeadroom(headroom) => {
                let mut candidate = self.settings.camera;
                candidate.headroom = headroom;
                let sanitized = candidate.sanitized();
                if sanitized != self.settings.camera {
                    self.settings.camera = sanitized;
                    self.reapply_camera();
                    self.persist_settings();
                }
            }
            ControlAction::SaveCamera => self.save_camera(),
            ControlAction::ResetRuntimeCamera => self.reset_runtime_camera(),
            // Snap increments affect only future detents. Persist the
            // accepted value without reapplying the current camera pose.
            ControlAction::SetYawSnap(snap_deg) => {
                let mut candidate = self.settings.camera;
                candidate.yaw_snap_deg = snap_deg;
                let sanitized = candidate.sanitized();
                if sanitized != self.settings.camera {
                    self.settings.camera = sanitized;
                    self.persist_settings();
                }
            }
            ControlAction::SetPitchSnap(snap_deg) => {
                let mut candidate = self.settings.camera;
                candidate.pitch_snap_deg = snap_deg;
                let sanitized = candidate.sanitized();
                if sanitized != self.settings.camera {
                    self.settings.camera = sanitized;
                    self.persist_settings();
                }
            }
            ControlAction::SetRollSnap(snap_deg) => {
                let mut candidate = self.settings.camera;
                candidate.roll_snap_deg = snap_deg;
                let sanitized = candidate.sanitized();
                if sanitized != self.settings.camera {
                    self.settings.camera = sanitized;
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
            ControlAction::SetMtoonRenderMode(mode) => {
                if mode != self.settings.rendering.mtoon_render_mode {
                    self.settings.rendering.mtoon_render_mode = mode;
                    self.persist_settings();
                    self.switch_mtoon_render_mode(mode);
                }
            }
            ControlAction::SettingsOpened => self.open_settings(),
            ControlAction::SettingsClosed => self.close_settings(),
            ControlAction::RestoreDefaults => self.restore_defaults(),
            ControlAction::SetWindowWidth(value) => {
                let mut candidate = self.settings.window.clone();
                candidate.width = sanitized_window_dimension(value, candidate.width);
                let sanitized = candidate.sanitized();
                if sanitized != self.settings.window {
                    self.settings.window = sanitized;
                    self.persist_settings();
                }
                let (_, observed_height) = self.window_size_for_edit();
                self.request_window_size((self.settings.window.width, observed_height));
            }
            ControlAction::SetWindowHeight(value) => {
                let mut candidate = self.settings.window.clone();
                candidate.height = sanitized_window_dimension(value, candidate.height);
                let sanitized = candidate.sanitized();
                if sanitized != self.settings.window {
                    self.settings.window = sanitized;
                    self.persist_settings();
                }
                let (observed_width, _) = self.window_size_for_edit();
                self.request_window_size((observed_width, self.settings.window.height));
            }
            ControlAction::SetWindowResizable(value) => {
                let mut candidate = self.settings.window.clone();
                candidate.resizable = value;
                let sanitized = candidate.sanitized();
                if sanitized != self.settings.window {
                    self.settings.window = sanitized;
                    self.persist_settings();
                }
                self.window_runtime_request.resizable = Some(self.settings.window.resizable);
            }
            ControlAction::SetWindowAlwaysOnTop(value) => {
                let mut candidate = self.settings.window.clone();
                candidate.always_on_top = value;
                let sanitized = candidate.sanitized();
                if sanitized != self.settings.window {
                    self.settings.window = sanitized;
                    self.persist_settings();
                }
                self.window_runtime_request.always_on_top =
                    Some(self.settings.window.always_on_top);
            }
            ControlAction::SetMaxFps(value) => {
                let mut candidate = self.settings.rendering.clone();
                candidate.max_fps = value;
                let sanitized = candidate.sanitized();
                if sanitized != self.settings.rendering {
                    self.settings.rendering = sanitized;
                    self.persist_settings();
                }
                self.window_runtime_request.max_fps = Some(Some(self.settings.rendering.max_fps));
                // A valid WINDOW edit takes ownership of the current process's
                // pacing request. It does not alter future launches, where an
                // explicit --max-fps still wins during startup.
                self.cli_max_fps_active = false;
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
            base.headroom,
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

    fn observed_window_logical_size(&self) -> Option<(u32, u32)> {
        self.window_runtime_state.and_then(|state| {
            (state.inner_size_px.0 != 0 && state.inner_size_px.1 != 0).then(|| {
                let (width, height) = logical_viewport_for(state.inner_size_px, state.scale_factor);
                (width as u32, height as u32)
            })
        })
    }

    fn menu_window_state(&self) -> MenuWindowState {
        let observed = self.observed_window_logical_size();
        let cli_max_fps_override = self.cli_max_fps_override.filter(|override_fps| {
            self.cli_max_fps_active
                && self
                    .window_runtime_state
                    .is_some_and(|state| state.max_fps == Some(*override_fps))
        });

        MenuWindowState {
            configured_width: self.settings.window.width,
            configured_height: self.settings.window.height,
            configured_resizable: self.settings.window.resizable,
            configured_always_on_top: self.settings.window.always_on_top,
            configured_max_fps: self.settings.rendering.max_fps,
            current_width_logical: observed.map(|(width, _)| width),
            current_height_logical: observed.map(|(_, height)| height),
            applied_resizable: self.window_runtime_state.map(|state| state.resizable),
            applied_always_on_top: self.window_runtime_state.map(|state| state.always_on_top),
            effective_max_fps: self.window_runtime_state.and_then(|state| state.max_fps),
            cli_max_fps_override,
        }
    }

    fn persist_settings(&mut self) {
        let Some(path) = self.settings_path.clone() else {
            return;
        };
        let settings = self.settings.clone();
        let _ = self.persist_settings_value_at(&settings, &path);
    }

    /// Persist a candidate settings snapshot and report the result so callers
    /// that replace authoritative runtime state can fail closed.
    fn persist_settings_value(&mut self, settings: &AppSettings) -> bool {
        let Some(path) = self.settings_path.clone() else {
            log::warn!("unable to persist settings: no settings path is available");
            return false;
        };
        self.persist_settings_value_at(settings, &path)
    }

    fn persist_settings_value_at(
        &mut self,
        settings: &AppSettings,
        path: &std::path::Path,
    ) -> bool {
        let result = settings.save_to_path(path);
        if let Err(error) = &result {
            log::warn!(
                "unable to persist settings to {}: {error:#}",
                path.display()
            );
        }
        #[cfg(test)]
        {
            self.save_count += 1;
        }
        result.is_ok()
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
        let mut expressions_dirty = false;
        for cmd in commands {
            match cmd {
                Command::SetTracking(mode) => {
                    if let Some(active) = self.active_avatar.as_mut() {
                        set_tracking_mode(&mut active.sim, &mode);
                    }
                }
                Command::SetExpression(name, w) => {
                    let Some(active) = self.active_avatar.as_mut() else {
                        continue;
                    };
                    if matches!(&active.expressions, ResolvedExpressionRuntime::Vrm0Legacy) {
                        apply_expression(active, &mut self.scene, &name, w);
                    } else if let ResolvedExpressionRuntime::Vrm1(runtime) = &mut active.expressions
                    {
                        expressions_dirty |= runtime.set_input(&name, w);
                    }
                }
                Command::PlayClip { name, looping } => {
                    if let Some(active) = self.active_avatar.as_mut() {
                        if let Some(i) = active.clips.iter().position(|(n, _)| *n == name) {
                            active.animation.clip_index = i;
                            active.animation.clip_time = 0.0;
                            active.animation.clip_looping = looping;
                        } else {
                            log::warn!("character.playClip: unknown clip '{name}'");
                        }
                    }
                }
                Command::SetMaxFps(fps) => {
                    // Pacing remains authoritative in Pocket3D; this is a
                    // process-local desired value and is never persisted.
                    if fps.is_finite() {
                        self.window_runtime_request.max_fps = Some(Some(fps));
                        self.cli_max_fps_active = false;
                    } else {
                        log::warn!("character.setMaxFps: ignoring non-finite value");
                    }
                }
                Command::Quit => self.exit = true,
            }
        }
        if expressions_dirty {
            if let Some(active) = self.active_avatar.as_mut() {
                if let ResolvedExpressionRuntime::Vrm1(runtime) = &mut active.expressions {
                    runtime.compose_if_needed(&mut self.scene, active.scene_slot, self.last_blink);
                }
            }
        }
    }
}

/// Resolve a named VRM expression to morph weights on the instance.
fn apply_expression(active: &ActiveAvatar, scene: &mut Scene, name: &str, w: f32) {
    let Some(vrm) = active.document.vrm0() else {
        return;
    };
    let model = &active.asset;
    let inst = active.scene_slot.get_mut(scene);
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
    fn window_runtime_state(&mut self, state: WindowRuntimeState) {
        self.window_runtime_state = Some(state);
        self.window_physical_size = Some(state.inner_size_px);
        self.window_scale_factor = normalized_scale_factor(state.scale_factor);
        self.window_observation_generation = self.window_observation_generation.wrapping_add(1);
        self.resolve_pending_window_size_request();
        self.enforce_settings_presentation_size();
    }

    fn window_metrics(&mut self, physical_size: (u32, u32), scale_factor: f64) {
        self.window_physical_size = Some(physical_size);
        self.window_scale_factor = normalized_scale_factor(scale_factor);
        self.window_observation_generation = self.window_observation_generation.wrapping_add(1);
        self.resolve_pending_window_size_request();
        self.enforce_settings_presentation_size();
    }

    fn window_runtime_request(&self) -> WindowRuntimeRequest {
        self.window_runtime_request
    }

    fn text_input_request(&self) -> TextInputRequest {
        if !self.menu_health.is_healthy() {
            return TextInputRequest::default();
        }
        physical_text_input_request(self.menu_text_input_capture, self.window_scale_factor)
    }

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

        let mut startup_request =
            startup_avatar_request(self.cfg.model_path.clone(), self.cfg.vrma_path.clone());
        startup_request.mtoon_render_mode = self.settings.rendering.mtoon_render_mode;
        let candidate =
            AvatarCandidate::prepare(gpu, renderer, &self.cfg.bundle_path, &startup_request)?;

        // Scene: one instance, transparent background, near-unlit shading
        // (MToon reads mostly flat; sun/hemisphere would double-shade it).
        self.scene.transparent_clear = true;
        self.commit_avatar_candidate(candidate);

        // The menu guest boots independently of the character guest (separate
        // QuickJS realm + ui surface), after the GPU/renderer exist so the
        // overlay pipeline can bind the render target's color format.
        let (menu_bundle, menu_pak) = crate::menu_guest::load_menu_assets(
            &self.cfg.menu_bundle_path,
            &self.cfg.menu_pak_path,
        )?;
        let initial_physical_size = self.window_physical_size.unwrap_or(self.cfg.size);
        let initial_ui_viewport =
            logical_viewport_for(initial_physical_size, self.window_scale_factor);
        self.menu = Some(MenuGuest::boot(
            gpu,
            &menu_bundle,
            &menu_pak,
            initial_ui_viewport,
            renderer.color_format,
        )?);
        self.open_settings();

        log::info!("init: {:.0} ms", t0.elapsed().as_secs_f32() * 1000.0);
        Ok(())
    }

    fn frame(&mut self, dt: f32, input: &Input) {
        self.render_fps.record(dt);
        self.buffer_menu_pointer(input);
        self.buffer_menu_input(input);

        let text_input_active = self.menu_text_input_capture.active;
        let camera_key_held = camera::controls::camera_input_held(input);
        let keyboard_suppressed = if text_input_active {
            self.camera_input_blocked = true;
            self.camera_controls.suspend_keyboard_input();
            true
        } else if self.camera_input_blocked {
            self.camera_controls.suspend_keyboard_input();
            if !camera_key_held {
                self.camera_input_blocked = false;
            }
            true
        } else {
            false
        };

        if !keyboard_suppressed {
            if input.key_pressed(KeyCode::F3) {
                self.debug_hud_enabled = !self.debug_hud_enabled;
            }
            if input.key_pressed(KeyCode::F4) {
                let preference = self.aa.next_msaa_preference();
                self.apply_control_action(ControlAction::RequestMsaa(preference));
            }
            if input.key_pressed(KeyCode::F5) {
                let enabled = self.aa.next_smaa_enabled();
                self.apply_control_action(ControlAction::RequestSmaa(enabled));
            }
            // Temporary F8 validation controls are never written to
            // AppSettings.
            let pan_context = self.active_avatar.as_ref().map(|active| {
                CameraPanContext::new(active.presentation_aabb, self.settings.camera)
            });
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
        self.update_mouse_tracking_target(input.cursor());
    }

    fn tick(&mut self, dt: f32, _input: &Input) {
        let t0 = Instant::now();
        if self.active_avatar.is_none() {
            return;
        }
        self.tick_count += 1;

        // Keep the complete avatar tick inside one borrow.  The guest result
        // is owned before commands are applied, so a guest command cannot
        // observe partially updated avatar state.
        let guest_turn = {
            let active = self.active_avatar.as_mut().expect("active avatar checked");
            let scene_slot = active.scene_slot;
            let out = active.advance_pose(scene_slot.get_mut(&mut self.scene), dt);

            // --- guest turn ---------------------------------------------
            let mut events: Vec<TickEvent> = std::mem::take(&mut self.pending_events);
            for _ in 0..self.pending_character_clicks {
                events.push(TickEvent::Click);
            }
            self.pending_character_clicks = 0;
            let state = TickState {
                t: self.tick_count as f64 * dt as f64,
                blink: out.blink,
                clip: active
                    .clips
                    .get(active.animation.clip_index)
                    .map(|(n, _)| n.clone())
                    .unwrap_or_default(),
                hovered: self.hovered,
                tracking: match active.sim.tracking {
                    TrackingMode::None => "none",
                    TrackingMode::Mouse => "mouse",
                },
                fps: self.stats.fps(),
                frame_ms: self.stats.frame_ms(),
            };
            let blink = out.blink;
            (active.guest.turn(&state, &events), blink)
        };
        match guest_turn {
            (Ok(commands), blink) => {
                self.last_blink = blink;
                self.apply_commands(commands)
            }
            (Err(e), _) => log::error!("guest turn: {e:#}"),
        }

        self.stats.record(t0.elapsed().as_secs_f32() * 1000.0);

        // --- menu (PocketUI controls bridge) ------------------------------
        // Fixed ordering: queue the authoritative host snapshot and logical
        // pointer → guest frame/input → guest action send → host action drain
        // → apply_control_action → the fresh snapshot becomes authoritative
        // for the next tick. This is one fixed-tick reconciliation delay.
        if self.menu_health.is_healthy() {
            let snapshot = self.controls_snapshot();
            let window_snapshot = self.menu_window_state();
            let avatar_status = self.avatar_load_status.ui_status();
            let avatar_error = self.avatar_load_status.ui_error_message();
            let scale_factor = self.window_scale_factor;
            let pointer_frames = std::mem::take(&mut self.pending_menu_pointer);
            let input_frames = self.take_pending_menu_input();
            let result =
                self.menu
                    .as_mut()
                    .map(|menu| -> Result<(Vec<MenuAction>, MenuTextInputCapture)> {
                        menu.push_state(
                            snapshot.base_fov_deg(),
                            snapshot.base_distance_scale(),
                            snapshot.headroom(),
                            snapshot.effective_fov_deg(),
                            snapshot.effective_distance_scale(),
                            snapshot.yaw_deg(),
                            snapshot.pitch_deg(),
                            snapshot.roll_deg(),
                            snapshot.yaw_snap_deg(),
                            snapshot.pitch_snap_deg(),
                            snapshot.roll_snap_deg(),
                            snapshot.requested_msaa(),
                            snapshot.effective_msaa(),
                            snapshot.requested_smaa(),
                            self.settings.rendering.mtoon_render_mode,
                            snapshot.effective_smaa(),
                            snapshot.msaa_pending(),
                            snapshot.smaa_pending(),
                            avatar_status,
                            avatar_error,
                            window_snapshot,
                        )?;
                        for pointer_frame in pointer_frames {
                            if pointer_frame.cancelled {
                                menu.cancel_pointer();
                                continue;
                            }
                            if pointer_frame.pressed_edge {
                                menu.push_pointer_transition(
                                    pointer_frame.press_cursor.or(pointer_frame.cursor),
                                    scale_factor,
                                    true,
                                );
                                if !pointer_frame.button_down {
                                    menu.push_pointer_transition(
                                        pointer_frame.cursor.or(pointer_frame.press_cursor),
                                        scale_factor,
                                        false,
                                    );
                                }
                            } else {
                                menu.push_pointer_transition(
                                    pointer_frame.cursor,
                                    scale_factor,
                                    pointer_frame.button_down,
                                );
                            }
                        }
                        for input_frame in input_frames {
                            menu.push_input_frame(&input_frame);
                        }
                        menu.step()?;
                        let actions = menu.drain_actions();
                        Ok((actions, menu.text_input_capture()))
                    });
            match result {
                Some(Ok((actions, text_input_capture))) => {
                    self.menu_text_input_capture = text_input_capture;
                    for action in actions {
                        self.apply_menu_action(action);
                    }
                }
                Some(Err(error)) => self.latch_menu_failure("frame", error),
                None => {}
            }
        }
    }

    fn prepare_render(&mut self, gpu: &Gpu, renderer: &mut Renderer) {
        let requests = self.aa.take_pending_requests();
        if !requests.is_empty() {
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

        self.process_pending_avatar_request(gpu, renderer);
    }

    fn compose(&mut self, _alpha: f32, time: f32, size: (u32, u32)) -> (&Scene, &Camera, &Hud) {
        self.scene.time = time;
        self.update_viewport(size);
        // Desktop UI coordinate contract: Pocket3D surface/cursor coordinates
        // are physical px; MenuGuest's UiSurface/layout/hit-test coordinates
        // are logical px = physical px / scale; UiRenderer multiplies the
        // logical DrawList by that scale into the physical target. The
        // pointer bridge therefore uses full numeric logical svc coordinates,
        // never the packed 9-bit Guest::frame_with_touches representation.
        if self.menu_health.is_healthy() {
            let ui_viewport = logical_viewport_for(size, self.window_scale_factor);
            let result = self
                .menu
                .as_mut()
                .map(|menu| menu.set_viewport(ui_viewport.0, ui_viewport.1));
            if let Some(Err(error)) = result {
                self.latch_menu_failure("resize", error);
            }
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
        if self.menu_health.is_healthy() {
            let result = self.menu.as_mut().map(|menu| {
                menu.render(
                    gpu,
                    encoder,
                    view,
                    format,
                    size,
                    self.window_scale_factor as f32,
                )
            });
            if let Some(Err(error)) = result {
                self.latch_menu_failure("overlay", error);
            }
        }
    }

    fn wants_exit(&self) -> bool {
        self.exit
    }

    fn drag_at(&mut self, cursor: glam::Vec2) -> bool {
        let owned = self.menu_owns_pointer(cursor);
        self.menu_pointer_owned = owned;
        self.pending_menu_press = Some(MenuPress { cursor, owned });
        native_drag_allowed_for_menu_pointer(owned)
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
