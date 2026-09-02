use super::*;

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
        let final_camera = resolve_camera_parameters_with_aspect(aabb, settings, admitted, aspect);
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
        let final_camera = resolve_camera_parameters_with_aspect(aabb, settings, admitted, aspect);
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
        let baseline_camera = resolve_camera_parameters_with_aspect(aabb, settings, pose, aspect);
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
    let outward = adjustments_after_pan_input(aabb, settings, pose, aspect, Vec2::new(0.0, 0.25));
    let outward_camera = resolve_camera_parameters_with_aspect(aabb, settings, outward, aspect);
    approx_eq(
        projected_rest_center(outward_camera, aabb, aspect).y,
        baseline.y + 0.25,
    );
    let stopped = adjustments_after_pan_input(aabb, settings, pose, aspect, Vec2::new(0.0, 10.0));
    let stopped_camera = resolve_camera_parameters_with_aspect(aabb, settings, stopped, aspect);
    approx_eq(
        projected_rest_center(stopped_camera, aabb, aspect).y,
        safe_limits.y,
    );
    let inward = adjustments_after_pan_input(aabb, settings, pose, aspect, Vec2::new(0.0, -0.25));
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

    let outward = adjustments_after_pan_input(aabb, settings, pose, aspect, Vec2::new(0.0, -0.25));
    let outward_camera = resolve_camera_parameters_with_aspect(aabb, settings, outward, aspect);
    approx_eq(
        projected_rest_center(outward_camera, aabb, aspect).y,
        baseline.y - 0.25,
    );
    let stopped = adjustments_after_pan_input(aabb, settings, pose, aspect, Vec2::new(0.0, -10.0));
    let stopped_camera = resolve_camera_parameters_with_aspect(aabb, settings, stopped, aspect);
    approx_eq(
        projected_rest_center(stopped_camera, aabb, aspect).y,
        -safe_limits.y,
    );
    let inward = adjustments_after_pan_input(aabb, settings, pose, aspect, Vec2::new(0.0, 0.25));
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
                ..CameraSettings::default()
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
        let parameters = resolve_camera_parameters_with_aspect(aabb, settings, adjustments, aspect);
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
    let saturated_camera = resolve_camera_parameters_with_aspect(aabb, settings, saturated, aspect);
    approx_vec2(
        projected_rest_center(saturated_camera, aabb, aspect),
        Vec2::new(safe_limits.x, -safe_limits.y),
    );

    let x_reversed =
        adjustments_after_pan_input(aabb, settings, saturated, aspect, Vec2::new(-0.2, 0.0));
    assert_eq!(x_reversed.pan_ndc.y, saturated.pan_ndc.y);
    let reversed_camera = resolve_camera_parameters_with_aspect(aabb, settings, x_reversed, aspect);
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
    let reversed_camera = resolve_camera_parameters_with_aspect(aabb, settings, reversed, aspect);
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
