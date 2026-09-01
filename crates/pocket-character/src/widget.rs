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

pub struct WidgetConfig {
    pub model_path: PathBuf,
    pub vrma_path: PathBuf,
    pub bundle_path: PathBuf,
    pub size: (u32, u32),
    /// Render N frames then exit (verification runs).
    pub frames: Option<u32>,
}

const MIN_MODEL_HEIGHT: f32 = 0.001;
const MAX_MODEL_BOUND: f32 = 100_000.0;
/// Rest-pose AABBs can omit a small animated excursion from hair/accessories.
const TOP_SAFETY_MARGIN: f32 = 0.02;
const DEFAULT_VIEWPORT_ASPECT: f32 = 0.75;
const MAX_RUNTIME_FOV_DELTA_DEG: f32 = 178.0;
const MAX_RUNTIME_DISTANCE_DELTA: f32 = 10.0;
const MAX_RUNTIME_PITCH_DEG: f32 = 89.0;

const CAMERA_FOV_RATE_DEG_PER_SEC: f32 = 45.0;
const CAMERA_DISTANCE_RATE_PER_SEC: f32 = 0.75;
/// Temporary keyboard tuning in rest-center NDC units per second. This
/// approximately preserves the previous manual pan feel for the default
/// character frame and is deliberately separate from the safety boundary.
const CAMERA_PAN_WITNESS_NDC_RATE_PER_SEC: f32 = 0.75;
/// UX safety envelope for the rest-bounds center. Values beyond one allow the
/// center to leave the physical viewport while retaining finite stoppers.
const PAN_WITNESS_SAFE_X_NDC: f32 = 1.8;
const PAN_WITNESS_SAFE_Y_NDC: f32 = 3.0;
/// When the authored zero-pan witness approaches or exceeds a nominal edge,
/// retain input travel on its outward side. Rolled X needs substantially more
/// room than Y without widening the ordinary zero-roll horizontal stopper.
const PAN_X_BASELINE_OUTWARD_SLACK_NDC: f32 = 1.5;
const PAN_Y_BASELINE_OUTWARD_SLACK_NDC: f32 = 0.5;
const PAN_WITNESS_DEPTH_EPSILON_RATIO: f32 = 1.0e-5;
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

#[derive(Clone, Copy, Debug, PartialEq)]
struct CameraFrame {
    target: Vec3,
    distance: f32,
    fov_y: f32,
    view_height: f32,
}

/// Runtime-only camera deltas used by the temporary F8 validation controls.
///
/// These deliberately live outside [`CameraSettings`]. Persisted settings
/// remain the framing baseline, while this state can be changed on the live
/// widget without changing the settings file or rebuilding any GPU object.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
struct CameraRuntimeAdjustments {
    fov_delta_deg: f32,
    distance_scale_delta: f32,
    /// Additional projected displacement of the zero-pan baseline target.
    /// Positive X is screen-right and positive Y is screen-up. Safety is
    /// enforced only when admitting new pan input; camera resolution never
    /// clamps or otherwise corrects this stored state.
    pan_ndc: Vec2,
    yaw_deg: f32,
    roll_deg: f32,
    pitch_deg: f32,
}

impl CameraRuntimeAdjustments {
    fn sanitized(self) -> Self {
        Self {
            fov_delta_deg: finite_clamped(
                self.fov_delta_deg,
                -MAX_RUNTIME_FOV_DELTA_DEG,
                MAX_RUNTIME_FOV_DELTA_DEG,
                0.0,
            ),
            distance_scale_delta: finite_clamped(
                self.distance_scale_delta,
                -MAX_RUNTIME_DISTANCE_DELTA,
                MAX_RUNTIME_DISTANCE_DELTA,
                0.0,
            ),
            pan_ndc: Vec2::new(
                finite_value(self.pan_ndc.x, 0.0),
                finite_value(self.pan_ndc.y, 0.0),
            ),
            yaw_deg: normalize_degrees(self.yaw_deg),
            roll_deg: normalize_degrees(self.roll_deg),
            pitch_deg: finite_clamped(
                self.pitch_deg,
                -MAX_RUNTIME_PITCH_DEG,
                MAX_RUNTIME_PITCH_DEG,
                0.0,
            ),
        }
    }

