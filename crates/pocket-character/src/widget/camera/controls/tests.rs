use super::*;

fn approx_eq(actual: f32, expected: f32) {
    assert!((actual - expected).abs() < 1.0e-5, "{actual} != {expected}");
}

fn test_widget() -> CameraControls {
    CameraControls::default()
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
    input.inject_key(KeyCode::AltLeft, true);
    input.inject_key(arrow, true);
    input
}

fn yaw_snap_test_input(arrow: KeyCode) -> Input {
    let mut input = Input::default();
    input.inject_key(KeyCode::AltLeft, true);
    input.inject_key(KeyCode::ShiftLeft, true);
    input.inject_key(arrow, true);
    input
}

fn pitch_snap_test_input(arrow: KeyCode) -> Input {
    let mut input = Input::default();
    input.inject_key(KeyCode::AltLeft, true);
    input.inject_key(KeyCode::ControlLeft, true);
    input.inject_key(arrow, true);
    input
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
fn f8_alt_alone_preserves_horizontal_pan_behavior() {
    let dt = 1.0 / 60.0;
    let mut input = Input::default();
    input.inject_key(KeyCode::AltLeft, true);
    input.inject_key(KeyCode::ArrowRight, true);
    assert_eq!(
        horizontal_camera_action(&input),
        HorizontalCameraAction::Pan
    );

    let delta = pan_witness_delta_after_keys(&[KeyCode::AltLeft, KeyCode::ArrowRight], dt);
    assert!(delta.x > 0.0);
    assert_eq!(delta.y, 0.0);
    assert_eq!(
        camera_adjustments_after_keys(&[KeyCode::AltLeft, KeyCode::ArrowRight]),
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
    assert!(shift_left.yaw_deg > 0.0);
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
            "Ctrl + Up / Down       Pitch up / down; Alt + Ctrl + Up / Down Snap pitch 15°",
            "Ctrl + Left/Right        Roll",
            "Alt + Ctrl + Left/Right Snap roll 15°; Alt + Shift + Left/Right Snap yaw 15°",
            "Q / E, [ / ]           Decrease / increase FOV",
            "R                      Reset runtime camera adjustments",
            "F8                     Toggle camera controls",
        ]
    );
}

#[test]
fn f8_fov_bracket_aliases_match_q_and_e() {
    let q = camera_adjustments_after_keys(&[KeyCode::KeyQ]);
    let left_bracket = camera_adjustments_after_keys(&[KeyCode::BracketLeft]);
    approx_eq(left_bracket.fov_delta_deg, q.fov_delta_deg);

    let e = camera_adjustments_after_keys(&[KeyCode::KeyE]);
    let right_bracket = camera_adjustments_after_keys(&[KeyCode::BracketRight]);
    approx_eq(right_bracket.fov_delta_deg, e.fov_delta_deg);
}

#[test]
fn f8_same_direction_fov_aliases_do_not_double_the_change() {
    let q = camera_adjustments_after_keys(&[KeyCode::KeyQ]);
    let q_and_left_bracket = camera_adjustments_after_keys(&[KeyCode::KeyQ, KeyCode::BracketLeft]);
    approx_eq(q_and_left_bracket.fov_delta_deg, q.fov_delta_deg);

    let e = camera_adjustments_after_keys(&[KeyCode::KeyE]);
    let e_and_right_bracket =
        camera_adjustments_after_keys(&[KeyCode::KeyE, KeyCode::BracketRight]);
    approx_eq(e_and_right_bracket.fov_delta_deg, e.fov_delta_deg);
}

#[test]
fn f8_mixed_fov_aliases_preserve_opposing_input_cancellation() {
    for keys in [
        [KeyCode::KeyQ, KeyCode::KeyE],
        [KeyCode::KeyQ, KeyCode::BracketRight],
        [KeyCode::BracketLeft, KeyCode::KeyE],
        [KeyCode::BracketLeft, KeyCode::BracketRight],
    ] {
        let adjustments = camera_adjustments_after_keys(&keys);
        assert_eq!(adjustments.fov_delta_deg, 0.0);
    }
}

