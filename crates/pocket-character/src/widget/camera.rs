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

/// Required visible fraction of the projected rest-bounds extent, subject to
/// absolute NDC floor and ceiling limits. The full-bounds requirement
/// strengthens toward the close target as the projected extent grows; the
/// horizontal body core keeps the unstrengthened hybrid base.
const PAN_VISIBLE_FRACTION: f32 = 0.25;
const PAN_MIN_VISIBLE_OVERLAP_NDC: f32 = 0.10;
const PAN_MAX_VISIBLE_OVERLAP_NDC: f32 = 0.40;
const PAN_CLOSE_VISIBLE_OVERLAP_NDC: f32 = 0.45;
const PAN_CLOSE_STRENGTHEN_START_EXTENT_NDC: f32 = 2.0;
const PAN_CLOSE_STRENGTHEN_END_EXTENT_NDC: f32 = 5.0;
/// Half-width of the central body proxy used to keep horizontal pan from
/// admitting an empty extreme of a wide rest AABB.
const PAN_X_CORE_HALF_WIDTH_FRACTION: f32 = 0.140;
const PAN_WITNESS_DEPTH_EPSILON_RATIO: f32 = 1.0e-5;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct CameraFrame {
    target: Vec3,
    distance: f32,
    pub(super) fov_y: f32,
    view_height: f32,
}

/// Camera inputs needed for admitting new pan movement. The persisted camera
/// settings remain encapsulated in the camera module rather than in controls.
#[derive(Clone, Copy, Debug)]
pub(super) struct CameraPanContext {
    aabb: (Vec3, Vec3),
    base_settings: CameraSettings,
}

impl CameraPanContext {
    pub(super) fn new(aabb: (Vec3, Vec3), base_settings: CameraSettings) -> Self {
        Self {
            aabb,
            base_settings,
        }
    }

    pub(super) fn admit(
        self,
        adjustments: CameraRuntimeAdjustments,
        viewport_aspect: f32,
        desired_witness_delta_ndc: Vec2,
    ) -> Vec2 {
        admit_pan_input(
            self.aabb,
            self.base_settings,
            adjustments,
            viewport_aspect,
            desired_witness_delta_ndc,
        )
    }