    fn effective(self, base: CameraSettings) -> EffectiveCameraValues {
        let base = base.sanitized();
        let adjustments = self.sanitized();
        let settings = CameraSettings {
            fov_deg: base.fov_deg + adjustments.fov_delta_deg,
            distance_scale: base.distance_scale + adjustments.distance_scale_delta,
            headroom: base.headroom,
        }
        .sanitized();

        EffectiveCameraValues {
            settings,
            pan_ndc: adjustments.pan_ndc,
            yaw_deg: adjustments.yaw_deg,
            roll_deg: adjustments.roll_deg,
            pitch_deg: adjustments.pitch_deg,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct EffectiveCameraValues {
    settings: CameraSettings,
    pan_ndc: Vec2,
    yaw_deg: f32,
    roll_deg: f32,
    pitch_deg: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct CameraParameters {
    frame: CameraFrame,
    /// The bounds-derived target before any runtime framing translation.
    ///
    /// This remains the baseline camera-composition target;
    /// `frame.target` is the final translated camera target.
    baseline_target: Vec3,
    /// Additional NDC displacement of `baseline_target`.
    pan_ndc: Vec2,
    yaw_deg: f32,
    roll_deg: f32,
    pitch_deg: f32,
    position: Vec3,
}

fn normalize_degrees(value: f32) -> f32 {
    if value.is_finite() {
        (value + 180.0).rem_euclid(360.0) - 180.0
    } else {
        0.0
    }
}

fn finite_clamped(value: f32, min: f32, max: f32, fallback: f32) -> f32 {
    finite_value(value, fallback).clamp(min, max)
}

fn finite_value(value: f32, fallback: f32) -> f32 {
    if value.is_finite() { value } else { fallback }
}

/// Resolve character-owned framing settings against model bounds.
///
/// The top of the model (including the safety margin) is placed at the
/// requested normalized headroom below the top of the vertical viewport.
fn resolve_camera_frame(aabb: (Vec3, Vec3), settings: CameraSettings) -> CameraFrame {
    let settings = settings.sanitized();
    let (min, max) = sanitize_model_aabb(aabb);
    let min_y = min.y.min(max.y);
    let max_y = min.y.max(max.y);
    let height = (max_y - min_y).clamp(MIN_MODEL_HEIGHT, MAX_MODEL_BOUND);
    let distance = height * settings.distance_scale;
    let fov_y = settings.fov_deg.to_radians();
    let view_height = 2.0 * distance * (fov_y * 0.5).tan();
    let framed_top = max_y + height * TOP_SAFETY_MARGIN;
    let target_y = framed_top - (0.5 - settings.headroom) * view_height;

    CameraFrame {
        target: Vec3::new((min.x + max.x) * 0.5, target_y, (min.z + max.z) * 0.5),
        distance,
        fov_y,
        view_height,
    }
}

fn valid_viewport_aspect(viewport_aspect: f32) -> f32 {
    if viewport_aspect.is_finite() && viewport_aspect > 0.0 {
        viewport_aspect
    } else {
        DEFAULT_VIEWPORT_ASPECT
    }
}

fn pan_witness_safe_limits(baseline_witness_ndc: Vec2) -> Vec2 {
    Vec2::new(PAN_WITNESS_SAFE_X_NDC, PAN_WITNESS_SAFE_Y_NDC).max(
        baseline_witness_ndc.abs()
            + Vec2::new(
                PAN_X_BASELINE_OUTWARD_SLACK_NDC,
                PAN_Y_BASELINE_OUTWARD_SLACK_NDC,
            ),
    )
}

/// World translation needed for one NDC unit at the authored target plane.
fn pan_world_per_ndc(distance: f32, fov_y: f32, viewport_aspect: f32) -> Vec2 {
    let aspect = valid_viewport_aspect(viewport_aspect);
    let half_y = fov_y * 0.5;
    let tan_y = half_y.tan();
    let half_x = (aspect * tan_y).atan();
    let world_per_ndc = Vec2::new(distance * half_x.tan(), distance * tan_y);

    if !distance.is_finite()
        || distance <= 0.0
        || !world_per_ndc.is_finite()
        || world_per_ndc.min_element() <= 0.0
    {
        return Vec2::ZERO;
    }

    world_per_ndc
}

fn aabb_is_finite((min, max): (Vec3, Vec3)) -> bool {
    min.is_finite() && max.is_finite()
}

fn sanitize_model_aabb((min, max): (Vec3, Vec3)) -> (Vec3, Vec3) {
    let min = Vec3::new(
        finite_clamped(min.x, -MAX_MODEL_BOUND, MAX_MODEL_BOUND, 0.0),
        finite_clamped(min.y, -MAX_MODEL_BOUND, MAX_MODEL_BOUND, 0.0),
        finite_clamped(min.z, -MAX_MODEL_BOUND, MAX_MODEL_BOUND, 0.0),
    );
    let max = Vec3::new(
        finite_clamped(max.x, -MAX_MODEL_BOUND, MAX_MODEL_BOUND, 0.0),
        finite_clamped(max.y, -MAX_MODEL_BOUND, MAX_MODEL_BOUND, 0.0),
        finite_clamped(max.z, -MAX_MODEL_BOUND, MAX_MODEL_BOUND, 0.0),
    );
    (min, max)
}

/// Resolve camera state in this order:
///
/// 1. Derive the baseline bounds/headroom target.
/// 2. Apply runtime yaw, pitch, and roll to the baseline orbit orientation.
/// 3. Derive screen-right and screen-up from that final orientation.
/// 4. Translate both camera position and target by the stored NDC pan.
///
/// The camera basis is therefore never derived from a partially panned frame.
/// The same path is used by live input and by re-framing after a resize.
fn resolve_camera_parameters_with_aspect(
    aabb: (Vec3, Vec3),
    base_settings: CameraSettings,
    adjustments: CameraRuntimeAdjustments,
    viewport_aspect: f32,
) -> CameraParameters {
    let base_settings = base_settings.sanitized();
    let effective = adjustments.effective(base_settings);
    let authored_frame = resolve_camera_frame(aabb, base_settings);
    let effective_distance = authored_frame.distance
        * (effective.settings.distance_scale / base_settings.distance_scale);
    let effective_fov_y = effective.settings.fov_deg.to_radians();
    let baseline_frame = CameraFrame {
        target: authored_frame.target,
        distance: effective_distance,
        fov_y: effective_fov_y,
        view_height: 2.0 * effective_distance * (effective_fov_y * 0.5).tan(),
    };
    let baseline_target = baseline_frame.target;
    let base_position = baseline_target + Vec3::new(0.0, 0.0, -baseline_frame.distance);
    let mut base_camera = Camera::default();
    base_camera.fov_y = baseline_frame.fov_y;
    base_camera.znear = 0.05;
    base_camera.pos = base_position;
    base_camera.look_at(baseline_target);

    // Runtime yaw/pitch are offsets from the already-derived bounds-aware
    // camera pose. The unpanned orbit remains composed around baseline_target.
    let yaw_deg = base_camera.yaw.to_degrees() + effective.yaw_deg;
    let roll_deg = effective.roll_deg;
    let pitch_deg = base_camera.pitch.to_degrees() + effective.pitch_deg;
    let mut orientation = base_camera;
    orientation.yaw = yaw_deg.to_radians();
    orientation.roll = roll_deg.to_radians();
    orientation.pitch = pitch_deg.to_radians();
    orientation.pos = baseline_target - orientation.forward() * baseline_frame.distance;

    let distance = (baseline_target - orientation.pos).length();
    let pan_ndc = effective.pan_ndc;
    let world_pan = pan_ndc * pan_world_per_ndc(distance, baseline_frame.fov_y, viewport_aspect);
    let screen_right = orientation.screen_right();
    let screen_up = orientation.screen_up();
    let requested_translation = -screen_right * world_pan.x - screen_up * world_pan.y;
    let translation = if requested_translation.is_finite() {
        requested_translation
    } else {
        Vec3::ZERO
    };
    let position = orientation.pos + translation;
    let mut frame = baseline_frame;
    frame.target = baseline_target + translation;

    CameraParameters {
        frame,
        baseline_target,
        pan_ndc,
        yaw_deg,
        roll_deg,
        pitch_deg,
        position,
    }
}

fn camera_for_parameters(parameters: CameraParameters) -> Camera {
    let mut camera = Camera::default();
    camera.pos = parameters.position;
    camera.yaw = parameters.yaw_deg.to_radians();
    camera.roll = parameters.roll_deg.to_radians();
    camera.pitch = parameters.pitch_deg.to_radians();
    camera.fov_y = parameters.frame.fov_y;
    camera.znear = 0.05;
    camera
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PanWitnessProjection {
    baseline_ndc: Vec2,
    response: f32,
}

/// Project the sanitized rest-bounds center through the current zero-pan
/// camera. The rest center is only an input-safety witness; it says nothing
/// about the current GPU-skinned silhouette.
fn resolve_pan_witness_projection(
    aabb: (Vec3, Vec3),
    base_settings: CameraSettings,
    adjustments: CameraRuntimeAdjustments,
    viewport_aspect: f32,
) -> Option<PanWitnessProjection> {
    if !aabb_is_finite(aabb) {
        return None;
    }

    let aspect = valid_viewport_aspect(viewport_aspect);
    let mut zero_pan_adjustments = adjustments.sanitized();
    zero_pan_adjustments.pan_ndc = Vec2::ZERO;
    let zero_pan =
        resolve_camera_parameters_with_aspect(aabb, base_settings, zero_pan_adjustments, aspect);
    let camera = camera_for_parameters(zero_pan);
    let (min, max) = sanitize_model_aabb(aabb);
    let witness = (min + max) * 0.5;
    let distance = (zero_pan.baseline_target - zero_pan.position).length();
    let depth = (witness - zero_pan.position).dot(camera.forward());
    let depth_epsilon = distance * PAN_WITNESS_DEPTH_EPSILON_RATIO;
    if !distance.is_finite() || distance <= 0.0 || !depth.is_finite() || depth <= depth_epsilon {
        return None;
    }

    let baseline_ndc = camera.view_proj(aspect).project_point3(witness).truncate();
    let response = distance / depth;
    if !baseline_ndc.is_finite() || !response.is_finite() || response <= 0.0 {
        return None;
    }

    Some(PanWitnessProjection {
        baseline_ndc,
        response,
    })
}

fn admit_pan_axis(
    current_pan_ndc: f32,
    desired_witness_delta_ndc: f32,
    baseline_witness_ndc: f32,
    response: f32,
    safe_limit_ndc: f32,
) -> f32 {
    if desired_witness_delta_ndc == 0.0 {
        return current_pan_ndc;
    }
    if !current_pan_ndc.is_finite()
        || !desired_witness_delta_ndc.is_finite()
        || !baseline_witness_ndc.is_finite()
        || !response.is_finite()
        || response <= 0.0
        || !safe_limit_ndc.is_finite()
        || safe_limit_ndc <= 0.0
    {
        return current_pan_ndc;
    }

    let current_witness_ndc = baseline_witness_ndc + response * current_pan_ndc;
    let requested_witness_ndc = current_witness_ndc + desired_witness_delta_ndc;
    if !current_witness_ndc.is_finite() || !requested_witness_ndc.is_finite() {
        return current_pan_ndc;
    }

    let (min_next, max_next) = if current_witness_ndc > safe_limit_ndc {
        (-safe_limit_ndc, current_witness_ndc)
    } else if current_witness_ndc < -safe_limit_ndc {
        (current_witness_ndc, safe_limit_ndc)
    } else {
        (-safe_limit_ndc, safe_limit_ndc)
    };
    let next_witness_ndc = requested_witness_ndc.clamp(min_next, max_next);
    let next_pan_ndc = (next_witness_ndc - baseline_witness_ndc) / response;
    if next_pan_ndc.is_finite() {
        next_pan_ndc
    } else {
        current_pan_ndc
    }
}

/// Admit only the newly requested witness-space movement. Existing stored pan
/// is grandfathered across every camera reframe and orientation change.
fn admit_pan_input(
    aabb: (Vec3, Vec3),
    base_settings: CameraSettings,
    adjustments: CameraRuntimeAdjustments,
    viewport_aspect: f32,
    desired_witness_delta_ndc: Vec2,
) -> Vec2 {
    let current = adjustments.sanitized().pan_ndc;
    if desired_witness_delta_ndc == Vec2::ZERO {
        return current;
    }
    let Some(projection) =
        resolve_pan_witness_projection(aabb, base_settings, adjustments, viewport_aspect)
    else {
        return current;
    };
    let safe_limits = pan_witness_safe_limits(projection.baseline_ndc);

    Vec2::new(
        admit_pan_axis(
            current.x,
            desired_witness_delta_ndc.x,
            projection.baseline_ndc.x,
            projection.response,
            safe_limits.x,
        ),
        admit_pan_axis(
            current.y,
            desired_witness_delta_ndc.y,
            projection.baseline_ndc.y,
            projection.response,
            safe_limits.y,
        ),
    )
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

    fn approx_vec3(actual: Vec3, expected: Vec3) {
        assert!(
            (actual - expected).abs().max_element() < 1.0e-5,
            "{actual:?} != {expected:?}"
        );
    }

    fn approx_vec2(actual: Vec2, expected: Vec2) {
        assert!(
            (actual - expected).abs().max_element() < 1.0e-4,
            "{actual:?} != {expected:?}"
        );
    }

    fn projected_point(parameters: CameraParameters, point: Vec3, aspect: f32) -> Vec3 {
        camera_for_parameters(parameters)
            .view_proj(aspect)
            .project_point3(point)
    }

    fn in_range_pan_test_settings() -> CameraSettings {
        CameraSettings {
            fov_deg: 55.0,
            distance_scale: 0.8,
            headroom: 0.17,
        }
    }

    fn outside_pan_test_settings() -> CameraSettings {
        CameraSettings {
            fov_deg: 40.0,
            distance_scale: 0.3,
            headroom: 0.49,
        }
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
        let short = resolve_camera_frame((Vec3::ZERO, Vec3::new(1.0, 1.0, 1.0)), settings);
        let tall = resolve_camera_frame((Vec3::ZERO, Vec3::new(1.0, 2.0, 1.0)), settings);

        approx_eq(tall.distance / short.distance, 2.0);
        approx_eq(
            tall.view_height / tall.distance,
            short.view_height / short.distance,
        );
    }

    #[test]
    fn default_runtime_adjustments_preserve_pre_adjustment_camera() {
        let aabb = (Vec3::new(-0.4, 0.0, -0.2), Vec3::new(0.6, 1.8, 0.4));
        let settings = CameraSettings::default();
        let base_frame = resolve_camera_frame(aabb, settings);
        let mut pre_f8_camera = Camera::default();
        pre_f8_camera.fov_y = base_frame.fov_y;
        pre_f8_camera.znear = 0.05;
        pre_f8_camera.pos = base_frame.target + Vec3::new(0.0, 0.0, -base_frame.distance);
        pre_f8_camera.look_at(base_frame.target);

        let parameters = resolve_camera_parameters_with_aspect(
            aabb,
            settings,
            CameraRuntimeAdjustments::default(),
            DEFAULT_VIEWPORT_ASPECT,
        );
        approx_vec3(parameters.frame.target, base_frame.target);
        approx_eq(parameters.frame.distance, base_frame.distance);
        approx_eq(parameters.frame.fov_y, pre_f8_camera.fov_y);
        approx_eq(parameters.frame.view_height, base_frame.view_height);
        approx_vec3(parameters.position, pre_f8_camera.pos);
        approx_eq(parameters.yaw_deg, pre_f8_camera.yaw.to_degrees());
        approx_eq(parameters.pitch_deg, pre_f8_camera.pitch.to_degrees());
    }

    #[test]
    fn runtime_fov_changes_only_effective_lens_at_zero_pan() {
        let aabb = standard_pan_aabb();
        let settings = in_range_pan_test_settings();
        let pose = CameraRuntimeAdjustments {
            yaw_deg: 31.0,
            roll_deg: 17.0,
            pitch_deg: -12.0,
            ..Default::default()
        };
        let baseline =
            resolve_camera_parameters_with_aspect(aabb, settings, pose, DEFAULT_VIEWPORT_ASPECT);
        let changed = resolve_camera_parameters_with_aspect(
            aabb,
            settings,
            CameraRuntimeAdjustments {
                fov_delta_deg: 12.0,
                ..pose
            },
            DEFAULT_VIEWPORT_ASPECT,
        );

        assert_ne!(changed.frame.fov_y, baseline.frame.fov_y);
        approx_vec3(changed.baseline_target, baseline.baseline_target);
        approx_vec3(changed.frame.target, baseline.frame.target);
        approx_vec3(changed.position, baseline.position);
        approx_eq(changed.frame.distance, baseline.frame.distance);
        approx_eq(changed.yaw_deg, baseline.yaw_deg);
        approx_eq(changed.roll_deg, baseline.roll_deg);
        approx_eq(changed.pitch_deg, baseline.pitch_deg);
    }

    #[test]
    fn runtime_distance_changes_only_orbit_radius_at_zero_pan() {
        let aabb = standard_pan_aabb();
        let settings = in_range_pan_test_settings();
        let pose = CameraRuntimeAdjustments {
            yaw_deg: 31.0,
            roll_deg: 17.0,
            pitch_deg: -12.0,
            ..Default::default()
        };
        let baseline =
            resolve_camera_parameters_with_aspect(aabb, settings, pose, DEFAULT_VIEWPORT_ASPECT);
        let changed = resolve_camera_parameters_with_aspect(
            aabb,
            settings,
            CameraRuntimeAdjustments {
                distance_scale_delta: 0.25,
                ..pose
            },
            DEFAULT_VIEWPORT_ASPECT,
        );

        assert_ne!(changed.frame.distance, baseline.frame.distance);
        approx_vec3(changed.baseline_target, baseline.baseline_target);
        approx_vec3(changed.frame.target, baseline.frame.target);
        approx_eq(changed.frame.fov_y, baseline.frame.fov_y);
        approx_eq(changed.yaw_deg, baseline.yaw_deg);
        approx_eq(changed.roll_deg, baseline.roll_deg);
        approx_eq(changed.pitch_deg, baseline.pitch_deg);

        let forward = camera_for_parameters(baseline).forward();
        approx_vec3(
            changed.position,
            changed.baseline_target - forward * changed.frame.distance,
        );
        approx_vec3(
            changed.position - baseline.position,
            -forward * (changed.frame.distance - baseline.frame.distance),
        );
    }

    #[test]
    fn combined_runtime_optics_preserve_authored_baseline_target() {
        let aabb = standard_pan_aabb();
        let settings = in_range_pan_test_settings();
        let baseline = resolve_camera_parameters_with_aspect(
            aabb,
            settings,
            CameraRuntimeAdjustments::default(),
            DEFAULT_VIEWPORT_ASPECT,
        );
        let changed = resolve_camera_parameters_with_aspect(
            aabb,
            settings,
            CameraRuntimeAdjustments {
                fov_delta_deg: 18.0,
                distance_scale_delta: 0.35,
                ..Default::default()
            },
            DEFAULT_VIEWPORT_ASPECT,
        );

        approx_vec3(changed.baseline_target, baseline.baseline_target);
        approx_vec3(changed.frame.target, baseline.frame.target);
        assert_ne!(changed.frame.fov_y, baseline.frame.fov_y);
        assert_ne!(changed.frame.distance, baseline.frame.distance);
    }

    #[test]
    fn persisted_camera_optics_and_headroom_recompute_authored_target() {
        let aabb = standard_pan_aabb();
        let base_settings = in_range_pan_test_settings();
        let baseline = resolve_camera_parameters_with_aspect(
            aabb,
            base_settings,
            CameraRuntimeAdjustments::default(),
            DEFAULT_VIEWPORT_ASPECT,
        );
        let variants = [
            CameraSettings {
                fov_deg: base_settings.fov_deg + 10.0,
                ..base_settings
            },
            CameraSettings {
                distance_scale: base_settings.distance_scale + 0.2,
                ..base_settings
            },
            CameraSettings {
                headroom: base_settings.headroom + 0.08,
                ..base_settings
            },
        ];

        for settings in variants {
            let authored = resolve_camera_frame(aabb, settings);
            let resolved = resolve_camera_parameters_with_aspect(
                aabb,
                settings,
                CameraRuntimeAdjustments::default(),
                DEFAULT_VIEWPORT_ASPECT,
            );

            approx_vec3(resolved.baseline_target, authored.target);
            approx_vec3(resolved.frame.target, authored.target);
            approx_eq(resolved.frame.distance, authored.distance);
            approx_eq(resolved.frame.fov_y, authored.fov_y);
            assert!((resolved.baseline_target - baseline.baseline_target).length() > 1.0e-4);
        }
    }

    #[test]
    fn runtime_optics_preserve_stored_pan_and_projected_ndc_displacement() {
        let aabb = standard_pan_aabb();
        let settings = in_range_pan_test_settings();
        let aspect = 1.37;
        let requested_pan = Vec2::new(0.37, -0.61);
        let pose = CameraRuntimeAdjustments {
            yaw_deg: 31.0,
            roll_deg: 17.0,
            pitch_deg: -12.0,
            ..Default::default()
        };
        let baseline_zero = resolve_camera_parameters_with_aspect(aabb, settings, pose, aspect);
        let baseline_panned = resolve_camera_parameters_with_aspect(
            aabb,
            settings,
            CameraRuntimeAdjustments {
                pan_ndc: requested_pan,
                ..pose
            },
            aspect,
        );
        let changed_pose = CameraRuntimeAdjustments {
            fov_delta_deg: 18.0,
            distance_scale_delta: 0.35,
            ..pose
        };
        let changed_zero =
            resolve_camera_parameters_with_aspect(aabb, settings, changed_pose, aspect);
        let changed_panned = resolve_camera_parameters_with_aspect(
            aabb,
            settings,
            CameraRuntimeAdjustments {
                pan_ndc: requested_pan,
                ..changed_pose
            },
            aspect,
        );

        assert_eq!(baseline_panned.pan_ndc, requested_pan);
        assert_eq!(changed_panned.pan_ndc, requested_pan);
        approx_vec2(
            projected_baseline_delta(baseline_zero, baseline_panned, aspect),
            requested_pan,
        );
        approx_vec2(
            projected_baseline_delta(changed_zero, changed_panned, aspect),
            requested_pan,
        );
        approx_vec3(changed_zero.baseline_target, baseline_zero.baseline_target);

        let baseline_world_pan = baseline_panned.position - baseline_zero.position;
        let changed_world_pan = changed_panned.position - changed_zero.position;
        assert!((changed_world_pan - baseline_world_pan).length() > 1.0e-4);
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
    fn runtime_camera_parameters_apply_on_top_of_bounds_frame() {
        let aabb = (Vec3::new(-0.4, 0.0, -0.2), Vec3::new(0.6, 1.8, 0.4));
        let settings = in_range_pan_test_settings();
        let adjustments = CameraRuntimeAdjustments {
            fov_delta_deg: -5.0,
            distance_scale_delta: 0.2,
            pan_ndc: Vec2::new(0.0, 0.02),
            yaw_deg: 30.0,
            roll_deg: 15.0,
            pitch_deg: -20.0,
        };
        let parameters = resolve_camera_parameters_with_aspect(
            aabb,
            settings,
            adjustments,
            DEFAULT_VIEWPORT_ASPECT,
        );
        let effective = adjustments.effective(settings);
        let base = resolve_camera_parameters_with_aspect(
            aabb,
            settings,
            CameraRuntimeAdjustments::default(),
            DEFAULT_VIEWPORT_ASPECT,
        );

        assert_eq!(effective.settings.fov_deg, 50.0);
        assert_eq!(effective.settings.distance_scale, 1.0);
        assert_eq!(effective.settings.headroom, 0.17);
        assert_eq!(effective.pan_ndc, Vec2::new(0.0, 0.02));
        assert_eq!(effective.roll_deg, 15.0);
        approx_eq(parameters.yaw_deg, base.yaw_deg + 30.0);
        approx_eq(parameters.roll_deg, 15.0);
        approx_eq(parameters.pitch_deg, base.pitch_deg - 20.0);
        approx_eq(parameters.frame.fov_y, 50.0_f32.to_radians());
        let mut orientation = Camera::default();
        orientation.yaw = parameters.yaw_deg.to_radians();
        orientation.roll = parameters.roll_deg.to_radians();
        orientation.pitch = parameters.pitch_deg.to_radians();
        let authored_frame = resolve_camera_frame(aabb, settings);
        let expected_target = authored_frame.target;
        approx_vec3(parameters.baseline_target, expected_target);
        approx_eq(
            parameters.frame.distance,
            authored_frame.distance * (effective.settings.distance_scale / settings.distance_scale),
        );
        let world_pan_y =
            parameters.pan_ndc.y * parameters.frame.distance * (parameters.frame.fov_y * 0.5).tan();
        approx_vec3(
            parameters.frame.target,
            expected_target - orientation.screen_up() * world_pan_y,
        );
        approx_eq(parameters.pan_ndc.y, 0.02);

        let expected_position =
            parameters.frame.target - orientation.forward() * parameters.frame.distance;
        approx_vec3(parameters.position, expected_position);
    }

    #[test]
    fn pan_witness_is_the_sanitized_rest_bounds_center() {
        let aabb = (
            Vec3::new(-2.0, f32::NAN, -4.0),
            Vec3::new(4.0, 1.8, f32::INFINITY),
        );
        let (min, max) = sanitize_model_aabb(aabb);
        let center = (min + max) * 0.5;

        approx_vec3(center, Vec3::new(1.0, 0.9, -2.0));
    }

    #[test]
    fn runtime_camera_values_are_finite_and_fit_controls_are_clamped() {
        let effective = CameraRuntimeAdjustments {
            fov_delta_deg: f32::INFINITY,
            distance_scale_delta: f32::NEG_INFINITY,
            pan_ndc: Vec2::splat(f32::NAN),
            yaw_deg: 999.0,
            roll_deg: f32::NAN,
            pitch_deg: -999.0,
        }
        .effective(CameraSettings::default());

        assert_eq!(effective.settings.fov_deg, 40.0);
        assert_eq!(effective.settings.distance_scale, 0.6);
        assert_eq!(effective.settings.headroom, 0.05);
        assert_eq!(effective.pan_ndc, Vec2::ZERO);
        assert_eq!(effective.roll_deg, 0.0);
        assert_eq!(effective.yaw_deg, -81.0);
        assert_eq!(effective.pitch_deg, -89.0);

        let clamped = CameraRuntimeAdjustments {
            fov_delta_deg: 999.0,
            distance_scale_delta: 999.0,
            pan_ndc: Vec2::splat(999.0),
            yaw_deg: 999.0,
            roll_deg: 999.0,
            pitch_deg: -999.0,
        }
        .effective(CameraSettings::default());
        assert_eq!(clamped.settings.fov_deg, 179.0);
        assert_eq!(clamped.settings.distance_scale, 10.0);
        assert_eq!(clamped.settings.headroom, 0.05);
        assert_eq!(clamped.pan_ndc, Vec2::splat(999.0));
        assert_eq!(clamped.roll_deg, -81.0);
        assert_eq!(clamped.yaw_deg, -81.0);
        assert_eq!(clamped.pitch_deg, -89.0);

        let parameters = resolve_camera_parameters_with_aspect(
            (Vec3::splat(f32::NAN), Vec3::splat(f32::INFINITY)),
            CameraSettings::default(),
            CameraRuntimeAdjustments {
                pan_ndc: Vec2::splat(f32::INFINITY),
                ..CameraRuntimeAdjustments::default()
            },
            DEFAULT_VIEWPORT_ASPECT,
        );
        assert!(
            parameters
                .position
                .to_array()
                .iter()
                .all(|value| value.is_finite())
        );
        assert!(parameters.frame.target.y.is_finite());
        assert!(parameters.pan_ndc.is_finite());
    }

    fn standard_pan_aabb() -> (Vec3, Vec3) {
        (Vec3::new(-0.4, 0.0, -0.2), Vec3::new(0.6, 1.8, 0.4))
    }

    fn rest_bounds_center(aabb: (Vec3, Vec3)) -> Vec3 {
        let (min, max) = sanitize_model_aabb(aabb);
        (min + max) * 0.5
    }

    fn projected_rest_center(
        parameters: CameraParameters,
        aabb: (Vec3, Vec3),
        aspect: f32,
    ) -> Vec2 {
        projected_point(parameters, rest_bounds_center(aabb), aspect).truncate()
    }

    fn projected_baseline_delta(
        zero_pan: CameraParameters,
        panned: CameraParameters,
        aspect: f32,
    ) -> Vec2 {
        let witness = zero_pan.baseline_target;
        let zero = projected_point(zero_pan, witness, aspect);
        let pan = projected_point(panned, witness, aspect);
        (pan - zero).truncate()
    }

    fn adjustments_after_pan_input(
        aabb: (Vec3, Vec3),
        settings: CameraSettings,
        mut adjustments: CameraRuntimeAdjustments,
        aspect: f32,
        desired_witness_delta: Vec2,
    ) -> CameraRuntimeAdjustments {
        adjustments.pan_ndc =
            admit_pan_input(aabb, settings, adjustments, aspect, desired_witness_delta);
        adjustments
    }

    #[test]
    fn zero_ndc_pan_is_exact_zero_pan_camera_for_any_orientation() {
        let aabb = standard_pan_aabb();
        let pose = CameraRuntimeAdjustments {
            yaw_deg: 47.0,
            pitch_deg: -12.0,
            roll_deg: 91.0,
            pan_ndc: Vec2::ZERO,
            ..Default::default()
        };
        let parameters = resolve_camera_parameters_with_aspect(
            aabb,
            in_range_pan_test_settings(),
            pose,
            DEFAULT_VIEWPORT_ASPECT,
        );

        approx_vec3(parameters.frame.target, parameters.baseline_target);
        assert_eq!(parameters.pan_ndc, Vec2::ZERO);
        let mut orientation = camera_for_parameters(parameters);
        orientation.pos =
            parameters.baseline_target - orientation.forward() * parameters.frame.distance;
        approx_vec3(parameters.position, orientation.pos);
    }

    #[test]
    fn resolver_preserves_stored_pan_and_exact_target_projection() {
        let aabb = standard_pan_aabb();
        let aspect = 1.37;
        let pose = CameraRuntimeAdjustments {
            yaw_deg: 47.0,
            pitch_deg: -12.0,
            roll_deg: 31.0,
            ..Default::default()
        };
        let zero =
            resolve_camera_parameters_with_aspect(aabb, in_range_pan_test_settings(), pose, aspect);
        let requested_pan = Vec2::new(0.37, -0.61);
        let panned = resolve_camera_parameters_with_aspect(
            aabb,
            in_range_pan_test_settings(),
            CameraRuntimeAdjustments {
                pan_ndc: requested_pan,
                ..pose
            },
            aspect,
        );

        assert_eq!(panned.pan_ndc, requested_pan);
        approx_vec2(
            projected_baseline_delta(zero, panned, aspect),
            requested_pan,
        );
    }

    #[test]
    fn baseline_inside_region_has_configured_rest_center_stoppers() {
        let aabb = standard_pan_aabb();
        let settings = in_range_pan_test_settings();
        let aspect = 1.2;
        let zero = resolve_camera_parameters_with_aspect(
            aabb,
            settings,
            CameraRuntimeAdjustments::default(),
            aspect,
        );
        let baseline = projected_rest_center(zero, aabb, aspect);
        let safe_limits = pan_witness_safe_limits(baseline);
        assert!(baseline.abs().cmplt(Vec2::ONE).all(), "{baseline:?}");
        approx_eq(safe_limits.y, PAN_WITNESS_SAFE_Y_NDC);

        for (request, expected) in [
            (Vec2::new(10.0, 0.0), Vec2::new(safe_limits.x, baseline.y)),
            (Vec2::new(-10.0, 0.0), Vec2::new(-safe_limits.x, baseline.y)),
            (Vec2::new(0.0, 10.0), Vec2::new(baseline.x, safe_limits.y)),
            (Vec2::new(0.0, -10.0), Vec2::new(baseline.x, -safe_limits.y)),
        ] {
            let admitted = adjustments_after_pan_input(
                aabb,
                settings,
                CameraRuntimeAdjustments::default(),
                aspect,
                request,
            );
            let final_camera =
                resolve_camera_parameters_with_aspect(aabb, settings, admitted, aspect);
            approx_vec2(projected_rest_center(final_camera, aabb, aspect), expected);
        }
    }

    #[test]
    fn widened_vertical_envelope_allows_default_baseline_to_move_toward_top() {
        let aabb = standard_pan_aabb();
        let settings = CameraSettings::default();
        let aspect = DEFAULT_VIEWPORT_ASPECT;
        let baseline_camera = resolve_camera_parameters_with_aspect(
            aabb,
            settings,
            CameraRuntimeAdjustments::default(),
            aspect,
        );
        let baseline = projected_rest_center(baseline_camera, aabb, aspect);
        assert!(baseline.y < -1.0, "{baseline:?}");
        assert!(baseline.y - 0.25 > -PAN_WITNESS_SAFE_Y_NDC);

        let admitted = adjustments_after_pan_input(
            aabb,
            settings,
            CameraRuntimeAdjustments::default(),
            aspect,
            Vec2::new(0.0, -0.25),
        );
        assert!(admitted.pan_ndc.y < 0.0);
        let final_camera = resolve_camera_parameters_with_aspect(aabb, settings, admitted, aspect);
        approx_eq(
            projected_rest_center(final_camera, aabb, aspect).y,
            baseline.y - 0.25,
        );
    }

    #[test]
    fn portrait_zero_roll_keeps_nominal_horizontal_stopper() {
        let aabb = standard_pan_aabb();
        let settings = CameraSettings::default();
        let aspect = 540.0 / 960.0;
        let zero = resolve_camera_parameters_with_aspect(
            aabb,
            settings,
            CameraRuntimeAdjustments::default(),
            aspect,
        );
        let baseline = projected_rest_center(zero, aabb, aspect);
        let safe_limits = pan_witness_safe_limits(baseline);
        approx_eq(safe_limits.x, PAN_WITNESS_SAFE_X_NDC);

        for desired_x in [-10.0, 10.0] {
            let admitted = adjustments_after_pan_input(
                aabb,
                settings,
                CameraRuntimeAdjustments::default(),
                aspect,
                Vec2::new(desired_x, 0.0),
            );
            let final_camera =
                resolve_camera_parameters_with_aspect(aabb, settings, admitted, aspect);
            approx_eq(
                projected_rest_center(final_camera, aabb, aspect).x,
                desired_x.signum() * PAN_WITNESS_SAFE_X_NDC,
            );
        }
    }

    #[test]
    fn portrait_roll_90_keeps_both_horizontal_pan_directions_available() {
        let aabb = standard_pan_aabb();
        let settings = CameraSettings::default();
        let aspect = 540.0 / 960.0;

        for roll_deg in [-90.0, 90.0] {
            let pose = CameraRuntimeAdjustments {
                roll_deg,
                ..Default::default()
            };
            let baseline_camera =
                resolve_camera_parameters_with_aspect(aabb, settings, pose, aspect);
            let baseline = projected_rest_center(baseline_camera, aabb, aspect);
            let safe_limits = pan_witness_safe_limits(baseline);
            approx_eq(
                safe_limits.x,
                baseline.x.abs() + PAN_X_BASELINE_OUTWARD_SLACK_NDC,
            );
            assert!(safe_limits.x > 3.0);
            assert!(baseline.x.abs() < safe_limits.x, "{baseline:?}");

            for desired_x in [-1.25, 1.25] {
                let admitted = adjustments_after_pan_input(
                    aabb,
                    settings,
                    pose,
                    aspect,
                    Vec2::new(desired_x, 0.0),
                );
                assert_eq!(admitted.pan_ndc.x.signum(), desired_x.signum());
                let final_camera =
                    resolve_camera_parameters_with_aspect(aabb, settings, admitted, aspect);
                approx_eq(
                    projected_rest_center(final_camera, aabb, aspect).x,
                    baseline.x + desired_x,
                );
            }
        }
    }

    #[test]
    fn outside_positive_baseline_has_bounded_outward_slack() {
        let aabb = standard_pan_aabb();
        let settings = outside_pan_test_settings();
        let aspect = DEFAULT_VIEWPORT_ASPECT;
        let pose = CameraRuntimeAdjustments {
            roll_deg: 180.0,
            ..Default::default()
        };
        let baseline_camera = resolve_camera_parameters_with_aspect(aabb, settings, pose, aspect);
        let baseline = projected_rest_center(baseline_camera, aabb, aspect);
        let safe_limits = pan_witness_safe_limits(baseline);
        assert!(baseline.y > PAN_WITNESS_SAFE_Y_NDC, "{baseline:?}");
        approx_eq(safe_limits.y, baseline.y + PAN_Y_BASELINE_OUTWARD_SLACK_NDC);

        let unchanged = adjustments_after_pan_input(aabb, settings, pose, aspect, Vec2::ZERO);
        assert_eq!(unchanged.pan_ndc, Vec2::ZERO);
        let outward =
            adjustments_after_pan_input(aabb, settings, pose, aspect, Vec2::new(0.0, 0.25));
        let outward_camera = resolve_camera_parameters_with_aspect(aabb, settings, outward, aspect);
        approx_eq(
            projected_rest_center(outward_camera, aabb, aspect).y,
            baseline.y + 0.25,
        );
        let stopped =
            adjustments_after_pan_input(aabb, settings, pose, aspect, Vec2::new(0.0, 10.0));
        let stopped_camera = resolve_camera_parameters_with_aspect(aabb, settings, stopped, aspect);
        approx_eq(
            projected_rest_center(stopped_camera, aabb, aspect).y,
            safe_limits.y,
        );
        let inward =
            adjustments_after_pan_input(aabb, settings, pose, aspect, Vec2::new(0.0, -0.25));
        let inward_camera = resolve_camera_parameters_with_aspect(aabb, settings, inward, aspect);
        approx_eq(
            projected_rest_center(inward_camera, aabb, aspect).y,
            baseline.y - 0.25,
        );
        assert!(inward.pan_ndc.y < 0.0);
    }

    #[test]
    fn outside_negative_baseline_mirrors_bounded_slack_policy() {
        let aabb = standard_pan_aabb();
        let settings = outside_pan_test_settings();
        let aspect = DEFAULT_VIEWPORT_ASPECT;
        let pose = CameraRuntimeAdjustments::default();
        let baseline_camera = resolve_camera_parameters_with_aspect(aabb, settings, pose, aspect);
        let baseline = projected_rest_center(baseline_camera, aabb, aspect);
        let safe_limits = pan_witness_safe_limits(baseline);
        assert!(baseline.y < -PAN_WITNESS_SAFE_Y_NDC, "{baseline:?}");
        approx_eq(
            safe_limits.y,
            -baseline.y + PAN_Y_BASELINE_OUTWARD_SLACK_NDC,
        );

        let outward =
            adjustments_after_pan_input(aabb, settings, pose, aspect, Vec2::new(0.0, -0.25));
        let outward_camera = resolve_camera_parameters_with_aspect(aabb, settings, outward, aspect);
        approx_eq(
            projected_rest_center(outward_camera, aabb, aspect).y,
            baseline.y - 0.25,
        );
        let stopped =
            adjustments_after_pan_input(aabb, settings, pose, aspect, Vec2::new(0.0, -10.0));
        let stopped_camera = resolve_camera_parameters_with_aspect(aabb, settings, stopped, aspect);
        approx_eq(
            projected_rest_center(stopped_camera, aabb, aspect).y,
            -safe_limits.y,
        );
        let inward =
            adjustments_after_pan_input(aabb, settings, pose, aspect, Vec2::new(0.0, 0.25));
        let inward_camera = resolve_camera_parameters_with_aspect(aabb, settings, inward, aspect);
        approx_eq(
            projected_rest_center(inward_camera, aabb, aspect).y,
            baseline.y + 0.25,
        );
        assert!(inward.pan_ndc.y > 0.0);
    }

    #[test]
    fn orientation_change_can_make_witness_outside_without_mutating_pan() {
        let aabb = standard_pan_aabb();
        let settings = CameraSettings {
            distance_scale: 0.4,
            ..CameraSettings::default()
        };
        let aspect = 1.0;
        let stored_pan = Vec2::new(0.0, -0.8);
        let before_pose = CameraRuntimeAdjustments {
            pan_ndc: stored_pan,
            roll_deg: 90.0,
            ..Default::default()
        };
        let after_pose = CameraRuntimeAdjustments {
            pan_ndc: stored_pan,
            roll_deg: 0.0,
            ..Default::default()
        };
        let before = resolve_camera_parameters_with_aspect(aabb, settings, before_pose, aspect);
        let after = resolve_camera_parameters_with_aspect(aabb, settings, after_pose, aspect);
        let zero_before = resolve_camera_parameters_with_aspect(
            aabb,
            settings,
            CameraRuntimeAdjustments {
                pan_ndc: Vec2::ZERO,
                ..before_pose
            },
            aspect,
        );

        let before_projection = projected_rest_center(before, aabb, aspect);
        let safe_limits = pan_witness_safe_limits(projected_rest_center(zero_before, aabb, aspect));
        assert!(
            before_projection.x.abs() < safe_limits.x && before_projection.y.abs() < safe_limits.y,
            "{before_projection:?}"
        );
        let zero_after = resolve_camera_parameters_with_aspect(
            aabb,
            settings,
            CameraRuntimeAdjustments {
                pan_ndc: Vec2::ZERO,
                ..after_pose
            },
            aspect,
        );
        let after_limits = pan_witness_safe_limits(projected_rest_center(zero_after, aabb, aspect));
        assert!(projected_rest_center(after, aabb, aspect).y < -after_limits.y);
        assert_eq!(before.pan_ndc, stored_pan);
        assert_eq!(after.pan_ndc, stored_pan);
    }

    #[test]
    fn orientation_fov_distance_and_aspect_changes_never_correct_stored_pan() {
        let aabb = standard_pan_aabb();
        let stored_pan = Vec2::new(0.73, -0.61);
        let cases = [
            (
                CameraSettings::default(),
                CameraRuntimeAdjustments {
                    pan_ndc: stored_pan,
                    ..Default::default()
                },
                0.75,
            ),
            (
                CameraSettings::default(),
                CameraRuntimeAdjustments {
                    fov_delta_deg: 65.0,
                    distance_scale_delta: 4.0,
                    pan_ndc: stored_pan,
                    yaw_deg: 137.0,
                    pitch_deg: -63.0,
                    roll_deg: 91.0,
                },
                2.4,
            ),
            (
                CameraSettings {
                    fov_deg: 22.0,
                    distance_scale: 0.2,
                    headroom: 0.31,
                },
                CameraRuntimeAdjustments {
                    pan_ndc: stored_pan,
                    yaw_deg: -179.9,
                    pitch_deg: 71.0,
                    roll_deg: -179.9,
                    ..Default::default()
                },
                0.31,
            ),
        ];

        for (settings, adjustments, aspect) in cases {
            let parameters =
                resolve_camera_parameters_with_aspect(aabb, settings, adjustments, aspect);
            assert_eq!(parameters.pan_ndc, stored_pan);
        }
    }

    #[test]
    fn nonzero_pan_has_no_safety_jump_through_roll_sweeps_and_wrap() {
        let aabb = standard_pan_aabb();
        let aspect = DEFAULT_VIEWPORT_ASPECT;
        let requested_pan = Vec2::new(0.43, -0.57);
        let rolls = [
            44.0, 44.5, 45.0, 45.5, 46.0, 89.0, 89.5, 90.0, 90.5, 91.0, -91.0, -90.5, -90.0, -89.5,
            -89.0, 179.0, 179.5, 179.9, 180.1, 180.5, 181.0, -181.0, -180.5, -180.1, -179.9,
            -179.5, -179.0,
        ];

        for roll_deg in rolls {
            let pose = CameraRuntimeAdjustments {
                roll_deg,
                ..Default::default()
            };
            let zero = resolve_camera_parameters_with_aspect(
                aabb,
                in_range_pan_test_settings(),
                pose,
                aspect,
            );
            let panned = resolve_camera_parameters_with_aspect(
                aabb,
                in_range_pan_test_settings(),
                CameraRuntimeAdjustments {
                    pan_ndc: requested_pan,
                    ..pose
                },
                aspect,
            );

            assert_eq!(panned.pan_ndc, requested_pan);
            approx_vec2(
                projected_baseline_delta(zero, panned, aspect),
                requested_pan,
            );
        }

        for (left_roll, right_roll) in [(179.9, -180.1), (-179.9, 180.1)] {
            let left = resolve_camera_parameters_with_aspect(
                aabb,
                in_range_pan_test_settings(),
                CameraRuntimeAdjustments {
                    pan_ndc: requested_pan,
                    roll_deg: left_roll,
                    ..Default::default()
                },
                aspect,
            );
            let right = resolve_camera_parameters_with_aspect(
                aabb,
                in_range_pan_test_settings(),
                CameraRuntimeAdjustments {
                    pan_ndc: requested_pan,
                    roll_deg: right_roll,
                    ..Default::default()
                },
                aspect,
            );
            approx_vec3(left.position, right.position);
            approx_vec3(left.frame.target, right.frame.target);
        }
    }

    #[test]
    fn combined_yaw_pitch_roll_preserves_screen_pan_axes() {
        let aabb = standard_pan_aabb();
        let aspect = 0.83;
        let pose = CameraRuntimeAdjustments {
            yaw_deg: 133.0,
            pitch_deg: 62.0,
            roll_deg: -73.0,
            ..Default::default()
        };
        let zero =
            resolve_camera_parameters_with_aspect(aabb, CameraSettings::default(), pose, aspect);
        for requested_pan in [Vec2::new(0.52, 0.0), Vec2::new(0.0, 0.46)] {
            let panned = resolve_camera_parameters_with_aspect(
                aabb,
                CameraSettings::default(),
                CameraRuntimeAdjustments {
                    pan_ndc: requested_pan,
                    ..pose
                },
                aspect,
            );
            approx_vec2(
                projected_baseline_delta(zero, panned, aspect),
                requested_pan,
            );
        }
    }

    #[test]
    fn combined_axes_gate_independently() {
        let aabb = standard_pan_aabb();
        let settings = in_range_pan_test_settings();
        let aspect = 1.2;
        let zero = resolve_camera_parameters_with_aspect(
            aabb,
            settings,
            CameraRuntimeAdjustments::default(),
            aspect,
        );
        let safe_limits = pan_witness_safe_limits(projected_rest_center(zero, aabb, aspect));
        let saturated = adjustments_after_pan_input(
            aabb,
            settings,
            CameraRuntimeAdjustments::default(),
            aspect,
            Vec2::new(10.0, -10.0),
        );
        let saturated_camera =
            resolve_camera_parameters_with_aspect(aabb, settings, saturated, aspect);
        approx_vec2(
            projected_rest_center(saturated_camera, aabb, aspect),
            Vec2::new(safe_limits.x, -safe_limits.y),
        );

        let x_reversed =
            adjustments_after_pan_input(aabb, settings, saturated, aspect, Vec2::new(-0.2, 0.0));
        assert_eq!(x_reversed.pan_ndc.y, saturated.pan_ndc.y);
        let reversed_camera =
            resolve_camera_parameters_with_aspect(aabb, settings, x_reversed, aspect);
        approx_vec2(
            projected_rest_center(reversed_camera, aabb, aspect),
            Vec2::new(safe_limits.x - 0.2, -safe_limits.y),
        );
    }

    #[test]
    fn pan_preserves_camera_target_distance_and_orientation() {
        let aabb = standard_pan_aabb();
        let pose = CameraRuntimeAdjustments {
            yaw_deg: 47.0,
            pitch_deg: -12.0,
            roll_deg: 31.0,
            ..Default::default()
        };
        let zero = resolve_camera_parameters_with_aspect(
            aabb,
            CameraSettings::default(),
            pose,
            DEFAULT_VIEWPORT_ASPECT,
        );
        let panned = resolve_camera_parameters_with_aspect(
            aabb,
            CameraSettings::default(),
            CameraRuntimeAdjustments {
                pan_ndc: Vec2::new(0.71, -0.63),
                ..pose
            },
            DEFAULT_VIEWPORT_ASPECT,
        );
        let zero_camera = camera_for_parameters(zero);
        let panned_camera = camera_for_parameters(panned);

        approx_eq(
            (panned.frame.target - panned.position).length(),
            panned.frame.distance,
        );
        approx_vec3(
            panned.frame.target - zero.frame.target,
            panned.position - zero.position,
        );
        approx_vec3(zero_camera.forward(), panned_camera.forward());
        approx_vec3(zero_camera.screen_right(), panned_camera.screen_right());
        approx_vec3(zero_camera.screen_up(), panned_camera.screen_up());
    }

    #[test]
    fn stopper_is_idempotent_and_reverse_has_no_hidden_windup() {
        let aabb = standard_pan_aabb();
        let settings = in_range_pan_test_settings();
        let aspect = 1.2;
        let zero = resolve_camera_parameters_with_aspect(
            aabb,
            settings,
            CameraRuntimeAdjustments::default(),
            aspect,
        );
        let safe_limits = pan_witness_safe_limits(projected_rest_center(zero, aabb, aspect));
        let saturated = adjustments_after_pan_input(
            aabb,
            settings,
            CameraRuntimeAdjustments::default(),
            aspect,
            Vec2::new(10.0, 0.0),
        );
        let repeated =
            adjustments_after_pan_input(aabb, settings, saturated, aspect, Vec2::new(10.0, 0.0));
        assert_eq!(repeated.pan_ndc, saturated.pan_ndc);

        let reversed =
            adjustments_after_pan_input(aabb, settings, repeated, aspect, Vec2::new(-0.1, 0.0));
        let reversed_camera =
            resolve_camera_parameters_with_aspect(aabb, settings, reversed, aspect);
        approx_eq(
            projected_rest_center(reversed_camera, aabb, aspect).x,
            safe_limits.x - 0.1,
        );
        assert!(reversed.pan_ndc.x < saturated.pan_ndc.x);
    }

    #[test]
    fn reset_is_exact_zero_even_for_outside_baseline() {
        let aabb = standard_pan_aabb();
        let settings = outside_pan_test_settings();
        let aspect = DEFAULT_VIEWPORT_ASPECT;
        let adjusted = CameraRuntimeAdjustments {
            pan_ndc: Vec2::new(0.4, -0.3),
            roll_deg: 180.0,
            ..Default::default()
        };
        let reset = CameraRuntimeAdjustments::default();
        let reset_camera = resolve_camera_parameters_with_aspect(aabb, settings, reset, aspect);

        assert_ne!(adjusted.pan_ndc, Vec2::ZERO);
        assert_eq!(reset.pan_ndc, Vec2::ZERO);
        assert_eq!(reset_camera.pan_ndc, Vec2::ZERO);
        assert!(projected_rest_center(reset_camera, aabb, aspect).y < -PAN_WITNESS_SAFE_Y_NDC);
        approx_vec3(reset_camera.frame.target, reset_camera.baseline_target);
    }

    #[test]
    fn uniform_model_scale_preserves_admission_and_scales_world_translation() {
        let base_aabb = standard_pan_aabb();
        let settings = in_range_pan_test_settings();
        let aspect = 1.2;
        let desired_witness_delta = Vec2::new(0.64, -0.31);
        let pose = CameraRuntimeAdjustments {
            yaw_deg: 47.0,
            pitch_deg: -12.0,
            roll_deg: 31.0,
            ..Default::default()
        };
        let mut reference_pan: Option<Vec2> = None;
        let mut reference_translation: Option<Vec3> = None;
        let mut reference_projection: Option<Vec2> = None;

        for scale in [1.0e-3_f32, 1.0, 1.0e3] {
            let aabb = (base_aabb.0 * scale, base_aabb.1 * scale);
            let admitted =
                adjustments_after_pan_input(aabb, settings, pose, aspect, desired_witness_delta);
            let zero = resolve_camera_parameters_with_aspect(aabb, settings, pose, aspect);
            let panned = resolve_camera_parameters_with_aspect(aabb, settings, admitted, aspect);
            let translation = panned.position - zero.position;
            let projection = projected_rest_center(panned, aabb, aspect);

            if let Some(reference) = reference_pan {
                approx_vec2(admitted.pan_ndc, reference);
            } else {
                reference_pan = Some(admitted.pan_ndc);
            }
            if let Some(reference) = reference_projection {
                approx_vec2(projection, reference);
            } else {
                reference_projection = Some(projection);
            }
            if let Some(reference) = reference_translation {
                assert!(
                    (translation / scale - reference).length()
                        < 2.0e-4 * reference.length().max(1.0),
                    "{translation:?} at scale {scale} did not scale from {reference:?}"
                );
            } else {
                reference_translation = Some(translation / scale);
            }
        }
    }

    #[test]
    fn invalid_or_behind_witness_rejects_pan_without_nonfinite_state() {
        let current = CameraRuntimeAdjustments {
            pan_ndc: Vec2::new(0.2, -0.3),
            ..Default::default()
        };
        let invalid_aabb = (Vec3::splat(f32::NAN), Vec3::splat(f32::INFINITY));
        let invalid_result = admit_pan_input(
            invalid_aabb,
            CameraSettings::default(),
            current,
            DEFAULT_VIEWPORT_ASPECT,
            Vec2::ONE,
        );
        assert_eq!(invalid_result, current.pan_ndc);
        assert!(invalid_result.is_finite());

        let behind_settings = CameraSettings {
            distance_scale: 0.1,
            headroom: 0.49,
            ..CameraSettings::default()
        };
        let behind_pose = CameraRuntimeAdjustments {
            pitch_deg: 89.0,
            ..current
        };
        assert!(
            resolve_pan_witness_projection(
                standard_pan_aabb(),
                behind_settings,
                behind_pose,
                DEFAULT_VIEWPORT_ASPECT,
            )
            .is_none()
        );
        let behind_result = admit_pan_input(
            standard_pan_aabb(),
            behind_settings,
            behind_pose,
            DEFAULT_VIEWPORT_ASPECT,
            Vec2::ONE,
        );
        assert_eq!(behind_result, current.pan_ndc);
        assert!(behind_result.is_finite());
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
    fn f8_yaw_normalization_is_visually_continuous() {
        let aabb = (Vec3::new(-0.4, 0.0, -0.2), Vec3::new(0.6, 1.8, 0.4));
        let before = resolve_camera_parameters_with_aspect(
            aabb,
            CameraSettings::default(),
            CameraRuntimeAdjustments {
                yaw_deg: 179.0,
                ..CameraRuntimeAdjustments::default()
            },
            DEFAULT_VIEWPORT_ASPECT,
        );
        let after = resolve_camera_parameters_with_aspect(
            aabb,
            CameraSettings::default(),
            CameraRuntimeAdjustments {
                yaw_deg: 181.0,
                ..CameraRuntimeAdjustments::default()
            },
            DEFAULT_VIEWPORT_ASPECT,
        );
        let mut before_camera = Camera::default();
        before_camera.yaw = before.yaw_deg.to_radians();
        before_camera.pitch = before.pitch_deg.to_radians();
        let mut after_camera = Camera::default();
        after_camera.yaw = after.yaw_deg.to_radians();
        after_camera.pitch = after.pitch_deg.to_radians();

        assert_eq!(
            CameraRuntimeAdjustments {
                yaw_deg: 181.0,
                ..CameraRuntimeAdjustments::default()
            }
            .effective(CameraSettings::default())
            .yaw_deg,
            -179.0
        );
        assert!(before_camera.forward().dot(after_camera.forward()) > 0.99);
        assert!(before_camera.right().dot(after_camera.right()) > 0.99);
        assert!((before.position - after.position).length() < 0.1);
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
    fn f8_roll_normalization_preserves_view_basis_across_wrap() {
        let before = CameraRuntimeAdjustments {
            roll_deg: 179.0,
            ..CameraRuntimeAdjustments::default()
        }
        .sanitized();
        let after = CameraRuntimeAdjustments {
            roll_deg: 181.0,
            ..CameraRuntimeAdjustments::default()
        }
        .sanitized();
        assert_eq!(before.roll_deg, 179.0);
        assert_eq!(after.roll_deg, -179.0);

        let mut before_camera = Camera::default();
        before_camera.yaw = 0.7;
        before_camera.pitch = -0.3;
        before_camera.roll = before.roll_deg.to_radians();
        let mut after_camera = before_camera;
        after_camera.roll = after.roll_deg.to_radians();

        approx_vec3(before_camera.forward(), after_camera.forward());
        assert!(
            before_camera
                .screen_right()
                .dot(after_camera.screen_right())
                > 0.99
        );
        assert!(before_camera.screen_up().dot(after_camera.screen_up()) > 0.99);
        assert_ne!(
            before_camera.view().to_cols_array(),
            after_camera.view().to_cols_array()
        );
    }

    #[test]
    fn f8_roll_is_a_forward_axis_camera_rotation() {
        let mut neutral = Camera::default();
        neutral.yaw = 0.7;
        neutral.pitch = -0.3;
        let mut rolled = neutral;
        rolled.roll = 0.6;

        approx_vec3(neutral.forward(), rolled.forward());
        assert!(neutral.screen_right().dot(rolled.screen_right()) < 0.9);
        assert!(neutral.screen_up().dot(rolled.screen_up()) < 0.9);
        approx_vec3(
            rolled.screen_up(),
            rolled.up() * 0.8253356 + rolled.right() * 0.5646425,
        );
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