#[test]
fn f8_fov_bracket_aliases_do_not_change_other_camera_controls() {
    for keys in [
        [KeyCode::BracketLeft, KeyCode::ArrowUp],
        [KeyCode::BracketRight, KeyCode::ArrowLeft],
        [KeyCode::BracketLeft, KeyCode::BracketRight],
    ] {
        let adjustments = camera_adjustments_after_keys(&keys);
        assert_eq!(adjustments.distance_scale_delta, 0.0);
        assert_eq!(adjustments.pan_ndc, Vec2::ZERO);
        assert_eq!(adjustments.yaw_deg, 0.0);
        assert_eq!(adjustments.roll_deg, 0.0);
        assert_eq!(adjustments.pitch_deg, 0.0);
    }
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
fn f8_vertical_modifier_precedence_selects_snap_pitch_first() {
    let mut input = Input::default();
    input.inject_key(KeyCode::ArrowUp, true);
    assert_eq!(vertical_camera_action(&input), VerticalCameraAction::Pan);

    input.inject_key(KeyCode::AltLeft, true);
    assert_eq!(vertical_camera_action(&input), VerticalCameraAction::Pan);

    input.inject_key(KeyCode::ShiftLeft, true);
    assert_eq!(vertical_camera_action(&input), VerticalCameraAction::Zoom);

    input.inject_key(KeyCode::ControlLeft, true);
    assert_eq!(
        vertical_camera_action(&input),
        VerticalCameraAction::SnapPitch
    );

    let adjustments = camera_adjustments_after_keys(&[
        KeyCode::AltLeft,
        KeyCode::ControlLeft,
        KeyCode::ShiftLeft,
        KeyCode::ArrowUp,
    ]);
    approx_eq(adjustments.pitch_deg, HORIZONTAL_SNAP_DEG);
    assert_eq!(adjustments.distance_scale_delta, 0.0);
    assert_eq!(adjustments.pan_ndc, Vec2::ZERO);
}

#[test]
fn f8_alt_alone_remains_vertical_pan() {
    let dt = 1.0 / 60.0;
    let mut input = Input::default();
    input.inject_key(KeyCode::AltLeft, true);
    input.inject_key(KeyCode::ArrowUp, true);
    assert_eq!(vertical_camera_action(&input), VerticalCameraAction::Pan);
    let delta = pan_witness_delta_after_keys(&[KeyCode::AltLeft, KeyCode::ArrowUp], dt);
    assert_eq!(delta.x, 0.0);
    assert!(delta.y > 0.0);
}

#[test]
fn f8_pitch_snap_selects_next_grid_point_for_off_grid_values() {
    for (current, expected_up, expected_down) in [
        (7.0, 15.0, 0.0),
        (-7.0, 0.0, -15.0),
        (22.0, 30.0, 15.0),
        (-22.0, -15.0, -30.0),
    ] {
        approx_eq(snap_pitch_degrees(current, 1), expected_up);
        approx_eq(snap_pitch_degrees(current, -1), expected_down);
    }
}

#[test]
fn f8_pitch_snap_exact_grid_values_always_advance_one_detent() {
    for (current, expected_up, expected_down) in [
        (0.0, 15.0, -15.0),
        (15.0, 30.0, 0.0),
        (-15.0, 0.0, -30.0),
        (30.0, 45.0, 15.0),
        (-30.0, -15.0, -45.0),
    ] {
        approx_eq(snap_pitch_degrees(current, 1), expected_up);
        approx_eq(snap_pitch_degrees(current, -1), expected_down);
    }
}

#[test]
fn f8_pitch_snap_direction_matches_continuous_pitch() {
    let snap_up =
        camera_adjustments_after_keys(&[KeyCode::AltLeft, KeyCode::ControlLeft, KeyCode::ArrowUp]);
    let continuous_up = {
        let mut widget = test_widget();
        widget.set_camera_adjustments(CameraRuntimeAdjustments {
            pitch_deg: 7.0,
            ..CameraRuntimeAdjustments::default()
        });
        let mut input = Input::default();
        input.inject_key(KeyCode::ControlLeft, true);
        input.inject_key(KeyCode::ArrowUp, true);
        widget.apply_camera_keyboard_controls(0.1, &input);
        widget.camera_adjustments.pitch_deg
    };
    assert!(snap_up.pitch_deg > 0.0);
    assert!(continuous_up > 7.0);

    let snap_down = {
        let mut widget = test_widget();
        widget.set_camera_adjustments(CameraRuntimeAdjustments {
            pitch_deg: 7.0,
            ..CameraRuntimeAdjustments::default()
        });
        let input = pitch_snap_test_input(KeyCode::ArrowDown);
        widget.apply_camera_keyboard_controls(0.0, &input);
        widget.camera_adjustments.pitch_deg
    };
    let continuous_down = {
        let mut widget = test_widget();
        widget.set_camera_adjustments(CameraRuntimeAdjustments {
            pitch_deg: 7.0,
            ..CameraRuntimeAdjustments::default()
        });
        let mut input = Input::default();
        input.inject_key(KeyCode::ControlLeft, true);
        input.inject_key(KeyCode::ArrowDown, true);
        widget.apply_camera_keyboard_controls(0.1, &input);
        widget.camera_adjustments.pitch_deg
    };
    assert!(snap_down < 7.0);
    assert!(continuous_down < 7.0);
}

#[test]
fn f8_pitch_snap_tap_emits_one_detent() {
    let mut widget = test_widget();
    widget.set_camera_adjustments(CameraRuntimeAdjustments {
        pitch_deg: 7.0,
        ..CameraRuntimeAdjustments::default()
    });
    let mut input = pitch_snap_test_input(KeyCode::ArrowUp);

    widget.apply_camera_keyboard_controls(0.0, &input);
    approx_eq(widget.camera_adjustments.pitch_deg, 15.0);

    input.end_frame();
    input.inject_key(KeyCode::ArrowUp, false);
    widget.apply_camera_keyboard_controls(0.0, &input);
    widget.apply_camera_keyboard_controls(1.0, &input);

    approx_eq(widget.camera_adjustments.pitch_deg, 15.0);
    assert_eq!(
        widget.vertical_snap_repeat,
        VerticalSnapRepeatState::default()
    );
}

#[test]
fn f8_pitch_snap_repeats_after_delay_at_the_configured_interval() {
    let mut widget = test_widget();
    widget.set_camera_adjustments(CameraRuntimeAdjustments {
        pitch_deg: 7.0,
        ..CameraRuntimeAdjustments::default()
    });
    let mut input = pitch_snap_test_input(KeyCode::ArrowUp);

    widget.apply_camera_keyboard_controls(0.0, &input);
    input.end_frame();
    widget.apply_camera_keyboard_controls(VERTICAL_SNAP_REPEAT_DELAY_SEC, &input);
    approx_eq(widget.camera_adjustments.pitch_deg, 30.0);
    widget.apply_camera_keyboard_controls(VERTICAL_SNAP_REPEAT_INTERVAL_SEC - 0.001, &input);
    approx_eq(widget.camera_adjustments.pitch_deg, 30.0);
    widget.apply_camera_keyboard_controls(0.001, &input);
    approx_eq(widget.camera_adjustments.pitch_deg, 45.0);
    widget.apply_camera_keyboard_controls(VERTICAL_SNAP_REPEAT_INTERVAL_SEC * 2.0, &input);
    approx_eq(widget.camera_adjustments.pitch_deg, 75.0);
}

#[test]
fn f8_pitch_snap_repeat_is_frame_rate_independent() {
    fn pitch_after_hold(dts: &[f32]) -> (f32, u32) {
        let mut widget = test_widget();
        widget.set_camera_adjustments(CameraRuntimeAdjustments {
            pitch_deg: 7.0,
            ..CameraRuntimeAdjustments::default()
        });
        let mut input = pitch_snap_test_input(KeyCode::ArrowUp);
        widget.apply_camera_keyboard_controls(0.0, &input);
        input.end_frame();
        for &dt in dts {
            widget.apply_camera_keyboard_controls(dt, &input);
        }
        (
            widget.camera_adjustments.pitch_deg,
            widget.vertical_snap_repeat.repeat_steps_emitted,
        )
    }

    let fine = pitch_after_hold(&[0.05; 13]);
    let coarse = pitch_after_hold(&[0.25, 0.25, 0.15]);
    let single_frame = pitch_after_hold(&[0.65]);

    assert_eq!(fine, coarse);
    assert_eq!(coarse, single_frame);
    approx_eq(fine.0, 75.0);
    assert_eq!(fine.1, 4);
}

#[test]
fn f8_pitch_snap_activates_in_both_modifier_press_orders() {
    for (first_modifier, second_modifier) in [
        (KeyCode::AltLeft, KeyCode::ControlLeft),
        (KeyCode::ControlLeft, KeyCode::AltLeft),
    ] {
        let mut widget = test_widget();
        widget.set_camera_adjustments(CameraRuntimeAdjustments {
            pitch_deg: 7.0,
            ..CameraRuntimeAdjustments::default()
        });
        let mut input = Input::default();
        input.inject_key(KeyCode::ArrowUp, true);
        input.inject_key(first_modifier, true);
        widget.apply_camera_keyboard_controls(0.0, &input);
        input.end_frame();
        input.inject_key(second_modifier, true);
        widget.apply_camera_keyboard_controls(0.0, &input);

        approx_eq(widget.camera_adjustments.pitch_deg, 15.0);
        assert_eq!(
            widget.vertical_snap_repeat.active_mode,
            VerticalSnapMode::Pitch
        );
        assert_eq!(widget.vertical_snap_repeat.held_duration_sec, 0.0);
        assert_eq!(widget.vertical_snap_repeat.repeat_steps_emitted, 0);
    }
}

#[test]
fn f8_releasing_alt_resumes_continuous_pitch_immediately() {
    let mut widget = test_widget();
    widget.set_camera_adjustments(CameraRuntimeAdjustments {
        pitch_deg: 7.0,
        ..CameraRuntimeAdjustments::default()
    });
    let mut input = Input::default();
    input.inject_key(KeyCode::ControlLeft, true);
    input.inject_key(KeyCode::ArrowUp, true);
    widget.apply_camera_keyboard_controls(0.1, &input);
    approx_eq(widget.camera_adjustments.pitch_deg, 13.0);

    input.end_frame();
    input.inject_key(KeyCode::AltLeft, true);
    widget.apply_camera_keyboard_controls(0.0, &input);
    approx_eq(widget.camera_adjustments.pitch_deg, 15.0);

    input.end_frame();
    input.inject_key(KeyCode::AltLeft, false);
    widget.apply_camera_keyboard_controls(0.1, &input);

    approx_eq(widget.camera_adjustments.pitch_deg, 21.0);
    assert_eq!(vertical_camera_action(&input), VerticalCameraAction::Pitch);
    assert_eq!(
        widget.vertical_snap_repeat,
        VerticalSnapRepeatState::default()
    );
}

#[test]
fn f8_snap_pitch_has_no_pan_or_zoom_leakage() {
    let adjustments = camera_adjustments_after_keys(&[
        KeyCode::AltLeft,
        KeyCode::ControlLeft,
        KeyCode::ShiftLeft,
        KeyCode::ArrowUp,
    ]);
    approx_eq(adjustments.pitch_deg, HORIZONTAL_SNAP_DEG);
    assert_eq!(adjustments.distance_scale_delta, 0.0);
    assert_eq!(adjustments.pan_ndc, Vec2::ZERO);
    assert_eq!(adjustments.yaw_deg, 0.0);
    assert_eq!(adjustments.roll_deg, 0.0);
}

#[test]
fn f8_horizontal_and_vertical_snap_repeats_are_independent() {
    let mut widget = test_widget();
    widget.set_camera_adjustments(CameraRuntimeAdjustments {
        pitch_deg: 7.0,
        roll_deg: 7.0,
        ..CameraRuntimeAdjustments::default()
    });
    let mut input = pitch_snap_test_input(KeyCode::ArrowUp);
    input.inject_key(KeyCode::ArrowRight, true);

    widget.apply_camera_keyboard_controls(0.0, &input);
    approx_eq(widget.camera_adjustments.pitch_deg, 15.0);
    approx_eq(widget.camera_adjustments.roll_deg, 15.0);
    input.end_frame();
    widget.apply_camera_keyboard_controls(VERTICAL_SNAP_REPEAT_DELAY_SEC, &input);
    approx_eq(widget.camera_adjustments.pitch_deg, 30.0);
    approx_eq(widget.camera_adjustments.roll_deg, 30.0);
    assert_eq!(widget.vertical_snap_repeat.repeat_steps_emitted, 1);
    assert_eq!(widget.horizontal_snap_repeat.repeat_steps_emitted, 1);
}

#[test]
fn f8_pitch_snap_at_bounds_does_not_accumulate_or_jump_on_reversal() {
    for (start, outward, inward, expected_after_reverse) in [
        (89.0, KeyCode::ArrowUp, KeyCode::ArrowDown, 75.0),
        (-89.0, KeyCode::ArrowDown, KeyCode::ArrowUp, -75.0),
    ] {
        let mut widget = test_widget();
        widget.set_camera_adjustments(CameraRuntimeAdjustments {
            pitch_deg: start,
            ..CameraRuntimeAdjustments::default()
        });
        let mut input = pitch_snap_test_input(outward);

        widget.apply_camera_keyboard_controls(0.0, &input);
        input.end_frame();
        widget.apply_camera_keyboard_controls(VERTICAL_SNAP_REPEAT_DELAY_SEC * 3.0, &input);
        approx_eq(widget.camera_adjustments.pitch_deg, start);

        input.inject_key(outward, false);
        input.inject_key(inward, true);
        widget.apply_camera_keyboard_controls(0.0, &input);
        approx_eq(widget.camera_adjustments.pitch_deg, expected_after_reverse);
        assert_eq!(
            widget.vertical_snap_repeat.active_direction,
            if inward == KeyCode::ArrowUp { 1 } else { -1 }
        );
        assert_eq!(widget.vertical_snap_repeat.held_duration_sec, 0.0);
        assert_eq!(widget.vertical_snap_repeat.repeat_steps_emitted, 0);
    }
}

#[test]
fn f8_disabling_controls_clears_pitch_snap_repeat() {
    let mut controls = CameraControls::default();
    let mut input = Input::default();
    input.inject_key(KeyCode::F8, true);
    controls.apply_frame(0.0, &input, None, CameraSettings::default(), 1.0);

    input.inject_key(KeyCode::F8, false);
    input.inject_key(KeyCode::AltLeft, true);
    input.inject_key(KeyCode::ControlLeft, true);
    input.inject_key(KeyCode::ArrowUp, true);
    input.end_frame();
    controls.apply_frame(0.0, &input, None, CameraSettings::default(), 1.0);
    assert_eq!(
        controls.vertical_snap_repeat.active_mode,
        VerticalSnapMode::Pitch
    );

    input.end_frame();
    input.inject_key(KeyCode::F8, true);
    controls.apply_frame(0.0, &input, None, CameraSettings::default(), 1.0);
    assert!(!controls.camera_controls_enabled());
    assert_eq!(
        controls.vertical_snap_repeat,
        VerticalSnapRepeatState::default()
    );
}

#[test]
fn f8_reset_clears_pitch_snap_repeat() {
    let mut controls = CameraControls::default();
    let mut input = Input::default();
    input.inject_key(KeyCode::F8, true);
    controls.apply_frame(0.0, &input, None, CameraSettings::default(), 1.0);

    input.inject_key(KeyCode::F8, false);
    input.inject_key(KeyCode::AltLeft, true);
    input.inject_key(KeyCode::ControlLeft, true);
    input.inject_key(KeyCode::ArrowUp, true);
    input.end_frame();
    controls.apply_frame(0.0, &input, None, CameraSettings::default(), 1.0);
    assert_eq!(
        controls.vertical_snap_repeat.active_mode,
        VerticalSnapMode::Pitch
    );

    input.end_frame();
    input.inject_key(KeyCode::KeyR, true);
    controls.apply_frame(0.0, &input, None, CameraSettings::default(), 1.0);
    assert_eq!(
        controls.vertical_snap_repeat,
        VerticalSnapRepeatState::default()
    );
    assert_eq!(
        controls.camera_adjustments,
        CameraRuntimeAdjustments::default()
    );
}

#[test]
fn f8_horizontal_modifier_precedence_uses_alt_for_snaps() {
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
        HorizontalCameraAction::Roll
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

    let ctrl_and_shift_left = camera_adjustments_after_keys(&[
        KeyCode::ShiftLeft,
        KeyCode::ControlLeft,
        KeyCode::ArrowLeft,
    ]);
    approx_eq(
        ctrl_and_shift_left.roll_deg,
        -CAMERA_ROLL_RATE_DEG_PER_SEC / 60.0,
    );
    assert_eq!(ctrl_and_shift_left.pan_ndc.x, 0.0);
    assert_eq!(ctrl_and_shift_left.yaw_deg, 0.0);

    let ctrl_left = camera_adjustments_after_keys(&[KeyCode::ControlLeft, KeyCode::ArrowLeft]);
    assert!(ctrl_left.roll_deg < 0.0);
    assert_eq!(ctrl_left.yaw_deg, 0.0);
    assert_eq!(ctrl_left.pan_ndc.x, 0.0);

    input.inject_key(KeyCode::AltLeft, true);
    assert_eq!(
        horizontal_camera_action(&input),
        HorizontalCameraAction::SnapRoll
    );

    let alt_and_ctrl_left = camera_adjustments_after_keys(&[
        KeyCode::AltLeft,
        KeyCode::ControlLeft,
        KeyCode::ArrowLeft,
    ]);
    assert_eq!(alt_and_ctrl_left.roll_deg, -ROLL_SNAP_DEG);
    assert_eq!(alt_and_ctrl_left.yaw_deg, 0.0);

    let alt_and_shift_left =
        camera_adjustments_after_keys(&[KeyCode::AltLeft, KeyCode::ShiftLeft, KeyCode::ArrowLeft]);
    assert_eq!(alt_and_shift_left.yaw_deg, ROLL_SNAP_DEG);
    assert_eq!(alt_and_shift_left.roll_deg, 0.0);
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
fn f8_yaw_snap_selects_next_grid_point_in_requested_direction() {
    for (current, expected_visual_right, expected_visual_left) in [
        (7.0, 0.0, 15.0),
        (30.0, 15.0, 45.0),
        (-7.0, -15.0, 0.0),
        (-30.0, -45.0, -15.0),
        (0.0, -15.0, 15.0),
    ] {
        approx_eq(snap_yaw_degrees(current, -1), expected_visual_right);
        approx_eq(snap_yaw_degrees(current, 1), expected_visual_left);
    }
}

#[test]
fn f8_yaw_snap_wraps_across_180_degrees() {
    approx_eq(snap_yaw_degrees(179.0, -1), 165.0);
    approx_eq(snap_yaw_degrees(-179.0, 1), -165.0);
    approx_eq(snap_yaw_degrees(-180.0, -1), 165.0);
    approx_eq(snap_yaw_degrees(-180.0, 1), -165.0);
}

#[test]
fn f8_alt_shift_selects_yaw_snap_without_horizontal_leakage() {
    let input = yaw_snap_test_input(KeyCode::ArrowRight);
    assert_eq!(
        horizontal_camera_action(&input),
        HorizontalCameraAction::SnapYaw
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

    let visual_right =
        camera_adjustments_after_keys(&[KeyCode::AltLeft, KeyCode::ShiftLeft, KeyCode::ArrowRight]);
    approx_eq(visual_right.yaw_deg, -YAW_SNAP_DEG);
    assert_eq!(visual_right.roll_deg, 0.0);
    assert_eq!(visual_right.pitch_deg, 0.0);
    assert_eq!(visual_right.distance_scale_delta, 0.0);
    assert_eq!(visual_right.pan_ndc, Vec2::ZERO);

    let visual_left =
        camera_adjustments_after_keys(&[KeyCode::AltLeft, KeyCode::ShiftLeft, KeyCode::ArrowLeft]);
    approx_eq(visual_left.yaw_deg, YAW_SNAP_DEG);
    assert_eq!(visual_left.roll_deg, 0.0);
    assert_eq!(visual_left.pitch_deg, 0.0);
    assert_eq!(visual_left.distance_scale_delta, 0.0);
    assert_eq!(visual_left.pan_ndc, Vec2::ZERO);
}

#[test]
fn f8_alt_does_not_change_vertical_controls() {
    let mut input = Input::default();
    input.inject_key(KeyCode::AltLeft, true);
    input.inject_key(KeyCode::ShiftLeft, true);
    input.inject_key(KeyCode::ArrowUp, true);
    assert_eq!(
        horizontal_camera_action(&input),
        HorizontalCameraAction::SnapYaw
    );
    assert_eq!(vertical_camera_action(&input), VerticalCameraAction::Zoom);

    let adjustments =
        camera_adjustments_after_keys(&[KeyCode::AltLeft, KeyCode::ShiftLeft, KeyCode::ArrowUp]);
    assert_eq!(adjustments.yaw_deg, 0.0);
    assert!(adjustments.distance_scale_delta < 0.0);
    assert_eq!(adjustments.roll_deg, 0.0);
    assert_eq!(adjustments.pitch_deg, 0.0);
    assert_eq!(adjustments.pan_ndc, Vec2::ZERO);
}

#[test]
fn f8_yaw_snap_tap_emits_one_detent() {
    let mut widget = test_widget();
    widget.set_camera_adjustments(CameraRuntimeAdjustments {
        yaw_deg: 7.0,
        ..CameraRuntimeAdjustments::default()
    });
    let mut input = yaw_snap_test_input(KeyCode::ArrowRight);

    widget.apply_camera_keyboard_controls(0.0, &input);
    approx_eq(widget.camera_adjustments.yaw_deg, 0.0);
    input.end_frame();
    input.inject_key(KeyCode::ArrowRight, false);
    widget.apply_camera_keyboard_controls(0.0, &input);
    widget.apply_camera_keyboard_controls(1.0, &input);

    approx_eq(widget.camera_adjustments.yaw_deg, 0.0);
    assert_eq!(
        widget.horizontal_snap_repeat,
        HorizontalSnapRepeatState::default()
    );
}

#[test]
fn f8_yaw_snap_repeats_after_delay_at_the_configured_interval() {
    let mut widget = test_widget();
    widget.set_camera_adjustments(CameraRuntimeAdjustments {
        yaw_deg: 7.0,
        ..CameraRuntimeAdjustments::default()
    });
    let mut input = yaw_snap_test_input(KeyCode::ArrowRight);

    widget.apply_camera_keyboard_controls(0.0, &input);
    input.end_frame();
    widget.apply_camera_keyboard_controls(HORIZONTAL_SNAP_REPEAT_DELAY_SEC, &input);
    approx_eq(widget.camera_adjustments.yaw_deg, -15.0);
    widget.apply_camera_keyboard_controls(HORIZONTAL_SNAP_REPEAT_INTERVAL_SEC - 0.001, &input);
    approx_eq(widget.camera_adjustments.yaw_deg, -15.0);
    widget.apply_camera_keyboard_controls(0.001, &input);
    approx_eq(widget.camera_adjustments.yaw_deg, -30.0);
    widget.apply_camera_keyboard_controls(HORIZONTAL_SNAP_REPEAT_INTERVAL_SEC * 2.0, &input);
    approx_eq(widget.camera_adjustments.yaw_deg, -60.0);
}

#[test]
fn f8_yaw_snap_repeat_is_frame_rate_independent() {
    fn yaw_after_hold(dts: &[f32]) -> (f32, u32) {
        let mut widget = test_widget();
        widget.set_camera_adjustments(CameraRuntimeAdjustments {
            yaw_deg: 7.0,
            ..CameraRuntimeAdjustments::default()
        });
        let mut input = yaw_snap_test_input(KeyCode::ArrowRight);
        widget.apply_camera_keyboard_controls(0.0, &input);
        input.end_frame();
        for &dt in dts {
            widget.apply_camera_keyboard_controls(dt, &input);
        }
        (
            widget.camera_adjustments.yaw_deg,
            widget.horizontal_snap_repeat.repeat_steps_emitted,
        )
    }

    let fine = yaw_after_hold(&[0.05; 15]);
    let coarse = yaw_after_hold(&[0.25, 0.25, 0.25]);
    let single_frame = yaw_after_hold(&[0.75]);

    assert_eq!(fine, coarse);
    assert_eq!(coarse, single_frame);
    approx_eq(fine.0, -75.0);
    assert_eq!(fine.1, 5);
}

#[test]
fn f8_shift_yaw_remains_continuous_at_the_existing_rate() {
    let mut widget = test_widget();
    widget.set_camera_adjustments(CameraRuntimeAdjustments {
        yaw_deg: 7.0,
        ..CameraRuntimeAdjustments::default()
    });
    let mut input = Input::default();
    input.inject_key(KeyCode::ShiftLeft, true);
    input.inject_key(KeyCode::ArrowRight, true);
    assert_eq!(
        horizontal_camera_action(&input),
        HorizontalCameraAction::Yaw
    );

    widget.apply_camera_keyboard_controls(0.1, &input);
    approx_eq(
        widget.camera_adjustments.yaw_deg,
        7.0 - CAMERA_YAW_RATE_DEG_PER_SEC * 0.1,
    );
    input.end_frame();
    widget.apply_camera_keyboard_controls(0.1, &input);
    approx_eq(
        widget.camera_adjustments.yaw_deg,
        7.0 - CAMERA_YAW_RATE_DEG_PER_SEC * 0.2,
    );
}

#[test]
fn f8_alt_shift_yaw_snap_activates_in_both_modifier_orders() {
    for (first_modifier, second_modifier) in [
        (KeyCode::AltLeft, KeyCode::ShiftLeft),
        (KeyCode::ShiftLeft, KeyCode::AltLeft),
    ] {
        let mut widget = test_widget();
        widget.set_camera_adjustments(CameraRuntimeAdjustments {
            yaw_deg: 7.0,
            ..CameraRuntimeAdjustments::default()
        });
        let mut input = Input::default();
        input.inject_key(KeyCode::ArrowRight, true);
        input.inject_key(first_modifier, true);
        widget.apply_camera_keyboard_controls(0.0, &input);
        input.end_frame();
        input.inject_key(second_modifier, true);
        widget.apply_camera_keyboard_controls(0.0, &input);

        approx_eq(widget.camera_adjustments.yaw_deg, 0.0);
        assert_eq!(
            widget.horizontal_snap_repeat.active_mode,
            HorizontalSnapMode::Yaw
        );
        assert_eq!(widget.horizontal_snap_repeat.held_duration_sec, 0.0);
        assert_eq!(widget.horizontal_snap_repeat.repeat_steps_emitted, 0);
    }
}

#[test]
fn f8_releasing_alt_resumes_continuous_yaw_immediately() {
    let mut widget = test_widget();
    widget.set_camera_adjustments(CameraRuntimeAdjustments {
        yaw_deg: 7.0,
        ..CameraRuntimeAdjustments::default()
    });
    let mut input = yaw_snap_test_input(KeyCode::ArrowRight);
    widget.apply_camera_keyboard_controls(0.0, &input);
    approx_eq(widget.camera_adjustments.yaw_deg, 0.0);

    input.end_frame();
    input.inject_key(KeyCode::AltLeft, false);
    widget.apply_camera_keyboard_controls(0.1, &input);

    approx_eq(widget.camera_adjustments.yaw_deg, -9.0);
    assert_eq!(
        horizontal_camera_action(&input),
        HorizontalCameraAction::Yaw
    );
    assert_eq!(
        widget.horizontal_snap_repeat,
        HorizontalSnapRepeatState::default()
    );
}

#[test]
fn f8_alt_ctrl_roll_snap_takes_precedence_over_alt_shift() {
    let mut input = yaw_snap_test_input(KeyCode::ArrowRight);
    input.inject_key(KeyCode::ControlLeft, true);
    assert_eq!(
        horizontal_camera_action(&input),
        HorizontalCameraAction::SnapRoll
    );

    let adjustments = camera_adjustments_after_keys(&[
        KeyCode::AltLeft,
        KeyCode::ShiftLeft,
        KeyCode::ControlLeft,
        KeyCode::ArrowRight,
    ]);
    approx_eq(adjustments.roll_deg, ROLL_SNAP_DEG);
    assert_eq!(adjustments.yaw_deg, 0.0);
    assert_eq!(adjustments.pan_ndc, Vec2::ZERO);
}

#[test]
fn f8_roll_and_yaw_snap_transitions_reset_repeat_and_snap_immediately() {
    let mut widget = test_widget();
    widget.set_camera_adjustments(CameraRuntimeAdjustments {
        yaw_deg: 7.0,
        roll_deg: 7.0,
        ..CameraRuntimeAdjustments::default()
    });
    let mut input = yaw_snap_test_input(KeyCode::ArrowRight);
    input.inject_key(KeyCode::ControlLeft, true);

    widget.apply_camera_keyboard_controls(0.0, &input);
    approx_eq(widget.camera_adjustments.roll_deg, 15.0);
    assert_eq!(
        widget.horizontal_snap_repeat.active_mode,
        HorizontalSnapMode::Roll
    );
    input.end_frame();
    widget.apply_camera_keyboard_controls(HORIZONTAL_SNAP_REPEAT_DELAY_SEC, &input);
    approx_eq(widget.camera_adjustments.roll_deg, 30.0);
    assert_eq!(widget.horizontal_snap_repeat.repeat_steps_emitted, 1);

    input.inject_key(KeyCode::ControlLeft, false);
    widget.apply_camera_keyboard_controls(0.0, &input);
    approx_eq(widget.camera_adjustments.yaw_deg, 0.0);
    approx_eq(widget.camera_adjustments.roll_deg, 30.0);
    assert_eq!(
        widget.horizontal_snap_repeat.active_mode,
        HorizontalSnapMode::Yaw
    );
    assert_eq!(widget.horizontal_snap_repeat.held_duration_sec, 0.0);
    assert_eq!(widget.horizontal_snap_repeat.repeat_steps_emitted, 0);

    input.inject_key(KeyCode::ControlLeft, true);
    widget.apply_camera_keyboard_controls(0.0, &input);
    approx_eq(widget.camera_adjustments.roll_deg, 45.0);
    approx_eq(widget.camera_adjustments.yaw_deg, 0.0);
    assert_eq!(
        widget.horizontal_snap_repeat.active_mode,
        HorizontalSnapMode::Roll
    );
    assert_eq!(widget.horizontal_snap_repeat.held_duration_sec, 0.0);
    assert_eq!(widget.horizontal_snap_repeat.repeat_steps_emitted, 0);
}

#[test]
fn f8_disabling_controls_clears_yaw_snap_repeat() {
    let mut controls = CameraControls::default();
    let mut input = Input::default();
    input.inject_key(KeyCode::F8, true);
    controls.apply_frame(0.0, &input, None, CameraSettings::default(), 1.0);
    assert!(controls.camera_controls_enabled());

    input.inject_key(KeyCode::F8, false);
    input.inject_key(KeyCode::AltLeft, true);
    input.inject_key(KeyCode::ShiftLeft, true);
    input.inject_key(KeyCode::ArrowRight, true);
    input.end_frame();
    controls.apply_frame(0.0, &input, None, CameraSettings::default(), 1.0);
    assert_eq!(
        controls.horizontal_snap_repeat.active_mode,
        HorizontalSnapMode::Yaw
    );

    input.end_frame();
    input.inject_key(KeyCode::F8, true);
    controls.apply_frame(0.0, &input, None, CameraSettings::default(), 1.0);
    assert!(!controls.camera_controls_enabled());
    assert_eq!(
        controls.horizontal_snap_repeat,
        HorizontalSnapRepeatState::default()
    );
}

#[test]
fn f8_reset_clears_yaw_snap_repeat() {
    let mut controls = CameraControls::default();
    let mut input = Input::default();
    input.inject_key(KeyCode::F8, true);
    controls.apply_frame(0.0, &input, None, CameraSettings::default(), 1.0);

    input.inject_key(KeyCode::F8, false);
    input.inject_key(KeyCode::AltLeft, true);
    input.inject_key(KeyCode::ShiftLeft, true);
    input.inject_key(KeyCode::ArrowRight, true);
    input.end_frame();
    controls.apply_frame(0.0, &input, None, CameraSettings::default(), 1.0);
    assert_eq!(
        controls.horizontal_snap_repeat.active_mode,
        HorizontalSnapMode::Yaw
    );

    input.end_frame();
    input.inject_key(KeyCode::KeyR, true);
    controls.apply_frame(0.0, &input, None, CameraSettings::default(), 1.0);
    assert_eq!(
        controls.horizontal_snap_repeat,
        HorizontalSnapRepeatState::default()
    );
    assert_eq!(
        controls.camera_adjustments,
        CameraRuntimeAdjustments::default()
    );
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
fn f8_ctrl_right_then_alt_immediately_activates_one_snap() {
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
    input.inject_key(KeyCode::AltLeft, true);
    widget.apply_camera_keyboard_controls(ROLL_SNAP_REPEAT_DELAY_SEC * 2.0, &input);
    approx_eq(widget.camera_adjustments.roll_deg, 30.0);
    assert_eq!(
        widget.horizontal_snap_repeat,
        HorizontalSnapRepeatState {
            active_mode: HorizontalSnapMode::Roll,
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
fn f8_ctrl_left_then_alt_immediately_activates_one_snap() {
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
    input.inject_key(KeyCode::AltLeft, true);
    widget.apply_camera_keyboard_controls(ROLL_SNAP_REPEAT_DELAY_SEC * 2.0, &input);
    approx_eq(widget.camera_adjustments.roll_deg, -15.0);
    assert_eq!(widget.horizontal_snap_repeat.active_direction, -1);
    assert_eq!(widget.horizontal_snap_repeat.repeat_steps_emitted, 0);
}

#[test]
fn f8_ctrl_continues_to_win_over_shift_without_alt() {
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
    approx_eq(widget.camera_adjustments.yaw_deg, -9.0);

    input.end_frame();
    input.inject_key(KeyCode::ControlLeft, true);
    assert_eq!(
        horizontal_camera_action(&input),
        HorizontalCameraAction::Roll
    );
    widget.apply_camera_keyboard_controls(0.1, &input);
    approx_eq(widget.camera_adjustments.roll_deg, 16.0);
    approx_eq(widget.camera_adjustments.yaw_deg, -9.0);
    assert_eq!(
        widget.horizontal_snap_repeat,
        HorizontalSnapRepeatState::default()
    );
}

#[test]
fn f8_ctrl_alt_then_right_immediately_activates_one_snap() {
    let mut widget = test_widget();
    widget.set_camera_adjustments(CameraRuntimeAdjustments {
        roll_deg: 7.0,
        ..CameraRuntimeAdjustments::default()
    });
    let mut input = Input::default();
    input.inject_key(KeyCode::ControlLeft, true);
    input.inject_key(KeyCode::AltLeft, true);
    widget.apply_camera_keyboard_controls(0.1, &input);
    assert_eq!(
        widget.horizontal_snap_repeat,
        HorizontalSnapRepeatState::default()
    );

    input.end_frame();
    input.inject_key(KeyCode::ArrowRight, true);
    widget.apply_camera_keyboard_controls(ROLL_SNAP_REPEAT_DELAY_SEC * 2.0, &input);
    approx_eq(widget.camera_adjustments.roll_deg, 15.0);
    assert_eq!(widget.horizontal_snap_repeat.active_direction, 1);
    assert_eq!(widget.horizontal_snap_repeat.repeat_steps_emitted, 0);
}

#[test]
fn f8_releasing_alt_resumes_continuous_ctrl_roll_immediately() {
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
    input.inject_key(KeyCode::AltLeft, true);
    widget.apply_camera_keyboard_controls(0.0, &input);
    approx_eq(widget.camera_adjustments.roll_deg, 30.0);

    input.end_frame();
    input.inject_key(KeyCode::AltLeft, false);
    widget.apply_camera_keyboard_controls(0.1, &input);
    approx_eq(widget.camera_adjustments.roll_deg, 39.0);
    assert_eq!(
        widget.horizontal_snap_repeat,
        HorizontalSnapRepeatState::default()
    );
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
    assert_eq!(
        widget.horizontal_snap_repeat,
        HorizontalSnapRepeatState::default()
    );
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
    assert_eq!(
        widget.horizontal_snap_repeat,
        HorizontalSnapRepeatState::default()
    );

    input.end_frame();
    input.inject_key(KeyCode::ArrowRight, true);
    widget.apply_camera_keyboard_controls(0.0, &input);
    approx_eq(widget.camera_adjustments.roll_deg, 45.0);
}

#[test]
fn f8_roll_snap_releasing_any_chord_member_cancels_repeat() {
    for released_key in [KeyCode::ArrowRight, KeyCode::ControlLeft, KeyCode::AltLeft] {
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
        assert_eq!(
            widget.horizontal_snap_repeat,
            HorizontalSnapRepeatState::default()
        );
        match released_key {
            KeyCode::ArrowRight => {
                approx_eq(widget.camera_adjustments.roll_deg, 30.0);
                approx_eq(widget.camera_adjustments.yaw_deg, 0.0);
            }
            KeyCode::ControlLeft => {
                approx_eq(widget.camera_adjustments.roll_deg, 30.0);
                approx_eq(widget.camera_adjustments.yaw_deg, 0.0);
            }
            KeyCode::AltLeft => {
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
            widget.horizontal_snap_repeat.repeat_steps_emitted,
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
    input.inject_key(KeyCode::ArrowLeft, true);
    widget.apply_camera_keyboard_controls(0.25, &input);
    assert!(widget.camera_adjustments.yaw_deg < -150.0);
    assert!(widget.camera_adjustments.yaw_deg > -180.0);

    let visual_left =
        camera_adjustments_after_repeated_keys(&[KeyCode::ShiftLeft, KeyCode::ArrowLeft], 17, 0.25);
    let visual_right = camera_adjustments_after_repeated_keys(
        &[KeyCode::ShiftLeft, KeyCode::ArrowRight],
        17,
        0.25,
    );
    approx_eq(visual_left.yaw_deg, 22.5);
    approx_eq(visual_right.yaw_deg, -22.5);
    assert!(visual_left.yaw_deg.abs() < 180.0);
    assert!(visual_right.yaw_deg.abs() < 180.0);

    let visual_left_two_revolutions =
        camera_adjustments_after_repeated_keys(&[KeyCode::ShiftLeft, KeyCode::ArrowLeft], 32, 0.25);
    let visual_right_two_revolutions = camera_adjustments_after_repeated_keys(
        &[KeyCode::ShiftLeft, KeyCode::ArrowRight],
        32,
        0.25,
    );
    approx_eq(visual_left_two_revolutions.yaw_deg, 0.0);
    approx_eq(visual_right_two_revolutions.yaw_deg, 0.0);
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
