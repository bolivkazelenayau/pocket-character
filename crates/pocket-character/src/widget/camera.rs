use glam::{Vec2, Vec3};
use pocket3d::camera::Camera;

use crate::settings::CameraSettings;

pub(super) mod controls;

const MIN_MODEL_HEIGHT: f32 = 0.001;
const MAX_MODEL_BOUND: f32 = 100_000.0;
/// Rest-pose AABBs can omit a small animated excursion from hair/accessories.
const TOP_SAFETY_MARGIN: f32 = 0.02;
pub(super) const DEFAULT_VIEWPORT_ASPECT: f32 = 0.75;
const MAX_RUNTIME_FOV_DELTA_DEG: f32 = 178.0;
const MAX_RUNTIME_DISTANCE_DELTA: f32 = 10.0;
const MAX_RUNTIME_PITCH_DEG: f32 = 89.0;

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

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct CameraFrame {
    target: Vec3,
    distance: f32,
    pub(super) fov_y: f32,
    view_height: f32,
}

/// Runtime-only camera deltas used by the temporary F8 validation controls.
///
/// These deliberately live outside [`CameraSettings`]. Persisted settings
/// remain the framing baseline, while this state can be changed on the live
/// widget without changing the settings file or rebuilding any GPU object.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub(super) struct CameraRuntimeAdjustments {
    pub(super) fov_delta_deg: f32,
    pub(super) distance_scale_delta: f32,
    /// Additional projected displacement of the zero-pan baseline target.
    /// Positive X is screen-right and positive Y is screen-up. Safety is
    /// enforced only when admitting new pan input; camera resolution never
    /// clamps or otherwise corrects this stored state.
    pub(super) pan_ndc: Vec2,
    pub(super) yaw_deg: f32,
    pub(super) roll_deg: f32,
    pub(super) pitch_deg: f32,
}

impl CameraRuntimeAdjustments {
    pub(super) fn sanitized(self) -> Self {
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

    pub(super) fn effective(self, base: CameraSettings) -> EffectiveCameraValues {
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
pub(super) struct EffectiveCameraValues {
    pub(super) settings: CameraSettings,
    pub(super) pan_ndc: Vec2,
    pub(super) yaw_deg: f32,
    pub(super) roll_deg: f32,
    pub(super) pitch_deg: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct CameraParameters {
    pub(super) frame: CameraFrame,
    /// The bounds-derived target before any runtime framing translation.
    ///
    /// This remains the baseline camera-composition target;
    /// `frame.target` is the final translated camera target.
    pub(super) baseline_target: Vec3,
    /// Additional NDC displacement of `baseline_target`.
    pan_ndc: Vec2,
    pub(super) yaw_deg: f32,
    pub(super) roll_deg: f32,
    pub(super) pitch_deg: f32,
    pub(super) position: Vec3,
}

pub(super) fn normalize_degrees(value: f32) -> f32 {
    if value.is_finite() {
        (value + 180.0).rem_euclid(360.0) - 180.0
    } else {
        0.0
    }
}

pub(super) fn finite_clamped(value: f32, min: f32, max: f32, fallback: f32) -> f32 {
    finite_value(value, fallback).clamp(min, max)
}

pub(super) fn finite_value(value: f32, fallback: f32) -> f32 {
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
pub(super) fn resolve_camera_parameters_with_aspect(
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
pub(super) fn admit_pan_input(
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

#[cfg(test)]
mod tests;