    pub(super) fn validate(
        self,
        adjustments: CameraRuntimeAdjustments,
        viewport_aspect: f32,
    ) -> CameraRuntimeAdjustments {
        validate_pan(self.aabb, self.base_settings, adjustments, viewport_aspect)
    }
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
    /// Positive X is screen-right and positive Y is screen-up. Both axes use
    /// projected rest-bounds safety and are revalidated when framing changes.
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
            yaw_snap_deg: base.yaw_snap_deg,
            roll_snap_deg: base.roll_snap_deg,
            pitch_snap_deg: base.pitch_snap_deg,
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

fn smoothstep(edge_start: f32, edge_end: f32, value: f32) -> f32 {
    let t = ((value - edge_start) / (edge_end - edge_start)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn lerp(start: f32, end: f32, t: f32) -> f32 {
    start + (end - start) * t
}

fn base_visible_overlap(projected_extent: f32) -> f32 {
    projected_extent.min(
        (projected_extent * PAN_VISIBLE_FRACTION)
            .clamp(PAN_MIN_VISIBLE_OVERLAP_NDC, PAN_MAX_VISIBLE_OVERLAP_NDC),
    )
}

fn required_visible_overlap(projected_extent: f32) -> f32 {
    let base = base_visible_overlap(projected_extent);
    let t = smoothstep(
        PAN_CLOSE_STRENGTHEN_START_EXTENT_NDC,
        PAN_CLOSE_STRENGTHEN_END_EXTENT_NDC,
        projected_extent,
    );

    lerp(base, PAN_CLOSE_VISIBLE_OVERLAP_NDC, t)
}

fn required_core_visible_overlap(projected_extent: f32) -> f32 {
    base_visible_overlap(projected_extent)
}

#[derive(Clone, Copy, Debug)]
struct CameraOrientation {
    frame: CameraFrame,
    baseline_target: Vec3,
    camera: Camera,
    distance: f32,
}

/// Resolve the effective orbit before applying pan. Keeping this separate
/// lets the projected-bounds solver inspect the exact effective FOV,
/// distance, yaw, pitch, and roll without recursing through pan resolution.
fn resolve_camera_orientation(
    aabb: (Vec3, Vec3),
    base_settings: CameraSettings,
    adjustments: CameraRuntimeAdjustments,
) -> CameraOrientation {
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

    CameraOrientation {
        frame: baseline_frame,
        baseline_target,
        camera: orientation,
        distance,
    }
}

/// Resolve camera state in this order:
///
/// 1. Derive the baseline bounds/headroom target.
/// 2. Apply runtime yaw, pitch, and roll to the baseline orbit orientation.
/// 3. Solve the projected-bounds-safe X and Y pan intervals for that exact
///    effective camera state.
/// 4. Correct either pan axis when necessary, then translate both camera
///    position and target by the resulting NDC pan.
///
/// The camera basis is therefore never derived from a partially panned frame.
/// The same path is used by live input and by re-framing after a resize.
pub(super) fn resolve_camera_parameters_with_aspect(
    aabb: (Vec3, Vec3),
    base_settings: CameraSettings,
    adjustments: CameraRuntimeAdjustments,
    viewport_aspect: f32,
) -> CameraParameters {
    let adjustments = adjustments.sanitized();
    let orientation = resolve_camera_orientation(aabb, base_settings, adjustments);
    let aspect = valid_viewport_aspect(viewport_aspect);
    let pan_ndc = validated_pan_ndc(aabb, orientation, adjustments.pan_ndc, aspect);
    let world_pan =
        pan_ndc * pan_world_per_ndc(orientation.distance, orientation.frame.fov_y, aspect);
    let screen_right = orientation.camera.screen_right();
    let screen_up = orientation.camera.screen_up();
    let requested_translation = -screen_right * world_pan.x - screen_up * world_pan.y;
    let translation = if requested_translation.is_finite() {
        requested_translation
    } else {
        Vec3::ZERO
    };
    let position = orientation.camera.pos + translation;
    let mut frame = orientation.frame;
    frame.target = orientation.baseline_target + translation;

    CameraParameters {
        frame,
        baseline_target: orientation.baseline_target,
        pan_ndc,
        yaw_deg: orientation.camera.yaw.to_degrees(),
        roll_deg: orientation.camera.roll.to_degrees(),
        pitch_deg: orientation.camera.pitch.to_degrees(),
        position,
    }
}

#[cfg(test)]
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
    pan_intervals: [PanInterval; 2],
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PanInterval {
    min: f32,
    max: f32,
}

impl PanInterval {
    fn is_valid(self) -> bool {
        self.min.is_finite() && self.max.is_finite() && self.min <= self.max
    }
}

fn rest_bounds_corners((min, max): (Vec3, Vec3)) -> [Vec3; 8] {
    let min = Vec3::new(min.x.min(max.x), min.y.min(max.y), min.z.min(max.z));
    let max = Vec3::new(max.x.max(min.x), max.y.max(min.y), max.z.max(min.z));

    [
        Vec3::new(min.x, min.y, min.z),
        Vec3::new(min.x, min.y, max.z),
        Vec3::new(min.x, max.y, min.z),
        Vec3::new(min.x, max.y, max.z),
        Vec3::new(max.x, min.y, min.z),
        Vec3::new(max.x, min.y, max.z),
        Vec3::new(max.x, max.y, min.z),
        Vec3::new(max.x, max.y, max.z),
    ]
}

fn horizontal_core_aabb((min, max): (Vec3, Vec3)) -> (Vec3, Vec3) {
    let center_x = (min.x + max.x) * 0.5;
    let half_width = (max.x - min.x) * PAN_X_CORE_HALF_WIDTH_FRACTION;
    (
        Vec3::new(center_x - half_width, min.y, min.z),
        Vec3::new(center_x + half_width, max.y, max.z),
    )
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ProjectedCorner {
    projected_ndc: Vec2,
    response: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ProjectedRestBounds {
    corners: [ProjectedCorner; 8],
    extent: Vec2,
}

/// Project every rest-AABB corner once. The same projected coordinates and
/// depth response feed the independent X and Y interval reductions below.
fn project_rest_bounds(
    camera: Camera,
    distance: f32,
    fov_y: f32,
    aspect: f32,
    corners: &[Vec3; 8],
) -> Option<ProjectedRestBounds> {
    if !distance.is_finite()
        || distance <= 0.0
        || !fov_y.is_finite()
        || fov_y <= 0.0
        || !aspect.is_finite()
        || aspect <= 0.0
    {
        return None;
    }

    let view_proj = camera.view_proj(aspect);
    let forward = camera.forward();
    let depth_epsilon = distance * PAN_WITNESS_DEPTH_EPSILON_RATIO;
    let mut projected_corners = [ProjectedCorner {
        projected_ndc: Vec2::ZERO,
        response: 0.0,
    }; 8];
    let mut min_ndc = Vec2::splat(f32::INFINITY);
    let mut max_ndc = Vec2::splat(f32::NEG_INFINITY);

    for (index, corner) in corners.iter().enumerate() {
        let depth = (*corner - camera.pos).dot(forward);
        if !depth.is_finite() || depth <= depth_epsilon {
            return None;
        }

        let projected = view_proj.project_point3(*corner);
        let projected_ndc = projected.truncate();
        let response = distance / depth;
        if !projected.is_finite()
            || !projected_ndc.is_finite()
            || !response.is_finite()
            || response <= 0.0
        {
            return None;
        }

        projected_corners[index] = ProjectedCorner {
            projected_ndc,
            response,
        };
        min_ndc = min_ndc.min(projected_ndc);
        max_ndc = max_ndc.max(projected_ndc);
    }

    let extent = max_ndc - min_ndc;
    if !min_ndc.is_finite()
        || !max_ndc.is_finite()
        || !extent.is_finite()
        || extent.min_element() < 0.0
    {
        return None;
    }

    Some(ProjectedRestBounds {
        corners: projected_corners,
        extent,
    })
}

fn projected_pan_interval_with_requirement(
    projected: &ProjectedRestBounds,
    axis: usize,
    required_overlap: f32,
) -> Option<PanInterval> {
    if !required_overlap.is_finite() || required_overlap < 0.0 {
        return None;
    }

    let viewport_min = -1.0 + required_overlap;
    let viewport_max = 1.0 - required_overlap;
    let mut min_pan = f32::INFINITY;
    let mut max_pan = f32::NEG_INFINITY;

    for corner in projected.corners {
        let coordinate = corner.projected_ndc[axis];
        let response = corner.response;
        min_pan = min_pan.min((viewport_min - coordinate) / response);
        max_pan = max_pan.max((viewport_max - coordinate) / response);
    }

    let interval = PanInterval {
        min: min_pan,
        max: max_pan,
    };
    interval.is_valid().then_some(interval)
}

fn projected_pan_interval(projected: &ProjectedRestBounds, axis: usize) -> Option<PanInterval> {
    let required_overlap = required_visible_overlap(projected.extent[axis]);
    projected_pan_interval_with_requirement(projected, axis, required_overlap)
}

fn projected_core_pan_interval(
    projected: &ProjectedRestBounds,
    axis: usize,
) -> Option<PanInterval> {
    let required_overlap = required_core_visible_overlap(projected.extent[axis]);
    projected_pan_interval_with_requirement(projected, axis, required_overlap)
}

fn intersect_pan_intervals(first: PanInterval, second: PanInterval) -> Option<PanInterval> {
    let interval = PanInterval {
        min: first.min.max(second.min),
        max: first.max.min(second.max),
    };
    interval.is_valid().then_some(interval)
}

fn projected_pan_intervals(
    camera: Camera,
    distance: f32,
    fov_y: f32,
    aspect: f32,
    corners: &[Vec3; 8],
    horizontal_core_corners: &[Vec3; 8],
) -> Option<[PanInterval; 2]> {
    let projected = project_rest_bounds(camera, distance, fov_y, aspect, corners)?;
    let projected_horizontal_core =
        project_rest_bounds(camera, distance, fov_y, aspect, horizontal_core_corners)?;
    let horizontal = intersect_pan_intervals(
        projected_pan_interval(&projected, 0)?,
        projected_core_pan_interval(&projected_horizontal_core, 0)?,
    )?;
    Some([horizontal, projected_pan_interval(&projected, 1)?])
}

fn pan_intervals_for_effective_state(
    aabb: (Vec3, Vec3),
    orientation: CameraOrientation,
    aspect: f32,
) -> Option<[PanInterval; 2]> {
    if !aabb_is_finite(aabb) {
        return None;
    }

    let sanitized_aabb = sanitize_model_aabb(aabb);
    projected_pan_intervals(
        orientation.camera,
        orientation.distance,
        orientation.frame.fov_y,
        aspect,
        &rest_bounds_corners(sanitized_aabb),
        &rest_bounds_corners(horizontal_core_aabb(sanitized_aabb)),
    )
}

fn clamp_pan_axis(pan_ndc: f32, interval: PanInterval) -> f32 {
    if pan_ndc.is_finite() && interval.is_valid() {
        pan_ndc.clamp(interval.min, interval.max)
    } else {
        pan_ndc
    }
}

fn validated_pan_ndc(
    aabb: (Vec3, Vec3),
    orientation: CameraOrientation,
    pan_ndc: Vec2,
    aspect: f32,
) -> Vec2 {
    let Some(intervals) = pan_intervals_for_effective_state(aabb, orientation, aspect) else {
        return pan_ndc;
    };

    Vec2::new(
        clamp_pan_axis(pan_ndc.x, intervals[0]),
        clamp_pan_axis(pan_ndc.y, intervals[1]),
    )
}

fn validate_pan(
    aabb: (Vec3, Vec3),
    base_settings: CameraSettings,
    adjustments: CameraRuntimeAdjustments,
    viewport_aspect: f32,
) -> CameraRuntimeAdjustments {
    let adjustments = adjustments.sanitized();
    let orientation = resolve_camera_orientation(aabb, base_settings, adjustments);
    let pan_ndc = validated_pan_ndc(
        aabb,
        orientation,
        adjustments.pan_ndc,
        valid_viewport_aspect(viewport_aspect),
    );

    CameraRuntimeAdjustments {
        pan_ndc,
        ..adjustments
    }
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
    let zero_pan = resolve_camera_orientation(aabb, base_settings, zero_pan_adjustments);
    let camera = zero_pan.camera;
    let (min, max) = sanitize_model_aabb(aabb);
    let witness = (min + max) * 0.5;
    let distance = zero_pan.distance;
    let depth = (witness - zero_pan.camera.pos).dot(camera.forward());
    let depth_epsilon = distance * PAN_WITNESS_DEPTH_EPSILON_RATIO;
    if !distance.is_finite() || distance <= 0.0 || !depth.is_finite() || depth <= depth_epsilon {
        return None;
    }

    let baseline_ndc = camera.view_proj(aspect).project_point3(witness).truncate();
    let response = distance / depth;
    if !baseline_ndc.is_finite() || !response.is_finite() || response <= 0.0 {
        return None;
    }

    let pan_intervals = pan_intervals_for_effective_state(aabb, zero_pan, aspect)?;

    Some(PanWitnessProjection {
        baseline_ndc,
        response,
        pan_intervals,
    })
}

fn admit_pan_axis(
    current_pan_ndc: f32,
    desired_witness_delta_ndc: f32,
    response: f32,
    interval: PanInterval,
) -> f32 {
    if desired_witness_delta_ndc == 0.0 {
        return current_pan_ndc;
    }
    if !current_pan_ndc.is_finite()
        || !desired_witness_delta_ndc.is_finite()
        || !response.is_finite()
        || response <= 0.0
        || !interval.is_valid()
    {
        return current_pan_ndc;
    }

    // Correct stale state before admitting any new movement. This keeps the
    // same projected-bounds interval invariant for both input and reframe.
    let current_pan_ndc = clamp_pan_axis(current_pan_ndc, interval);
    // Pan input is expressed in the rest-center witness space. Convert it to
    // stored target-plane NDC so perspective does not change the input rate.
    let delta_pan_ndc = desired_witness_delta_ndc / response;
    let requested_pan_ndc = current_pan_ndc + delta_pan_ndc;
    if !requested_pan_ndc.is_finite() {
        return current_pan_ndc;
    }

    let next_pan_ndc = requested_pan_ndc.clamp(interval.min, interval.max);
    if next_pan_ndc.is_finite() {
        next_pan_ndc
    } else {
        current_pan_ndc
    }
}

/// Admit newly requested witness-space movement against the current camera
/// policy, correcting stale state when the shared interval solver says it is
/// outside the valid range.
pub(super) fn admit_pan_input(
    aabb: (Vec3, Vec3),
    base_settings: CameraSettings,
    adjustments: CameraRuntimeAdjustments,
    viewport_aspect: f32,
    desired_witness_delta_ndc: Vec2,
) -> Vec2 {
    let current = adjustments.sanitized().pan_ndc;
    let Some(projection) =
        resolve_pan_witness_projection(aabb, base_settings, adjustments, viewport_aspect)
    else {
        return current;
    };
    Vec2::new(
        admit_pan_axis(
            current.x,
            desired_witness_delta_ndc.x,
            projection.response,
            projection.pan_intervals[0],
        ),
        admit_pan_axis(
            current.y,
            desired_witness_delta_ndc.y,
            projection.response,
            projection.pan_intervals[1],
        ),
    )
}

#[cfg(test)]
mod tests;
