use super::controls::{CameraControls, CameraSnapSteps};
use super::*;
use pocket3d::input::Input;
use pocket3d::winit::keyboard::KeyCode;

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
        ..CameraSettings::default()
    }
}

fn outside_pan_test_settings() -> CameraSettings {
    CameraSettings {
        fov_deg: 40.0,
        distance_scale: 0.3,
        headroom: 0.49,
        ..CameraSettings::default()
    }
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
    let changed_zero = resolve_camera_parameters_with_aspect(aabb, settings, changed_pose, aspect);
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
    let parameters =
        resolve_camera_parameters_with_aspect(aabb, settings, adjustments, DEFAULT_VIEWPORT_ASPECT);
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

fn projected_rest_center(parameters: CameraParameters, aabb: (Vec3, Vec3), aspect: f32) -> Vec2 {
    projected_point(parameters, rest_bounds_center(aabb), aspect).truncate()
}

fn projected_rest_bounds_y(
    parameters: CameraParameters,
    aabb: (Vec3, Vec3),
    aspect: f32,
) -> (f32, f32) {
    projected_rest_bounds_axis(parameters, aabb, aspect, 1)
}

fn projected_rest_bounds_x(
    parameters: CameraParameters,
    aabb: (Vec3, Vec3),
    aspect: f32,
) -> (f32, f32) {
    projected_rest_bounds_axis(parameters, aabb, aspect, 0)
}

fn projected_rest_bounds_axis(
    parameters: CameraParameters,
    aabb: (Vec3, Vec3),
    aspect: f32,
    axis: usize,
) -> (f32, f32) {
    rest_bounds_corners(sanitize_model_aabb(aabb))
        .into_iter()
        .map(|corner| projected_point(parameters, corner, aspect)[axis])
        .fold(
            (f32::INFINITY, f32::NEG_INFINITY),
            |(min_value, max_value), value| (min_value.min(value), max_value.max(value)),
        )
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
fn plain_vertical_pan_moves_resolved_camera_along_requested_screen_axis() {
    fn resolved_after(key: KeyCode) -> CameraParameters {
        let aabb = standard_pan_aabb();
        let settings = in_range_pan_test_settings();
        let aspect = DEFAULT_VIEWPORT_ASPECT;
        let snap_steps = CameraSnapSteps {
            yaw_deg: settings.yaw_snap_deg,
            roll_deg: settings.roll_snap_deg,
            pitch_deg: settings.pitch_snap_deg,
        };
        let context = CameraPanContext::new(aabb, settings);
        let mut controls = CameraControls::default();

        let mut toggle = Input::default();
        toggle.inject_key(KeyCode::F8, true);
        controls.apply_frame(0.0, &toggle, Some(context), snap_steps, aspect);

        let mut input = Input::default();
        input.inject_key(key, true);
        controls.apply_frame(1.0 / 60.0, &input, Some(context), snap_steps, aspect);

        resolve_camera_parameters_with_aspect(aabb, settings, controls.adjustments(), aspect)
    }

    let aabb = standard_pan_aabb();
    let settings = in_range_pan_test_settings();
    let aspect = DEFAULT_VIEWPORT_ASPECT;
    let baseline = resolve_camera_parameters_with_aspect(
        aabb,
        settings,
        CameraRuntimeAdjustments::default(),
        aspect,
    );
    let screen_up = camera_for_parameters(baseline).screen_up();
    let up = resolved_after(KeyCode::ArrowUp);
    let down = resolved_after(KeyCode::ArrowDown);
    let up_delta = up.position - baseline.position;
    let down_delta = down.position - baseline.position;

    assert!(
        up_delta.dot(screen_up) > 0.0,
        "ArrowUp camera delta {up_delta:?} did not move along screen-up {screen_up:?}"
    );
    assert!(
        down_delta.dot(screen_up) < 0.0,
        "ArrowDown camera delta {down_delta:?} did not move along screen-down {screen_up:?}"
    );
}

#[test]
fn baseline_inside_region_has_projected_bounds_stoppers() {
    let aabb = standard_pan_aabb();
    let settings = in_range_pan_test_settings();
    let aspect = 1.2;
    let zero = resolve_camera_parameters_with_aspect(
        aabb,
        settings,
        CameraRuntimeAdjustments::default(),
        aspect,
    );
    let intervals = pan_intervals(aabb, settings, CameraRuntimeAdjustments::default(), aspect);
    assert!(intervals.iter().all(|interval| interval.is_valid()));
    assert!(
        intervals
            .iter()
            .all(|interval| interval.min < 0.0 && interval.max > 0.0)
    );

    for axis in 0..2 {
        let zero_bounds = projected_rest_bounds_axis(zero, aabb, aspect, axis);
        let required = required_visible_overlap(zero_bounds.1 - zero_bounds.0);
        for direction in [-1.0, 1.0] {
            let mut request = Vec2::ZERO;
            request[axis] = direction * 10.0;
            let admitted = adjustments_after_pan_input(
                aabb,
                settings,
                CameraRuntimeAdjustments::default(),
                aspect,
                request,
            );
            let expected = if direction < 0.0 {
                intervals[axis].min
            } else {
                intervals[axis].max
            };
            approx_eq(admitted.pan_ndc[axis], expected);
            let final_camera =
                resolve_camera_parameters_with_aspect(aabb, settings, admitted, aspect);
            let final_bounds = projected_rest_bounds_axis(final_camera, aabb, aspect, axis);
            let visible = (final_bounds.1.min(1.0) - final_bounds.0.max(-1.0)).max(0.0);
            assert!(visible >= required.min(final_bounds.1 - final_bounds.0) - 1.0e-4);
        }
    }
}

#[test]
fn default_baseline_vertical_bounds_allow_inward_motion() {
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

fn visible_vertical_overlap((min_y, max_y): (f32, f32)) -> f32 {
    (max_y.min(1.0) - min_y.max(-1.0)).max(0.0)
}

fn existing_hybrid_visible_overlap(projected_extent: f32) -> f32 {
    projected_extent.min(
        (projected_extent * PAN_VISIBLE_FRACTION)
            .clamp(PAN_MIN_VISIBLE_OVERLAP_NDC, PAN_MAX_VISIBLE_OVERLAP_NDC),
    )
}

fn vertical_interval(
    aabb: (Vec3, Vec3),
    settings: CameraSettings,
    adjustments: CameraRuntimeAdjustments,
    aspect: f32,
) -> PanInterval {
    pan_intervals(aabb, settings, adjustments, aspect)[1]
}

fn horizontal_interval(
    aabb: (Vec3, Vec3),
    settings: CameraSettings,
    adjustments: CameraRuntimeAdjustments,
    aspect: f32,
) -> PanInterval {
    pan_intervals(aabb, settings, adjustments, aspect)[0]
}

fn full_rest_horizontal_interval(
    aabb: (Vec3, Vec3),
    settings: CameraSettings,
    adjustments: CameraRuntimeAdjustments,
    aspect: f32,
) -> PanInterval {
    let orientation = resolve_camera_orientation(aabb, settings, adjustments);
    let projected = project_rest_bounds(
        orientation.camera,
        orientation.distance,
        orientation.frame.fov_y,
        aspect,
        &rest_bounds_corners(sanitize_model_aabb(aabb)),
    )
    .unwrap();
    projected_pan_interval(&projected, 0).unwrap()
}

fn pan_intervals(
    aabb: (Vec3, Vec3),
    settings: CameraSettings,
    adjustments: CameraRuntimeAdjustments,
    aspect: f32,
) -> [PanInterval; 2] {
    resolve_pan_witness_projection(aabb, settings, adjustments, aspect)
        .unwrap()
        .pan_intervals
}

fn legacy_vertical_interval(
    aabb: (Vec3, Vec3),
    settings: CameraSettings,
    adjustments: CameraRuntimeAdjustments,
    aspect: f32,
) -> PanInterval {
    let orientation = resolve_camera_orientation(aabb, settings, adjustments.sanitized());
    let corners = rest_bounds_corners(sanitize_model_aabb(aabb));
    let view_proj = orientation.camera.view_proj(valid_viewport_aspect(aspect));
    let forward = orientation.camera.forward();
    let mut projected_y = [0.0; 8];
    let mut responses = [0.0; 8];
    let mut min_y = f32::INFINITY;
    let mut max_y = f32::NEG_INFINITY;

    for (index, corner) in corners.iter().enumerate() {
        let depth = (*corner - orientation.camera.pos).dot(forward);
        projected_y[index] = view_proj.project_point3(*corner).y;
        responses[index] = orientation.distance / depth;
        min_y = min_y.min(projected_y[index]);
        max_y = max_y.max(projected_y[index]);
    }

    let required_overlap = existing_hybrid_visible_overlap(max_y - min_y);
    let viewport_min = -1.0 + required_overlap;
    let viewport_max = 1.0 - required_overlap;
    let mut min_pan = f32::INFINITY;
    let mut max_pan = f32::NEG_INFINITY;

    for index in 0..corners.len() {
        min_pan = min_pan.min((viewport_min - projected_y[index]) / responses[index]);
        max_pan = max_pan.max((viewport_max - projected_y[index]) / responses[index]);
    }

    PanInterval {
        min: min_pan,
        max: max_pan,
    }
}

#[test]
fn close_zoom_expands_vertical_outward_travel_from_projected_bounds() {
    let aabb = standard_pan_aabb();
    let settings = CameraSettings::default();
    let aspect = DEFAULT_VIEWPORT_ASPECT;
    let baseline_interval =
        vertical_interval(aabb, settings, CameraRuntimeAdjustments::default(), aspect);
    let close_pose = CameraRuntimeAdjustments {
        distance_scale_delta: 0.279 - settings.distance_scale,
        ..Default::default()
    };
    let close_interval = vertical_interval(aabb, settings, close_pose, aspect);

    assert!(baseline_interval.is_valid());
    assert!(close_interval.is_valid());
    let baseline_outward_down = -baseline_interval.min;
    let close_outward_down = -close_interval.min;
    assert!(baseline_outward_down.is_finite() && close_outward_down.is_finite());
    assert!(close_outward_down > 0.5, "{close_interval:?}");
    assert!(
        close_outward_down >= baseline_outward_down,
        "baseline={baseline_interval:?}, close={close_interval:?}"
    );

    let admitted =
        adjustments_after_pan_input(aabb, settings, close_pose, aspect, Vec2::new(0.0, -10.0));
    assert_eq!(admitted.pan_ndc.y, close_interval.min);
    let stopped = resolve_camera_parameters_with_aspect(aabb, settings, admitted, aspect);
    assert!(
        visible_vertical_overlap(projected_rest_bounds_y(stopped, aabb, aspect))
            >= PAN_MIN_VISIBLE_OVERLAP_NDC - 1.0e-4
    );
}

#[test]
fn runtime_fov_changes_vertical_envelope_consistently() {
    let aabb = standard_pan_aabb();
    let settings = CameraSettings::default();
    let aspect = DEFAULT_VIEWPORT_ASPECT;
    let baseline = vertical_interval(aabb, settings, CameraRuntimeAdjustments::default(), aspect);
    let narrow = vertical_interval(
        aabb,
        settings,
        CameraRuntimeAdjustments {
            fov_delta_deg: -10.0,
            ..Default::default()
        },
        aspect,
    );
    let wide = vertical_interval(
        aabb,
        settings,
        CameraRuntimeAdjustments {
            fov_delta_deg: 20.0,
            ..Default::default()
        },
        aspect,
    );

    assert!(narrow.is_valid() && baseline.is_valid() && wide.is_valid());
    assert!(-narrow.min >= -baseline.min);
    assert!(-baseline.min >= -wide.min);
    assert_ne!(narrow, wide);
}

#[test]
fn orientation_changes_keep_vertical_limits_finite_and_ordered() {
    let aabb = standard_pan_aabb();
    let settings = CameraSettings::default();
    let aspect = 0.83;

    for yaw_deg in [-179.0, -73.0, 0.0, 91.0, 179.0] {
        for pitch_deg in [-80.0, -35.0, 0.0, 35.0, 80.0] {
            for roll_deg in [-179.0, -47.0, 0.0, 89.0, 179.0] {
                let adjustments = CameraRuntimeAdjustments {
                    yaw_deg,
                    pitch_deg,
                    roll_deg,
                    ..Default::default()
                };
                if let Some(projection) =
                    resolve_pan_witness_projection(aabb, settings, adjustments, aspect)
                {
                    assert!(
                        projection
                            .pan_intervals
                            .iter()
                            .all(|interval| interval.is_valid())
                    );
                }
                let admitted =
                    admit_pan_input(aabb, settings, adjustments, aspect, Vec2::new(0.0, 10.0));
                assert!(admitted.is_finite(), "{adjustments:?} -> {admitted:?}");
            }
        }
    }
}

#[test]
fn vertical_stoppers_keep_a_minimum_projected_rest_bounds_visible() {
    let aabb = standard_pan_aabb();
    let settings = CameraSettings::default();
    let aspect = DEFAULT_VIEWPORT_ASPECT;
    let poses = [
        CameraRuntimeAdjustments::default(),
        CameraRuntimeAdjustments {
            distance_scale_delta: 0.279 - settings.distance_scale,
            yaw_deg: 47.0,
            pitch_deg: -12.0,
            roll_deg: 31.0,
            ..Default::default()
        },
    ];

    for pose in poses {
        let Some(projection) = resolve_pan_witness_projection(aabb, settings, pose, aspect) else {
            continue;
        };
        let zero_camera = resolve_camera_parameters_with_aspect(aabb, settings, pose, aspect);
        let zero_bounds = projected_rest_bounds_y(zero_camera, aabb, aspect);
        let required_overlap = required_visible_overlap(zero_bounds.1 - zero_bounds.0);
        for desired_y in [-10.0, 10.0] {
            let admitted = adjustments_after_pan_input(
                aabb,
                settings,
                pose,
                aspect,
                Vec2::new(0.0, desired_y),
            );
            let final_camera =
                resolve_camera_parameters_with_aspect(aabb, settings, admitted, aspect);
            let final_bounds = projected_rest_bounds_y(final_camera, aabb, aspect);
            let final_projected_extent = final_bounds.1 - final_bounds.0;
            let guaranteed_overlap = required_overlap.min(final_projected_extent);
            assert!(
                visible_vertical_overlap(final_bounds) >= guaranteed_overlap - 1.0e-4,
                "pose={pose:?}, desired={desired_y}, interval={:?}, pan={:?}",
                projection.pan_intervals[1],
                admitted.pan_ndc
            );
        }
    }
}

#[test]
fn portrait_zero_roll_horizontal_stopper_preserves_projected_overlap() {
    let aabb = standard_pan_aabb();
    let settings = CameraSettings::default();
    let aspect = 540.0 / 960.0;
    let zero = resolve_camera_parameters_with_aspect(
        aabb,
        settings,
        CameraRuntimeAdjustments::default(),
        aspect,
    );
    let interval = horizontal_interval(aabb, settings, CameraRuntimeAdjustments::default(), aspect);
    let zero_bounds = projected_rest_bounds_x(zero, aabb, aspect);
    let required = required_visible_overlap(zero_bounds.1 - zero_bounds.0);
    assert!(interval.is_valid());

    for direction in [-1.0, 1.0] {
        let admitted = adjustments_after_pan_input(
            aabb,
            settings,
            CameraRuntimeAdjustments::default(),
            aspect,
            Vec2::new(direction * 10.0, 0.0),
        );
        let expected = if direction < 0.0 {
            interval.min
        } else {
            interval.max
        };
        approx_eq(admitted.pan_ndc.x, expected);
        let final_camera = resolve_camera_parameters_with_aspect(aabb, settings, admitted, aspect);
        let (min_x, max_x) = projected_rest_bounds_x(final_camera, aabb, aspect);
        assert!(
            (max_x.min(1.0) - min_x.max(-1.0)).max(0.0) >= required.min(max_x - min_x) - 1.0e-4
        );
    }

    assert!(projected_rest_center(zero, aabb, aspect).is_finite());
}

#[test]
fn portrait_roll_plus_and_minus_90_horizontal_intervals_mirror() {
    let aabb = standard_pan_aabb();
    let settings = CameraSettings::default();
    let aspect = 540.0 / 960.0;

    let intervals = [-90.0_f32, 90.0_f32].map(|roll_deg| {
        let pose = CameraRuntimeAdjustments {
            roll_deg,
            ..Default::default()
        };
        horizontal_interval(aabb, settings, pose, aspect)
    });

    assert!(intervals.iter().all(|interval| interval.is_valid()));
    approx_eq(intervals[0].min, -intervals[1].max);
    approx_eq(intervals[0].max, -intervals[1].min);
    assert!(
        intervals
            .iter()
            .all(|interval| interval.min < 0.0 && interval.max > 0.0)
    );
}

#[test]
fn close_zoom_expands_horizontal_traversal_from_projected_bounds() {
    let aabb = standard_pan_aabb();
    let settings = CameraSettings::default();
    let aspect = DEFAULT_VIEWPORT_ASPECT;
    let baseline = horizontal_interval(aabb, settings, CameraRuntimeAdjustments::default(), aspect);
    let close_pose = CameraRuntimeAdjustments {
        distance_scale_delta: 0.279 - settings.distance_scale,
        ..Default::default()
    };
    let close = horizontal_interval(aabb, settings, close_pose, aspect);

    assert!(baseline.is_valid() && close.is_valid());
    assert!(
        close.min < baseline.min,
        "baseline={baseline:?}, close={close:?}"
    );
    assert!(
        close.max > baseline.max,
        "baseline={baseline:?}, close={close:?}"
    );
}

#[test]
fn small_far_projected_model_keeps_horizontal_overlap_at_both_stoppers() {
    let aabb = (Vec3::new(-0.02, 0.0, -0.02), Vec3::new(0.02, 2.0, 0.02));
    let settings = CameraSettings {
        distance_scale: 2.0,
        ..CameraSettings::default()
    };
    let aspect = 1.6;
    let zero = resolve_camera_parameters_with_aspect(
        aabb,
        settings,
        CameraRuntimeAdjustments::default(),
        aspect,
    );
    let interval = horizontal_interval(aabb, settings, CameraRuntimeAdjustments::default(), aspect);
    let zero_bounds = projected_rest_bounds_x(zero, aabb, aspect);
    let required = required_visible_overlap(zero_bounds.1 - zero_bounds.0);

    for direction in [-1.0, 1.0] {
        let admitted = adjustments_after_pan_input(
            aabb,
            settings,
            CameraRuntimeAdjustments::default(),
            aspect,
            Vec2::new(direction * 100.0, 0.0),
        );
        let stopped = resolve_camera_parameters_with_aspect(aabb, settings, admitted, aspect);
        let bounds = projected_rest_bounds_x(stopped, aabb, aspect);
        let overlap = (bounds.1.min(1.0) - bounds.0.max(-1.0)).max(0.0);
        assert!(overlap >= required.min(bounds.1 - bounds.0) - 1.0e-4);
        assert_eq!(
            admitted.pan_ndc.x,
            if direction < 0.0 {
                interval.min
            } else {
                interval.max
            }
        );
    }
}

#[test]
fn outside_positive_baseline_has_projected_bounds_stopper() {
    let aabb = standard_pan_aabb();
    let settings = outside_pan_test_settings();
    let aspect = DEFAULT_VIEWPORT_ASPECT;
    let pose = CameraRuntimeAdjustments {
        roll_deg: 180.0,
        ..Default::default()
    };
    let baseline_camera = resolve_camera_parameters_with_aspect(aabb, settings, pose, aspect);
    let baseline = projected_rest_center(baseline_camera, aabb, aspect);
    let projection = resolve_pan_witness_projection(aabb, settings, pose, aspect).unwrap();
    let interval = projection.pan_intervals[1];
    assert!(baseline.y > 1.0, "{baseline:?}");
    assert!(interval.is_valid());

    let unchanged = adjustments_after_pan_input(aabb, settings, pose, aspect, Vec2::ZERO);
    assert_eq!(unchanged.pan_ndc, Vec2::ZERO);
    let outward = adjustments_after_pan_input(aabb, settings, pose, aspect, Vec2::new(0.0, 0.25));
    let outward_camera = resolve_camera_parameters_with_aspect(aabb, settings, outward, aspect);
    assert_eq!(outward.pan_ndc.y, 0.25_f32.min(interval.max));
    assert!(projected_rest_bounds_y(outward_camera, aabb, aspect).1 >= -1.0);
    let stopped = adjustments_after_pan_input(aabb, settings, pose, aspect, Vec2::new(0.0, 10.0));
    assert_eq!(stopped.pan_ndc.y, interval.max);
    let inward = adjustments_after_pan_input(aabb, settings, pose, aspect, Vec2::new(0.0, -0.25));
    let inward_camera = resolve_camera_parameters_with_aspect(aabb, settings, inward, aspect);
    assert_eq!(inward.pan_ndc.y, (-0.25_f32).max(interval.min));
    assert!(projected_rest_bounds_y(inward_camera, aabb, aspect).0 <= 1.0);
    assert!(inward.pan_ndc.y < 0.0);
}

#[test]
fn outside_negative_baseline_mirrors_projected_bounds_policy() {
    let aabb = standard_pan_aabb();
    let settings = outside_pan_test_settings();
    let aspect = DEFAULT_VIEWPORT_ASPECT;
    let pose = CameraRuntimeAdjustments::default();
    let baseline_camera = resolve_camera_parameters_with_aspect(aabb, settings, pose, aspect);
    let baseline = projected_rest_center(baseline_camera, aabb, aspect);
    let projection = resolve_pan_witness_projection(aabb, settings, pose, aspect).unwrap();
    let interval = projection.pan_intervals[1];
    assert!(baseline.y < -1.0, "{baseline:?}");
    assert!(interval.is_valid());

    let outward = adjustments_after_pan_input(aabb, settings, pose, aspect, Vec2::new(0.0, -0.25));
    let outward_camera = resolve_camera_parameters_with_aspect(aabb, settings, outward, aspect);
    assert_eq!(outward.pan_ndc.y, (-0.25_f32).max(interval.min));
    assert!(projected_rest_bounds_y(outward_camera, aabb, aspect).0 <= 1.0);
    let stopped = adjustments_after_pan_input(aabb, settings, pose, aspect, Vec2::new(0.0, -10.0));
    assert_eq!(stopped.pan_ndc.y, interval.min);
    let inward = adjustments_after_pan_input(aabb, settings, pose, aspect, Vec2::new(0.0, 0.25));
    let inward_camera = resolve_camera_parameters_with_aspect(aabb, settings, inward, aspect);
    assert_eq!(inward.pan_ndc.y, 0.25_f32.min(interval.max));
    assert!(projected_rest_bounds_y(inward_camera, aabb, aspect).1 >= -1.0);
    assert!(inward.pan_ndc.y > 0.0);
}

#[test]
fn close_pan_zoom_out_clamps_both_pan_axes() {
    let aabb = standard_pan_aabb();
    let settings = CameraSettings::default();
    let aspect = DEFAULT_VIEWPORT_ASPECT;
    let close_pose = CameraRuntimeAdjustments {
        distance_scale_delta: 0.30 - settings.distance_scale,
        ..Default::default()
    };
    let close_intervals = pan_intervals(aabb, settings, close_pose, aspect);
    for axis in 0..2 {
        for stored_value in [close_intervals[axis].min, close_intervals[axis].max] {
            let mut stored_pan = Vec2::ZERO;
            stored_pan[axis] = stored_value;
            let far_pose = CameraRuntimeAdjustments {
                distance_scale_delta: 2.00 - settings.distance_scale,
                pan_ndc: stored_pan,
                ..Default::default()
            };
            let far_intervals = pan_intervals(aabb, settings, far_pose, aspect);
            assert!(
                stored_pan[axis] < far_intervals[axis].min
                    || stored_pan[axis] > far_intervals[axis].max,
                "close boundary {stored_value} unexpectedly remained valid in {far_intervals:?}"
            );

            let mut controls = CameraControls::default();
            controls.set_adjustments(far_pose);
            assert!(controls.validate_pan(CameraPanContext::new(aabb, settings), aspect,));
            let corrected = controls.adjustments();
            let mut expected = stored_pan;
            expected[0] = expected[0].clamp(far_intervals[0].min, far_intervals[0].max);
            expected[1] = expected[1].clamp(far_intervals[1].min, far_intervals[1].max);
            assert_eq!(corrected.pan_ndc, expected);

            let resolved = resolve_camera_parameters_with_aspect(aabb, settings, far_pose, aspect);
            assert_eq!(resolved.pan_ndc, expected);
        }
    }
}

#[test]
fn zoom_input_is_not_blocked_by_vertical_pan_revalidation() {
    let aabb = standard_pan_aabb();
    let settings = CameraSettings::default();
    let aspect = DEFAULT_VIEWPORT_ASPECT;
    let close_pose = CameraRuntimeAdjustments {
        distance_scale_delta: 0.30 - settings.distance_scale,
        ..Default::default()
    };
    let close_interval = vertical_interval(aabb, settings, close_pose, aspect);
    let stored_pan = Vec2::new(0.37, close_interval.min);
    let initial = CameraRuntimeAdjustments {
        pan_ndc: stored_pan,
        ..close_pose
    };
    let mut controls = CameraControls::default();
    controls.set_adjustments(initial);
    let snap_steps = CameraSnapSteps {
        yaw_deg: settings.yaw_snap_deg,
        roll_deg: settings.roll_snap_deg,
        pitch_deg: settings.pitch_snap_deg,
    };
    let context = CameraPanContext::new(aabb, settings);

    let mut toggle = Input::default();
    toggle.inject_key(KeyCode::F8, true);
    controls.apply_frame(0.0, &toggle, Some(context), snap_steps, aspect);

    let mut input = Input::default();
    input.inject_key(KeyCode::ShiftLeft, true);
    input.inject_key(KeyCode::ArrowDown, true);
    assert!(controls.apply_frame(0.25, &input, Some(context), snap_steps, aspect));

    let changed = controls.adjustments();
    let expected_distance_delta = close_pose.distance_scale_delta + 0.75 * 0.25;
    let expected_pose = CameraRuntimeAdjustments {
        distance_scale_delta: expected_distance_delta,
        pan_ndc: stored_pan,
        ..Default::default()
    };
    let changed_interval = vertical_interval(aabb, settings, expected_pose, aspect);
    assert_eq!(changed.distance_scale_delta, expected_distance_delta);
    assert_eq!(changed.pan_ndc.x, stored_pan.x);
    assert_eq!(changed.pan_ndc.y, changed_interval.min);
}

#[test]
fn zoom_out_while_vertical_pan_is_valid_does_not_move_it() {
    let aabb = standard_pan_aabb();
    let settings = CameraSettings::default();
    let aspect = DEFAULT_VIEWPORT_ASPECT;
    let stored_pan = Vec2::new(0.73, 0.0);
    let far_pose = CameraRuntimeAdjustments {
        distance_scale_delta: 2.00 - settings.distance_scale,
        pan_ndc: stored_pan,
        ..Default::default()
    };
    let far_interval = vertical_interval(aabb, settings, far_pose, aspect);
    assert!(far_interval.min < stored_pan.y && stored_pan.y < far_interval.max);

    let mut controls = CameraControls::default();
    controls.set_adjustments(far_pose);
    assert!(!controls.validate_pan(CameraPanContext::new(aabb, settings), aspect,));
    assert_eq!(controls.adjustments().pan_ndc, stored_pan);
}

#[test]
fn fov_change_revalidates_and_clamps_both_pan_axes() {
    let aabb = standard_pan_aabb();
    let settings = CameraSettings::default();
    let aspect = DEFAULT_VIEWPORT_ASPECT;
    let close_pose = CameraRuntimeAdjustments {
        distance_scale_delta: 0.30 - settings.distance_scale,
        ..Default::default()
    };
    let close_intervals = pan_intervals(aabb, settings, close_pose, aspect);
    let stored_pan = Vec2::new(close_intervals[0].min, close_intervals[1].min);
    let changed = CameraRuntimeAdjustments {
        fov_delta_deg: 60.0,
        pan_ndc: stored_pan,
        ..close_pose
    };
    let changed_intervals = pan_intervals(aabb, settings, changed, aspect);
    assert!(stored_pan.x < changed_intervals[0].min);
    assert!(stored_pan.y < changed_intervals[1].min);

    let mut controls = CameraControls::default();
    controls.set_adjustments(changed);
    assert!(controls.validate_pan(CameraPanContext::new(aabb, settings), aspect,));
    assert_eq!(
        controls.adjustments().pan_ndc,
        Vec2::new(changed_intervals[0].min, changed_intervals[1].min)
    );
}

#[test]
fn yaw_pitch_and_roll_changes_revalidate_pan() {
    let aabb = standard_pan_aabb();
    let settings = CameraSettings::default();
    let aspect = DEFAULT_VIEWPORT_ASPECT;
    let cases = [
        (
            "yaw",
            CameraRuntimeAdjustments {
                yaw_deg: 45.0,
                ..Default::default()
            },
            CameraRuntimeAdjustments::default(),
        ),
        (
            "pitch",
            CameraRuntimeAdjustments {
                pitch_deg: -20.0,
                ..Default::default()
            },
            CameraRuntimeAdjustments::default(),
        ),
        (
            "roll",
            CameraRuntimeAdjustments {
                roll_deg: 90.0,
                ..Default::default()
            },
            CameraRuntimeAdjustments::default(),
        ),
    ];

    for (axis, before_pose, after_pose) in cases {
        let before_interval = vertical_interval(aabb, settings, before_pose, aspect);
        let stored_pan = Vec2::new(0.19, before_interval.min);
        let before = CameraRuntimeAdjustments {
            pan_ndc: stored_pan,
            ..before_pose
        };
        let after = CameraRuntimeAdjustments {
            pan_ndc: stored_pan,
            ..after_pose
        };
        let after_interval = vertical_interval(aabb, settings, after, aspect);
        assert!(
            stored_pan.y < after_interval.min,
            "{axis} did not make the lower boundary tighter: before={before_interval:?}, after={after_interval:?}"
        );
        assert_eq!(
            resolve_camera_parameters_with_aspect(aabb, settings, before, aspect).pan_ndc,
            stored_pan
        );

        let mut controls = CameraControls::default();
        controls.set_adjustments(after);
        assert!(controls.validate_pan(CameraPanContext::new(aabb, settings), aspect,));
        assert_eq!(controls.adjustments().pan_ndc.x, stored_pan.x);
        assert_eq!(controls.adjustments().pan_ndc.y, after_interval.min);
    }
}

#[test]
fn aspect_change_revalidates_horizontal_pan_without_moving_valid_y() {
    let aabb = standard_pan_aabb();
    let settings = CameraSettings::default();
    let old_aspect = 540.0 / 960.0;
    let new_aspect = 1920.0 / 1080.0;
    let old_interval = horizontal_interval(
        aabb,
        settings,
        CameraRuntimeAdjustments::default(),
        old_aspect,
    );
    let new_intervals = pan_intervals(
        aabb,
        settings,
        CameraRuntimeAdjustments::default(),
        new_aspect,
    );
    assert!(old_interval.max > new_intervals[0].max);
    let stored_pan = Vec2::new(old_interval.max, 0.2);
    assert!(stored_pan.x > new_intervals[0].max);

    let mut controls = CameraControls::default();
    controls.set_adjustments(CameraRuntimeAdjustments {
        pan_ndc: stored_pan,
        ..Default::default()
    });
    assert!(controls.validate_pan(CameraPanContext::new(aabb, settings), new_aspect));
    assert_eq!(controls.adjustments().pan_ndc.x, new_intervals[0].max);
    assert_eq!(controls.adjustments().pan_ndc.y, stored_pan.y);
}

#[test]
fn roll_change_revalidates_horizontal_pan_without_moving_valid_y() {
    let aabb = standard_pan_aabb();
    let settings = CameraSettings::default();
    let aspect = 540.0 / 960.0;
    let before_pose = CameraRuntimeAdjustments {
        roll_deg: 90.0,
        ..Default::default()
    };
    let before_interval = horizontal_interval(aabb, settings, before_pose, aspect);
    let after_pose = CameraRuntimeAdjustments {
        roll_deg: 0.0,
        pan_ndc: Vec2::new(before_interval.min, 0.2),
        ..Default::default()
    };
    let after_interval = horizontal_interval(aabb, settings, after_pose, aspect);
    assert!(after_pose.pan_ndc.x < after_interval.min);

    let mut controls = CameraControls::default();
    controls.set_adjustments(after_pose);
    assert!(controls.validate_pan(CameraPanContext::new(aabb, settings), aspect));
    assert_eq!(controls.adjustments().pan_ndc.x, after_interval.min);
    assert_eq!(controls.adjustments().pan_ndc.y, 0.2);
}

#[test]
fn runtime_orientation_revalidation_clamps_both_axes_independently() {
    let aabb = standard_pan_aabb();
    let settings = CameraSettings::default();
    let aspect = DEFAULT_VIEWPORT_ASPECT;
    for pose in [
        CameraRuntimeAdjustments {
            yaw_deg: 45.0,
            ..Default::default()
        },
        CameraRuntimeAdjustments {
            pitch_deg: -20.0,
            ..Default::default()
        },
        CameraRuntimeAdjustments {
            roll_deg: 45.0,
            ..Default::default()
        },
    ] {
        let intervals = pan_intervals(aabb, settings, pose, aspect);
        let mut controls = CameraControls::default();
        controls.set_adjustments(CameraRuntimeAdjustments {
            pan_ndc: Vec2::new(100.0, -100.0),
            ..pose
        });
        assert!(controls.validate_pan(CameraPanContext::new(aabb, settings), aspect));
        assert_eq!(
            controls.adjustments().pan_ndc,
            Vec2::new(intervals[0].max, intervals[1].min)
        );
    }
}

#[test]
fn returning_to_close_zoom_does_not_restore_clamped_pan() {
    let aabb = standard_pan_aabb();
    let settings = CameraSettings::default();
    let aspect = DEFAULT_VIEWPORT_ASPECT;
    let close_pose = CameraRuntimeAdjustments {
        distance_scale_delta: 0.30 - settings.distance_scale,
        ..Default::default()
    };
    let close_interval = vertical_interval(aabb, settings, close_pose, aspect);
    let stored_pan = Vec2::new(0.27, close_interval.min);
    let far_pose = CameraRuntimeAdjustments {
        distance_scale_delta: 2.00 - settings.distance_scale,
        pan_ndc: stored_pan,
        ..Default::default()
    };
    let far_interval = vertical_interval(aabb, settings, far_pose, aspect);
    let mut controls = CameraControls::default();
    controls.set_adjustments(far_pose);
    assert!(controls.validate_pan(CameraPanContext::new(aabb, settings), aspect,));
    let clamped_pan = controls.adjustments().pan_ndc;
    assert_eq!(clamped_pan.y, far_interval.min);

    controls.set_adjustments(CameraRuntimeAdjustments {
        distance_scale_delta: 0.30 - settings.distance_scale,
        pan_ndc: clamped_pan,
        ..Default::default()
    });
    assert!(!controls.validate_pan(CameraPanContext::new(aabb, settings), aspect,));
    assert_eq!(controls.adjustments().pan_ndc, clamped_pan);
}

#[test]
fn nonzero_pan_has_no_safety_jump_through_roll_sweeps_and_wrap() {
    let aabb = standard_pan_aabb();
    let aspect = DEFAULT_VIEWPORT_ASPECT;
    let requested_pan = Vec2::new(0.43, -0.57);
    let rolls = [
        44.0, 44.5, 45.0, 45.5, 46.0, 89.0, 89.5, 90.0, 90.5, 91.0, -91.0, -90.5, -90.0, -89.5,
        -89.0, 179.0, 179.5, 179.9, 180.1, 180.5, 181.0, -181.0, -180.5, -180.1, -179.9, -179.5,
        -179.0,
    ];

    for roll_deg in rolls {
        let pose = CameraRuntimeAdjustments {
            roll_deg,
            ..Default::default()
        };
        let zero =
            resolve_camera_parameters_with_aspect(aabb, in_range_pan_test_settings(), pose, aspect);
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
    let zero = resolve_camera_parameters_with_aspect(aabb, CameraSettings::default(), pose, aspect);
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
    let intervals = pan_intervals(aabb, settings, CameraRuntimeAdjustments::default(), aspect);
    let saturated = adjustments_after_pan_input(
        aabb,
        settings,
        CameraRuntimeAdjustments::default(),
        aspect,
        Vec2::new(10.0, -10.0),
    );
    assert_eq!(saturated.pan_ndc.x, intervals[0].max);
    assert_eq!(saturated.pan_ndc.y, intervals[1].min);

    let x_reversed =
        adjustments_after_pan_input(aabb, settings, saturated, aspect, Vec2::new(-0.2, 0.0));
    assert_eq!(x_reversed.pan_ndc.y, saturated.pan_ndc.y);
    approx_eq(x_reversed.pan_ndc.x, intervals[0].max - 0.2);
    assert_eq!(x_reversed.pan_ndc.y, saturated.pan_ndc.y);
}

#[test]
fn perspective_pan_input_preserves_witness_rate_on_both_axes() {
    let aabb = standard_pan_aabb();
    let settings = in_range_pan_test_settings();
    let aspect = 1.25;
    let pose = CameraRuntimeAdjustments {
        yaw_deg: 31.0,
        pitch_deg: -12.0,
        roll_deg: 23.0,
        ..Default::default()
    };
    let projection = resolve_pan_witness_projection(aabb, settings, pose, aspect).unwrap();
    assert!((projection.response - 1.0).abs() > 1.0e-3);

    let desired = Vec2::new(0.12, -0.08);
    let admitted = adjustments_after_pan_input(aabb, settings, pose, aspect, desired);
    approx_vec2(admitted.pan_ndc, desired / projection.response);

    let zero = resolve_camera_parameters_with_aspect(aabb, settings, pose, aspect);
    let moved = resolve_camera_parameters_with_aspect(
        aabb,
        settings,
        CameraRuntimeAdjustments {
            pan_ndc: admitted.pan_ndc,
            ..pose
        },
        aspect,
    );
    approx_vec2(
        projected_point(moved, rest_bounds_center(aabb), aspect).truncate()
            - projected_point(zero, rest_bounds_center(aabb), aspect).truncate(),
        desired,
    );
}

#[test]
fn simultaneous_x_and_y_pan_admission_is_independent() {
    let aabb = standard_pan_aabb();
    let settings = in_range_pan_test_settings();
    let aspect = 0.83;
    let pose = CameraRuntimeAdjustments {
        yaw_deg: 47.0,
        pitch_deg: -12.0,
        roll_deg: 31.0,
        ..Default::default()
    };
    let x_only = adjustments_after_pan_input(aabb, settings, pose, aspect, Vec2::new(0.11, 0.0));
    let y_only = adjustments_after_pan_input(aabb, settings, pose, aspect, Vec2::new(0.0, -0.09));
    let both = adjustments_after_pan_input(aabb, settings, pose, aspect, Vec2::new(0.11, -0.09));

    approx_eq(both.pan_ndc.x, x_only.pan_ndc.x);
    approx_eq(both.pan_ndc.y, y_only.pan_ndc.y);
}

#[test]
fn projected_intervals_are_finite_for_audit_rolls_aspects_and_zoom_ranges() {
    let aabb = standard_pan_aabb();
    let settings = CameraSettings::default();
    for aspect in [540.0 / 960.0, 1920.0 / 1080.0] {
        for roll_deg in [0.0, 45.0, 90.0, -90.0] {
            for distance_scale in [0.30, settings.distance_scale, 2.0] {
                let pose = CameraRuntimeAdjustments {
                    distance_scale_delta: distance_scale - settings.distance_scale,
                    roll_deg,
                    ..Default::default()
                };
                let intervals = pan_intervals(aabb, settings, pose, aspect);
                assert!(
                    intervals.iter().all(|interval| interval.is_valid()),
                    "aspect={aspect}, roll={roll_deg}, distance_scale={distance_scale}: {intervals:?}"
                );
            }
        }
    }
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
    let interval = horizontal_interval(aabb, settings, CameraRuntimeAdjustments::default(), aspect);
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
    approx_eq(reversed.pan_ndc.x, interval.max - 0.1);
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
    let (min_y, max_y) = projected_rest_bounds_y(reset_camera, aabb, aspect);
    assert!((max_y.min(1.0) - min_y.max(-1.0)).max(0.0) >= PAN_MIN_VISIBLE_OVERLAP_NDC);
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
                (translation / scale - reference).length() < 2.0e-4 * reference.length().max(1.0),
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
fn projected_bounds_use_fraction_floor_and_ceiling() {
    assert_eq!(required_visible_overlap(0.05), 0.05);
    assert_eq!(required_visible_overlap(0.20), 0.10);
    assert_eq!(required_visible_overlap(0.40), 0.10);
    assert_eq!(required_visible_overlap(1.00), 0.25);
    approx_eq(required_visible_overlap(3.00), 0.41296296);
    approx_eq(required_visible_overlap(5.00), 0.45);
    approx_eq(required_visible_overlap(10.00), 0.45);
}

#[test]
fn horizontal_core_uses_configured_width_fraction() {
    let (min, max) = horizontal_core_aabb(standard_pan_aabb());
    approx_vec3(min, Vec3::new(-0.04, 0.0, -0.2));
    approx_vec3(max, Vec3::new(0.24, 1.8, 0.4));
    let (aabb_min, aabb_max) = standard_pan_aabb();
    let full_width = aabb_max.x - aabb_min.x;
    let core_width = max.x - min.x;
    approx_eq(
        core_width / full_width,
        2.0 * PAN_X_CORE_HALF_WIDTH_FRACTION,
    );
    approx_eq(PAN_X_CORE_HALF_WIDTH_FRACTION, 0.140);
}

#[test]
fn zero_roll_horizontal_core_stays_visible_across_zoom_and_aspect() {
    let aabb = standard_pan_aabb();
    let core = horizontal_core_aabb(sanitize_model_aabb(aabb));
    let settings = CameraSettings::default();

    for aspect in [540.0 / 960.0, DEFAULT_VIEWPORT_ASPECT, 1920.0 / 1080.0] {
        for distance_scale in [0.30, 0.60, 1.20] {
            let pose = CameraRuntimeAdjustments {
                distance_scale_delta: distance_scale - settings.distance_scale,
                ..Default::default()
            };
            let zero = resolve_camera_parameters_with_aspect(aabb, settings, pose, aspect);
            let zero_core_bounds = projected_rest_bounds_axis(zero, core, aspect, 0);
            let required = required_core_visible_overlap(zero_core_bounds.1 - zero_core_bounds.0);
            let constrained = horizontal_interval(aabb, settings, pose, aspect);
            let full_rest = full_rest_horizontal_interval(aabb, settings, pose, aspect);
            assert!(
                constrained.min > full_rest.min && constrained.max < full_rest.max,
                "aspect={aspect}, distance={distance_scale}, core={constrained:?}, full={full_rest:?}"
            );

            for direction in [-1.0, 1.0] {
                let admitted = adjustments_after_pan_input(
                    aabb,
                    settings,
                    pose,
                    aspect,
                    Vec2::new(direction * 100.0, 0.0),
                );
                let stopped =
                    resolve_camera_parameters_with_aspect(aabb, settings, admitted, aspect);
                let bounds = projected_rest_bounds_axis(stopped, core, aspect, 0);
                let overlap = (bounds.1.min(1.0) - bounds.0.max(-1.0)).max(0.0);
                assert!(
                    overlap >= required.min(bounds.1 - bounds.0) - 1.0e-4,
                    "aspect={aspect}, distance={distance_scale}, direction={direction}, bounds={bounds:?}"
                );
            }
        }
    }
}

#[test]
fn roll_90_horizontal_core_preserves_full_height_traversal() {
    let aabb = standard_pan_aabb();
    let settings = CameraSettings::default();

    for aspect in [540.0 / 960.0, DEFAULT_VIEWPORT_ASPECT, 1920.0 / 1080.0] {
        for distance_scale in [0.30, 0.60, 1.20] {
            for roll_deg in [-90.0, 90.0] {
                let pose = CameraRuntimeAdjustments {
                    distance_scale_delta: distance_scale - settings.distance_scale,
                    roll_deg,
                    ..Default::default()
                };
                let constrained = horizontal_interval(aabb, settings, pose, aspect);
                let full_rest = full_rest_horizontal_interval(aabb, settings, pose, aspect);
                approx_eq(constrained.min, full_rest.min);
                approx_eq(constrained.max, full_rest.max);
            }
        }
    }
}

#[test]
fn close_zoom_ceiling_keeps_existing_behavior_at_or_below_start_extent() {
    for extent in [0.05, 0.20, 0.40, 1.00, 1.80, 2.00] {
        approx_eq(
            required_visible_overlap(extent),
            existing_hybrid_visible_overlap(extent),
        );
    }
}

#[test]
fn close_zoom_ceiling_reaches_the_close_cap_at_or_above_end_extent() {
    for extent in [5.00, 5.50, 10.00] {
        approx_eq(
            required_visible_overlap(extent),
            PAN_CLOSE_VISIBLE_OVERLAP_NDC,
        );
    }
    assert_eq!(PAN_CLOSE_VISIBLE_OVERLAP_NDC, 0.45);
}

#[test]
fn horizontal_core_ignores_close_strengthening() {
    for extent in [0.05, 0.20, 0.40, 1.00, 1.80, 2.00, 3.00, 5.00, 10.00] {
        approx_eq(
            required_core_visible_overlap(extent),
            existing_hybrid_visible_overlap(extent),
        );
    }
    approx_eq(required_core_visible_overlap(3.00), 0.40);
    approx_eq(required_core_visible_overlap(5.00), 0.40);
    approx_eq(required_core_visible_overlap(10.00), 0.40);
}

#[test]
fn full_bounds_close_requirement_is_axis_neutral() {
    // The full-bounds policy is a single shared function of projected extent.
    // There must be no per-axis close constants: the same extent demands the
    // same overlap on X and Y, so roll=90 (which swaps anatomical axes) can
    // not change the safety policy.
    for extent in [0.40, 1.00, 2.00, 3.00, 5.00, 10.00] {
        approx_eq(
            required_visible_overlap(extent),
            required_visible_overlap(extent),
        );
        assert!(
            required_core_visible_overlap(extent) <= required_visible_overlap(extent) + 1.0e-6
                || extent <= PAN_CLOSE_STRENGTHEN_START_EXTENT_NDC + 1.0e-6
        );
    }

    let aabb = standard_pan_aabb();
    let settings = CameraSettings::default();
    let aspect = 540.0 / 960.0;
    for roll_deg in [0.0, 90.0, -90.0] {
        let pose = CameraRuntimeAdjustments {
            roll_deg,
            ..Default::default()
        };
        let zero = resolve_camera_parameters_with_aspect(aabb, settings, pose, aspect);
        for axis in 0..2 {
            let bounds = projected_rest_bounds_axis(zero, aabb, aspect, axis);
            let required = required_visible_overlap(bounds.1 - bounds.0);
            for direction in [-1.0, 1.0] {
                let mut request = Vec2::ZERO;
                request[axis] = direction * 100.0;
                let admitted = adjustments_after_pan_input(aabb, settings, pose, aspect, request);
                let stopped =
                    resolve_camera_parameters_with_aspect(aabb, settings, admitted, aspect);
                let final_bounds = projected_rest_bounds_axis(stopped, aabb, aspect, axis);
                let overlap = (final_bounds.1.min(1.0) - final_bounds.0.max(-1.0)).max(0.0);
                assert!(
                    overlap >= required.min(final_bounds.1 - final_bounds.0) - 1.0e-4,
                    "roll={roll_deg}, axis={axis}, direction={direction}, bounds={final_bounds:?}"
                );
            }
        }
    }
}

#[test]
fn admission_and_revalidation_share_close_requirements() {
    let aabb = standard_pan_aabb();
    let settings = CameraSettings::default();
    let aspect = DEFAULT_VIEWPORT_ASPECT;
    let close_pose = CameraRuntimeAdjustments {
        distance_scale_delta: 0.30 - settings.distance_scale,
        ..Default::default()
    };
    let intervals = pan_intervals(aabb, settings, close_pose, aspect);
    for axis in 0..2 {
        for direction in [-1.0, 1.0] {
            let mut request = Vec2::ZERO;
            request[axis] = direction * 10.0;
            let admitted = adjustments_after_pan_input(aabb, settings, close_pose, aspect, request);
            let expected = if direction < 0.0 {
                intervals[axis].min
            } else {
                intervals[axis].max
            };
            approx_eq(admitted.pan_ndc[axis], expected);
        }
    }
    // Revalidation of the admitted stopper must be a no-op: both paths use
    // the same required_visible_overlap / required_core_visible_overlap.
    for axis in 0..2 {
        for endpoint in [intervals[axis].min, intervals[axis].max] {
            let mut stored_pan = Vec2::ZERO;
            stored_pan[axis] = endpoint;
            let stored = CameraRuntimeAdjustments {
                pan_ndc: stored_pan,
                ..close_pose
            };
            let resolved = resolve_camera_parameters_with_aspect(aabb, settings, stored, aspect);
            assert_eq!(resolved.pan_ndc, stored_pan);
        }
    }
}

#[test]
fn close_zoom_ceiling_transition_is_continuous_and_monotonic() {
    let start = PAN_CLOSE_STRENGTHEN_START_EXTENT_NDC;
    let end = PAN_CLOSE_STRENGTHEN_END_EXTENT_NDC;
    let before_start = required_visible_overlap(start - 1.0e-4);
    let at_start = required_visible_overlap(start);
    let at_end = required_visible_overlap(end);
    let after_end = required_visible_overlap(end + 1.0e-4);

    approx_eq(before_start, at_start);
    approx_eq(at_end, after_end);

    let mut previous = at_start;
    let mut largest_step: f32 = 0.0;
    for index in 1..=300 {
        let extent = start + (end - start) * index as f32 / 300.0;
        let current = required_visible_overlap(extent);
        assert!(current + 1.0e-6 >= previous, "{previous} -> {current}");
        largest_step = largest_step.max(current - previous);
        previous = current;
    }
    assert!(
        largest_step < 0.002,
        "largest transition step: {largest_step}"
    );
}

#[test]
fn close_zoom_strengthens_the_vertical_interval_and_reduces_travel() {
    let aabb = standard_pan_aabb();
    let settings = CameraSettings::default();
    let aspect = DEFAULT_VIEWPORT_ASPECT;
    let close_pose = CameraRuntimeAdjustments {
        distance_scale_delta: 0.30 - settings.distance_scale,
        ..Default::default()
    };
    let zero = resolve_camera_parameters_with_aspect(aabb, settings, close_pose, aspect);
    let bounds = projected_rest_bounds_y(zero, aabb, aspect);
    let extent = bounds.1 - bounds.0;
    assert!(
        extent >= PAN_CLOSE_STRENGTHEN_END_EXTENT_NDC,
        "extent={extent}"
    );
    assert_eq!(
        required_visible_overlap(extent),
        PAN_CLOSE_VISIBLE_OVERLAP_NDC
    );

    let strengthened = vertical_interval(aabb, settings, close_pose, aspect);
    let legacy = legacy_vertical_interval(aabb, settings, close_pose, aspect);
    assert!(
        strengthened.min > legacy.min,
        "legacy={legacy:?}, strengthened={strengthened:?}"
    );
    assert!(
        strengthened.max < legacy.max,
        "legacy={legacy:?}, strengthened={strengthened:?}"
    );
}

#[test]
fn far_zoom_vertical_intervals_remain_unchanged() {
    let aabb = standard_pan_aabb();
    let settings = CameraSettings::default();
    let aspect = DEFAULT_VIEWPORT_ASPECT;

    for distance_scale in [1.80, 2.00] {
        let pose = CameraRuntimeAdjustments {
            distance_scale_delta: distance_scale - settings.distance_scale,
            ..Default::default()
        };
        let zero = resolve_camera_parameters_with_aspect(aabb, settings, pose, aspect);
        let bounds = projected_rest_bounds_y(zero, aabb, aspect);
        assert!(
            bounds.1 - bounds.0 <= PAN_CLOSE_STRENGTHEN_START_EXTENT_NDC,
            "distance_scale={distance_scale}, bounds={bounds:?}"
        );

        let actual = vertical_interval(aabb, settings, pose, aspect);
        let legacy = legacy_vertical_interval(aabb, settings, pose, aspect);
        approx_eq(actual.min, legacy.min);
        approx_eq(actual.max, legacy.max);
    }
}

#[test]
fn requested_distances_keep_the_projected_bounds_interval_meaningful() {
    let aabb = standard_pan_aabb();
    let settings = CameraSettings::default();
    let aspect = DEFAULT_VIEWPORT_ASPECT;

    for distance_scale in [0.30, 0.60, 1.20, 2.00] {
        let adjustments = CameraRuntimeAdjustments {
            distance_scale_delta: distance_scale - settings.distance_scale,
            ..Default::default()
        };
        let interval = vertical_interval(aabb, settings, adjustments, aspect);
        let parameters = resolve_camera_parameters_with_aspect(aabb, settings, adjustments, aspect);
        let zero_bounds = projected_rest_bounds_y(parameters, aabb, aspect);
        let required = required_visible_overlap(zero_bounds.1 - zero_bounds.0);

        assert!(interval.is_valid());
        for desired_y in [-10.0, 10.0] {
            let admitted = adjustments_after_pan_input(
                aabb,
                settings,
                adjustments,
                aspect,
                Vec2::new(0.0, desired_y),
            );
            let stopped = resolve_camera_parameters_with_aspect(aabb, settings, admitted, aspect);
            assert!(
                visible_vertical_overlap(projected_rest_bounds_y(stopped, aabb, aspect))
                    >= required - 1.0e-4,
                "distance_scale={distance_scale}, required={required}, interval={interval:?}"
            );
        }
    }
}

#[test]
fn projected_stoppers_preserve_required_overlap_on_both_axes_after_rolls() {
    let aabb = standard_pan_aabb();
    let settings = CameraSettings::default();
    let aspect = 540.0 / 960.0;
    for pose in [
        CameraRuntimeAdjustments {
            roll_deg: 45.0,
            ..Default::default()
        },
        CameraRuntimeAdjustments {
            roll_deg: 90.0,
            ..Default::default()
        },
        CameraRuntimeAdjustments {
            roll_deg: -90.0,
            ..Default::default()
        },
        CameraRuntimeAdjustments {
            yaw_deg: 31.0,
            pitch_deg: -12.0,
            roll_deg: 23.0,
            ..Default::default()
        },
    ] {
        let zero = resolve_camera_parameters_with_aspect(aabb, settings, pose, aspect);
        for axis in 0..2 {
            let zero_bounds = projected_rest_bounds_axis(zero, aabb, aspect, axis);
            let required = required_visible_overlap(zero_bounds.1 - zero_bounds.0);
            for direction in [-1.0, 1.0] {
                let mut request = Vec2::ZERO;
                request[axis] = direction * 100.0;
                let admitted = adjustments_after_pan_input(aabb, settings, pose, aspect, request);
                let stopped =
                    resolve_camera_parameters_with_aspect(aabb, settings, admitted, aspect);
                let bounds = projected_rest_bounds_axis(stopped, aabb, aspect, axis);
                let overlap = (bounds.1.min(1.0) - bounds.0.max(-1.0)).max(0.0);
                assert!(
                    overlap >= required.min(bounds.1 - bounds.0) - 1.0e-4,
                    "pose={pose:?}, axis={axis}, direction={direction}, bounds={bounds:?}"
                );
            }
        }
    }
}

#[test]
fn projected_interval_regression_values_match_audit_cases() {
    let aabb = standard_pan_aabb();
    let settings = CameraSettings::default();
    let cases = [
        (
            "portrait roll 0 baseline",
            540.0 / 960.0,
            0.0,
            0.0,
            0.0,
            0.0,
            (-1.3998301, 1.3998303),
            (-1.5111949, 4.4734893),
        ),
        (
            "portrait roll +90 baseline",
            540.0 / 960.0,
            0.0,
            0.0,
            90.0,
            0.0,
            (-7.4062653, 2.1399643),
            (-2.0059867, 2.0059869),
        ),
        (
            "portrait roll -90 baseline",
            540.0 / 960.0,
            0.0,
            0.0,
            -90.0,
            0.0,
            (-2.1399643, 7.4062653),
            (-2.0059872, 2.0059867),
        ),
        (
            "portrait roll +90 far",
            540.0 / 960.0,
            0.0,
            0.0,
            90.0,
            2.0 - settings.distance_scale,
            (-2.6542566, 1.074366),
            (-1.23944, 1.2394401),
        ),
        (
            "wide roll 0 baseline",
            1920.0 / 1080.0,
            0.0,
            0.0,
            0.0,
            0.0,
            (-1.3008934, 1.3008934),
            (-1.5111949, 4.4734893),
        ),
        (
            "wide roll +90 baseline",
            1920.0 / 1080.0,
            0.0,
            0.0,
            90.0,
            0.0,
            (-2.8536265, 1.187336),
            (-2.0059867, 2.0059869),
        ),
        (
            "nonzero yaw pitch roll",
            1.25,
            31.0,
            -12.0,
            23.0,
            0.0,
            (-2.5083506, 1.1935925),
            (-1.82348, 4.6816516),
        ),
    ];

    for (
        name,
        aspect,
        yaw_deg,
        pitch_deg,
        roll_deg,
        distance_scale_delta,
        expected_x,
        expected_y,
    ) in cases
    {
        let adjustments = CameraRuntimeAdjustments {
            yaw_deg,
            pitch_deg,
            roll_deg,
            distance_scale_delta,
            ..Default::default()
        };
        let projection =
            resolve_pan_witness_projection(aabb, settings, adjustments, aspect).unwrap();
        let actual_x = projection.pan_intervals[0];
        let actual_y = projection.pan_intervals[1];
        assert!(
            (actual_x.min - expected_x.0).abs() < 1.0e-4,
            "{name}: {actual_x:?}"
        );
        assert!(
            (actual_x.max - expected_x.1).abs() < 1.0e-4,
            "{name}: {actual_x:?}"
        );
        assert!(
            (actual_y.min - expected_y.0).abs() < 1.0e-4,
            "{name}: {actual_y:?}"
        );
        assert!(
            (actual_y.max - expected_y.1).abs() < 1.0e-4,
            "{name}: {actual_y:?}"
        );
    }
}
