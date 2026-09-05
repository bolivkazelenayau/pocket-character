use super::*;
use crate::menu_guest::MenuAction;
use crate::settings::{AntiAliasingPreference, AppSettings, RenderSettings};
use glam::Vec2;
use tempfile::tempdir;

fn test_widget() -> Widget {
    Widget::new(test_config())
}

fn test_config() -> WidgetConfig {
    WidgetConfig {
        model_path: PathBuf::new(),
        vrma_path: PathBuf::new(),
        bundle_path: PathBuf::new(),
        menu_bundle_path: PathBuf::new(),
        menu_pak_path: PathBuf::new(),
        size: (450, 600),
        frames: None,
    }
}

fn approx_eq(actual: f32, expected: f32) {
    assert!((actual - expected).abs() < 1.0e-5, "{actual} != {expected}");
}

#[test]
fn invalid_live_settings_are_sanitized() {
    let settings = CameraSettings {
        fov_deg: f32::NAN,
        distance_scale: -1.0,
        headroom: 1.0,
        ..CameraSettings::default()
    };
    assert_eq!(
        settings.sanitized(),
        CameraSettings {
            fov_deg: 40.0,
            distance_scale: 0.1,
            headroom: 0.49,
            ..CameraSettings::default()
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
    // Canonical base lives in `settings.camera`; menu actions work before the
    // model loads (no pan to revalidate, no camera to reapply yet).
    widget.apply_control_action(super::controls::ControlAction::SetBaseFov(35.0));
    widget.apply_control_action(super::controls::ControlAction::SetBaseDistance(0.75));

    assert_eq!(widget.settings.camera.fov_deg, 35.0);
    assert_eq!(widget.settings.camera.distance_scale, 0.75);
    assert_eq!(
        widget.settings.camera.headroom,
        CameraSettings::default().headroom
    );
    let snapshot = widget.controls_snapshot();
    assert_eq!(snapshot.base_fov_deg(), 35.0);
    assert_eq!(snapshot.base_distance_scale(), 0.75);
}

#[test]
fn discrete_menu_actions_map_to_current_base_values() {
    let mut widget = test_widget();
    widget.apply_control_action(ControlAction::SetBaseFov(55.0));
    widget.apply_control_action(ControlAction::SetBaseDistance(0.8));

    assert_eq!(
        widget.menu_control_action(MenuAction::FovDecrement),
        ControlAction::SetBaseFov(super::camera::controls::base_fov_after_step(55.0, -1))
    );
    assert_eq!(
        widget.menu_control_action(MenuAction::FovIncrement),
        ControlAction::SetBaseFov(super::camera::controls::base_fov_after_step(55.0, 1))
    );
    assert_eq!(
        widget.menu_control_action(MenuAction::DistanceDecrement),
        ControlAction::SetBaseDistance(super::camera::controls::base_distance_after_step(0.8, -1))
    );
    assert_eq!(
        widget.menu_control_action(MenuAction::DistanceIncrement),
        ControlAction::SetBaseDistance(super::camera::controls::base_distance_after_step(0.8, 1))
    );
    assert_eq!(
        widget.menu_control_action(MenuAction::ResetRuntimeCamera),
        ControlAction::ResetRuntimeCamera
    );
}

#[test]
fn one_discrete_menu_change_persists_once() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("settings.json");
    let mut widget =
        Widget::new_with_settings_path(test_config(), AppSettings::default(), Some(path.clone()));
    let expected = super::camera::controls::base_fov_after_step(widget.settings.camera.fov_deg, 1);

    widget.apply_menu_action(MenuAction::FovIncrement);

    assert_eq!(widget.settings.camera.fov_deg, expected);
    assert_eq!(widget.save_count, 1);
    assert_eq!(AppSettings::load_from_path(&path).camera.fov_deg, expected);
}

#[test]
fn reset_menu_action_preserves_base_settings_and_does_not_persist() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("settings.json");
    let settings = AppSettings {
        camera: CameraSettings {
            fov_deg: 55.0,
            distance_scale: 0.8,
            ..CameraSettings::default()
        },
        ..AppSettings::default()
    };
    let mut widget = Widget::new_with_settings_path(test_config(), settings, Some(path));
    widget.set_camera_adjustments(CameraRuntimeAdjustments {
        fov_delta_deg: 6.0,
        distance_scale_delta: -0.1,
        yaw_deg: 15.0,
        ..CameraRuntimeAdjustments::default()
    });

    widget.apply_menu_action(MenuAction::ResetRuntimeCamera);

    assert_eq!(widget.settings.camera.fov_deg, 55.0);
    assert_eq!(widget.settings.camera.distance_scale, 0.8);
    assert_eq!(
        widget.camera_controls.adjustments(),
        CameraRuntimeAdjustments::default()
    );
    assert_eq!(widget.save_count, 0);
}

#[test]
fn menu_owned_pointer_blocks_native_drag_and_character_click() {
    assert!(!native_drag_allowed_for_menu_pointer(true));
    assert!(native_drag_allowed_for_menu_pointer(false));
}

#[test]
fn pointer_press_is_buffered_at_press_position_across_zero_tick_frames() {
    let mut widget = test_widget();
    let press_cursor = Vec2::new(24.0, 520.0);
    let release_cursor = Vec2::new(80.0, 80.0);
    let mut input = Input::default();
    input.inject_cursor(press_cursor.x, press_cursor.y);
    input.inject_mouse_button(pocket3d::winit::event::MouseButton::Left, true);
    // The desktop host calls drag_at at the native press edge, before a later
    // cursor move/release can change Input::cursor().
    assert!(widget.drag_at(press_cursor));
    input.inject_cursor(release_cursor.x, release_cursor.y);
    input.inject_mouse_button(pocket3d::winit::event::MouseButton::Left, false);

    widget.frame(0.0, &input);
    input.end_frame();
    widget.frame(0.0, &input);

    assert_eq!(widget.pending_menu_pointer.len(), 1);
    let frame = widget.pending_menu_pointer[0];
    assert_eq!(frame.press_cursor, Some(press_cursor));
    assert_eq!(frame.cursor, Some(release_cursor));
    assert!(frame.pressed_edge);
    assert!(!frame.button_down);
    assert_eq!(widget.pending_character_clicks, 1);
}

#[test]
fn focus_loss_buffers_cancellation_and_clears_pending_character_click() {
    let mut widget = test_widget();
    let cursor = Vec2::new(24.0, 520.0);
    let mut input = Input::default();
    input.inject_cursor(cursor.x, cursor.y);
    input.inject_mouse_button(pocket3d::winit::event::MouseButton::Left, true);
    assert!(widget.drag_at(cursor));
    widget.frame(0.0, &input);

    input.end_frame();
    input.clear();
    widget.frame(0.0, &input);

    assert_eq!(widget.pending_character_clicks, 0);
    assert!(!widget.menu_pointer_owned);
    assert_eq!(widget.pending_menu_pointer.len(), 2);
    assert!(widget.pending_menu_pointer[1].cancelled);
}

#[test]
fn multiple_outside_presses_preserve_character_click_count_across_zero_tick_frames() {
    let mut widget = test_widget();
    let cursor = Vec2::new(24.0, 520.0);
    let mut input = Input::default();

    for _ in 0..2 {
        input.inject_cursor(cursor.x, cursor.y);
        input.inject_mouse_button(pocket3d::winit::event::MouseButton::Left, true);
        assert!(widget.drag_at(cursor));
        widget.frame(0.0, &input);
        input.end_frame();

        input.inject_mouse_button(pocket3d::winit::event::MouseButton::Left, false);
        widget.frame(0.0, &input);
        input.end_frame();
    }

    assert_eq!(widget.pending_character_clicks, 2);
}

#[test]
fn unhealthy_menu_discards_pointer_buffer_and_stops_recording_frames() {
    let mut widget = test_widget();
    widget.pending_menu_pointer.push(MenuPointerFrame {
        cursor: Some(Vec2::new(10.0, 10.0)),
        press_cursor: Some(Vec2::new(10.0, 10.0)),
        pressed_edge: true,
        button_down: true,
        cancelled: false,
    });
    widget.pending_menu_press = Some(MenuPress {
        cursor: Vec2::new(10.0, 10.0),
        owned: true,
    });
    widget.latch_menu_failure("test", anyhow::anyhow!("terminal"));

    assert!(widget.pending_menu_pointer.is_empty());
    assert!(widget.pending_menu_press.is_none());

    let mut input = Input::default();
    input.inject_cursor(80.0, 80.0);
    widget.frame(0.0, &input);
    assert!(widget.pending_menu_pointer.is_empty());
}

#[test]
fn live_snap_steps_update_controls_without_resetting_runtime_adjustments() {
    let mut widget = test_widget();
    let existing_adjustments = CameraRuntimeAdjustments {
        fov_delta_deg: 4.0,
        distance_scale_delta: -0.1,
        pan_ndc: Vec2::new(0.03, 0.02),
        yaw_deg: 4.0,
        roll_deg: 6.0,
        pitch_deg: -8.0,
    };
    widget.set_camera_adjustments(existing_adjustments);
    widget.apply_control_action(super::controls::ControlAction::SetAllSnaps {
        yaw_deg: 5.0,
        pitch_deg: 30.0,
        roll_deg: 17.5,
    });

    assert_eq!(widget.camera_controls.adjustments(), existing_adjustments);

    let mut input = Input::default();
    input.inject_key(KeyCode::F8, true);
    widget.frame(0.0, &input);
    input.inject_key(KeyCode::F8, false);
    input.inject_key(KeyCode::AltLeft, true);
    input.inject_key(KeyCode::ShiftLeft, true);
    input.inject_key(KeyCode::ArrowLeft, true);
    input.end_frame();
    widget.frame(0.0, &input);

    let after_first_snap = CameraRuntimeAdjustments {
        yaw_deg: 5.0,
        ..existing_adjustments
    };
    assert_eq!(widget.camera_controls.adjustments(), after_first_snap);
    assert_eq!(widget.settings.camera.yaw_snap_deg, 5.0);
    assert_eq!(widget.settings.camera.roll_snap_deg, 17.5);
    assert_eq!(widget.settings.camera.pitch_snap_deg, 30.0);

    input.end_frame();
    input.inject_key(KeyCode::ArrowLeft, false);
    widget.frame(0.0, &input);
    widget.apply_control_action(super::controls::ControlAction::SetYawSnap(10.0));
    assert_eq!(widget.camera_controls.adjustments(), after_first_snap);

    input.end_frame();
    input.inject_key(KeyCode::ArrowLeft, true);
    widget.frame(0.0, &input);

    let after_second_snap = CameraRuntimeAdjustments {
        yaw_deg: 10.0,
        ..after_first_snap
    };
    assert_eq!(widget.camera_controls.adjustments(), after_second_snap);
    assert_eq!(widget.settings.camera.yaw_snap_deg, 10.0);
    assert_eq!(widget.settings.camera.roll_snap_deg, 17.5);
    assert_eq!(widget.settings.camera.pitch_snap_deg, 30.0);
}

#[test]
fn f8_toggles_live_camera_controls_without_touching_base_settings() {
    let mut widget = test_widget();
    let mut input = Input::default();

    input.inject_key(KeyCode::F8, true);
    widget.frame(0.0, &input);
    assert!(widget.camera_controls.camera_controls_enabled());
    assert_eq!(widget.settings.camera, CameraSettings::default());

    input.inject_key(KeyCode::F8, false);
    input.end_frame();
    input.inject_key(KeyCode::KeyE, true);
    widget.frame(1.0 / 60.0, &input);
    assert!(widget.effective_camera_values().settings.fov_deg > 40.0);
    assert_eq!(widget.settings.camera, CameraSettings::default());
}

#[test]
fn f8_disabling_camera_controls_clears_roll_snap_repeat() {
    let mut widget = test_widget();
    let mut input = Input::default();
    input.inject_key(KeyCode::F8, true);
    widget.frame(0.0, &input);
    input.inject_key(KeyCode::F8, false);
    input.inject_key(KeyCode::ControlLeft, true);
    input.inject_key(KeyCode::AltLeft, true);
    input.inject_key(KeyCode::ArrowRight, true);
    input.end_frame();
    widget.frame(0.0, &input);
    assert!(widget.camera_controls.roll_snap_repeat_is_active());
    input.end_frame();
    input.inject_key(KeyCode::F8, true);
    widget.frame(0.0, &input);

    assert!(!widget.camera_controls.camera_controls_enabled());
    assert!(!widget.camera_controls.roll_snap_repeat_is_active());
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
    let mut input = Input::default();
    input.inject_key(KeyCode::F8, true);
    widget.frame(0.0, &input);
    input.inject_key(KeyCode::F8, false);
    input.inject_key(KeyCode::ControlLeft, true);
    input.inject_key(KeyCode::AltLeft, true);
    input.inject_key(KeyCode::ArrowRight, true);
    input.end_frame();
    widget.frame(0.0, &input);
    assert!(widget.camera_controls.roll_snap_repeat_is_active());
    input.end_frame();
    input.inject_key(KeyCode::KeyR, true);
    widget.frame(1.0 / 60.0, &input);

    assert_eq!(
        widget.camera_controls.adjustments(),
        CameraRuntimeAdjustments::default()
    );
    assert!(!widget.camera_controls.roll_snap_repeat_is_active());
}

#[test]
fn f8_camera_controls_are_gated_and_repeat_while_enabled() {
    let mut widget = test_widget();
    let mut input = Input::default();
    input.inject_key(KeyCode::ShiftLeft, true);
    input.inject_key(KeyCode::ArrowUp, true);

    widget.frame(1.0 / 60.0, &input);
    assert_eq!(
        widget.camera_controls.adjustments(),
        CameraRuntimeAdjustments::default()
    );

    input.inject_key(KeyCode::F8, true);
    widget.frame(0.0, &input);
    input.inject_key(KeyCode::F8, false);
    input.end_frame();

    widget.frame(1.0 / 60.0, &input);
    let first = widget.camera_controls.adjustments().distance_scale_delta;
    input.end_frame();
    widget.frame(1.0 / 60.0, &input);
    let second = widget.camera_controls.adjustments().distance_scale_delta;

    assert!(first < 0.0);
    approx_eq(second, first * 2.0);

    input.inject_key(KeyCode::F8, true);
    widget.frame(0.0, &input);
    assert!(!widget.camera_controls.camera_controls_enabled());
    input.inject_key(KeyCode::F8, false);
    input.end_frame();
    widget.frame(1.0 / 60.0, &input);
    assert_eq!(
        widget.camera_controls.adjustments().distance_scale_delta,
        second
    );
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

    assert_eq!(widget.camera_controls.adjustments(), adjustments);
    assert_eq!(
        widget.effective_camera_values(),
        adjustments.effective(widget.settings.camera)
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
fn desktop_menu_coordinates_use_logical_pixels_at_common_scale_factors() {
    assert_eq!(logical_viewport_for((450, 600), 1.0), (450.0, 600.0));
    assert_eq!(logical_viewport_for((675, 900), 1.5), (450.0, 600.0));
    assert_eq!(logical_viewport_for((900, 1200), 2.0), (450.0, 600.0));
}

#[test]
fn menu_failure_policy_latches_the_first_terminal_error() {
    let mut health = MenuHealth::default();

    assert!(health.is_healthy());
    assert!(health.latch("frame", "guest threw"));
    assert_eq!(health.failure(), Some("frame: guest threw"));
    assert!(!health.is_healthy());

    assert!(!health.latch("overlay", "second error"));
    assert_eq!(health.failure(), Some("frame: guest threw"));
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
    assert_eq!(widget.aa.status().requested_msaa, 2);
    assert_eq!(widget.aa.pending_requests().msaa, Some(2));

    input.end_frame();
    assert!(input.key_down(KeyCode::F4));
    assert!(!input.key_pressed(KeyCode::F4));
    widget.frame(0.0, &input);
    assert_eq!(widget.aa.status().requested_msaa, 2);
    assert_eq!(widget.aa.pending_requests().msaa, Some(2));

    input.inject_key(KeyCode::F4, false);
    input.end_frame();
    input.inject_key(KeyCode::F4, true);
    widget.frame(0.0, &input);
    assert_eq!(widget.aa.status().requested_msaa, 4);
    assert_eq!(widget.aa.pending_requests().msaa, Some(4));
}

#[test]
fn f5_queues_one_smaa_change_per_key_press() {
    let mut widget = test_widget();
    let mut input = Input::default();

    assert!(!widget.aa.requested_smaa());
    assert_eq!(widget.aa.pending_requests().smaa, None);

    input.inject_key(KeyCode::F5, true);
    widget.frame(0.0, &input);
    assert!(widget.aa.requested_smaa());
    assert_eq!(widget.aa.pending_requests().smaa, Some(true));

    input.end_frame();
    assert!(input.key_down(KeyCode::F5));
    assert!(!input.key_pressed(KeyCode::F5));
    widget.frame(0.0, &input);
    assert!(widget.aa.requested_smaa());
    assert_eq!(widget.aa.pending_requests().smaa, Some(true));

    input.inject_key(KeyCode::F5, false);
    input.end_frame();
    input.inject_key(KeyCode::F5, true);
    widget.frame(0.0, &input);
    assert!(!widget.aa.requested_smaa());
    assert_eq!(widget.aa.pending_requests().smaa, Some(false));
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

    let aa = widget.aa.status();
    assert_eq!(aa.requested_msaa, 8);
    assert!(widget.aa.requested_smaa());
    assert_eq!(widget.aa.pending_requests().msaa, None);
    assert_eq!(widget.aa.pending_requests().smaa, None);
}

#[test]
fn headless_widget_does_not_use_desktop_settings_path() {
    let widget = test_widget();

    assert!(widget.settings_path.is_none());
    assert_eq!(widget.aa.status().requested_msaa, 1);
    assert!(!widget.aa.requested_smaa());
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
    let mut widget = Widget::new_with_settings_path(test_config(), settings, Some(path.clone()));
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
    let mut widget = Widget::new_with_settings_path(test_config(), settings, Some(path.clone()));
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
    widget.aa.request_smaa_toggle();

    let requested_msaa = renderer.requested_sample_count();
    let effective_msaa = renderer.effective_sample_count();
    widget.prepare_render(&gpu, &mut renderer);

    assert!(renderer.smaa_enabled());
    assert_eq!(renderer.requested_sample_count(), requested_msaa);
    assert_eq!(renderer.effective_sample_count(), effective_msaa);
    assert!(widget.aa.status().smaa_enabled);
    assert_eq!(widget.aa.pending_requests().smaa, None);

    widget.aa.request_smaa_toggle();
    widget.prepare_render(&gpu, &mut renderer);

    assert!(!renderer.smaa_enabled());
    assert_eq!(renderer.requested_sample_count(), requested_msaa);
    assert_eq!(renderer.effective_sample_count(), effective_msaa);
    assert!(!widget.aa.status().smaa_enabled);
}

#[test]
fn widget_frame_records_render_fps() {
    let mut widget = test_widget();
    let input = Input::default();

    for _ in 0..4 {
        widget.frame(0.25, &input);
    }

    approx_eq(widget.render_fps.fps(), 4.0);
}

use super::controls::{ControlAction, ControlsSnapshot};

fn snapshot_base_equals_persisted(widget: &Widget) {
    let snapshot = widget.controls_snapshot();
    assert_eq!(snapshot.base_fov_deg(), widget.settings.camera.fov_deg);
    assert_eq!(
        snapshot.base_distance_scale(),
        widget.settings.camera.distance_scale
    );
    assert_eq!(snapshot.yaw_snap_deg(), widget.settings.camera.yaw_snap_deg);
    assert_eq!(
        snapshot.pitch_snap_deg(),
        widget.settings.camera.pitch_snap_deg
    );
    assert_eq!(
        snapshot.roll_snap_deg(),
        widget.settings.camera.roll_snap_deg
    );
}

#[test]
fn canonical_camera_settings_cannot_diverge_from_persisted_settings() {
    let mut widget = test_widget();
    // Single source of truth: snapshot base always mirrors `settings.camera`.
    snapshot_base_equals_persisted(&widget);

    widget.apply_control_action(ControlAction::SetBaseFov(55.0));
    snapshot_base_equals_persisted(&widget);
    assert_eq!(widget.settings.camera.fov_deg, 55.0);

    widget.apply_control_action(ControlAction::SetBaseDistance(1.25));
    snapshot_base_equals_persisted(&widget);
    assert_eq!(widget.settings.camera.distance_scale, 1.25);

    widget.apply_control_action(ControlAction::SetYawSnap(7.0));
    snapshot_base_equals_persisted(&widget);
    assert_eq!(widget.settings.camera.yaw_snap_deg, 7.0);

    widget.apply_control_action(ControlAction::SetPitchSnap(9.0));
    snapshot_base_equals_persisted(&widget);
    assert_eq!(widget.settings.camera.pitch_snap_deg, 9.0);

    widget.apply_control_action(ControlAction::SetRollSnap(11.0));
    snapshot_base_equals_persisted(&widget);
    assert_eq!(widget.settings.camera.roll_snap_deg, 11.0);

    widget.apply_control_action(ControlAction::SetAllSnaps {
        yaw_deg: 5.0,
        pitch_deg: 10.0,
        roll_deg: 20.0,
    });
    snapshot_base_equals_persisted(&widget);
    assert_eq!(widget.settings.camera.yaw_snap_deg, 5.0);
    assert_eq!(widget.settings.camera.pitch_snap_deg, 10.0);
    assert_eq!(widget.settings.camera.roll_snap_deg, 20.0);

    // Session-only pose never touches persisted base.
    widget.apply_control_action(ControlAction::SetYaw(33.0));
    snapshot_base_equals_persisted(&widget);
    widget.apply_control_action(ControlAction::ResetRuntimeCamera);
    snapshot_base_equals_persisted(&widget);
}

#[test]
fn setting_base_fov_rebases_runtime_fov_delta() {
    let mut widget = test_widget();
    widget.set_camera_adjustments(CameraRuntimeAdjustments {
        fov_delta_deg: 10.0,
        ..CameraRuntimeAdjustments::default()
    });
    assert_eq!(
        widget.effective_camera_values().settings.fov_deg,
        50.0,
        "precondition: 40 base + 10 delta"
    );

    let snapshot = widget.apply_control_action(ControlAction::SetBaseFov(55.0));
    assert_eq!(widget.settings.camera.fov_deg, 55.0);
    assert_eq!(widget.camera_controls.adjustments().fov_delta_deg, 0.0);
    assert_eq!(snapshot.base_fov_deg(), 55.0);
    assert_eq!(snapshot.effective_fov_deg(), 55.0);
    assert_eq!(
        widget.effective_camera_values().settings.fov_deg,
        55.0,
        "no hidden keyboard delta reappears"
    );
}

#[test]
fn setting_base_distance_rebases_runtime_distance_delta() {
    let mut widget = test_widget();
    widget.set_camera_adjustments(CameraRuntimeAdjustments {
        distance_scale_delta: 0.25,
        ..CameraRuntimeAdjustments::default()
    });
    assert_eq!(
        widget.effective_camera_values().settings.distance_scale,
        0.85,
        "precondition: 0.6 base + 0.25 delta"
    );

    let snapshot = widget.apply_control_action(ControlAction::SetBaseDistance(1.0));
    assert_eq!(widget.settings.camera.distance_scale, 1.0);
    assert_eq!(
        widget.camera_controls.adjustments().distance_scale_delta,
        0.0
    );
    assert_eq!(snapshot.base_distance_scale(), 1.0);
    assert_eq!(snapshot.effective_distance_scale(), 1.0);
    assert_eq!(
        widget.effective_camera_values().settings.distance_scale,
        1.0,
        "no hidden keyboard delta reappears"
    );
}

#[test]
fn control_actions_preserve_runtime_pan_before_model_load() {
    let mut widget = test_widget();
    // No model is loaded, so Widget-level `validate_pan(CameraPanContext)` is
    // a no-op here. This proves only that control actions preserve existing
    // runtime pan before model load. Projected-bound clamping is covered by
    // the existing camera-kernel tests.
    widget.set_camera_adjustments(CameraRuntimeAdjustments {
        pan_ndc: Vec2::new(0.03, 0.02),
        ..CameraRuntimeAdjustments::default()
    });

    widget.apply_control_action(ControlAction::SetYaw(10.0));
    assert_eq!(
        widget.camera_controls.adjustments().pan_ndc,
        Vec2::new(0.03, 0.02)
    );

    widget.apply_control_action(ControlAction::SetBaseFov(45.0));
    // Base FOV clears only its own delta; existing pan is preserved.
    assert_eq!(widget.settings.camera.fov_deg, 45.0);
    assert_eq!(
        widget.camera_controls.adjustments().pan_ndc,
        Vec2::new(0.03, 0.02)
    );

    widget.apply_control_action(ControlAction::SetBaseDistance(0.8));
    assert_eq!(widget.settings.camera.distance_scale, 0.8);
    assert_eq!(
        widget.camera_controls.adjustments().pan_ndc,
        Vec2::new(0.03, 0.02)
    );
}

#[test]
fn yaw_pitch_roll_actions_return_authoritative_accepted_values() {
    let mut widget = test_widget();

    let snapshot = widget.apply_control_action(ControlAction::SetYaw(45.0));
    assert_eq!(snapshot.yaw_deg(), 45.0);
    assert_eq!(widget.camera_controls.adjustments().yaw_deg, 45.0);

    // Yaw/roll wrap through the kernel normalizer; pitch clamps.
    let snapshot = widget.apply_control_action(ControlAction::SetYaw(190.0));
    assert_eq!(snapshot.yaw_deg(), -170.0);
    let snapshot = widget.apply_control_action(ControlAction::SetRoll(190.0));
    assert_eq!(snapshot.roll_deg(), -170.0);
    let snapshot = widget.apply_control_action(ControlAction::SetPitch(100.0));
    assert_eq!(snapshot.pitch_deg(), 89.0);
    let snapshot = widget.apply_control_action(ControlAction::SetPitch(-100.0));
    assert_eq!(snapshot.pitch_deg(), -89.0);

    // Nonfinite session values sanitize to the kernel fallback (0).
    let snapshot = widget.apply_control_action(ControlAction::SetYaw(f32::NAN));
    assert_eq!(snapshot.yaw_deg(), 0.0);
    let snapshot = widget.apply_control_action(ControlAction::SetPitch(f32::INFINITY));
    assert_eq!(snapshot.pitch_deg(), 0.0);
    let snapshot = widget.apply_control_action(ControlAction::SetRoll(f32::NEG_INFINITY));
    assert_eq!(snapshot.roll_deg(), 0.0);

    // Persisted base is untouched by session-only pose.
    assert_eq!(widget.settings.camera, CameraSettings::default());
}

#[test]
fn reset_restores_saved_framing_without_resetting_snaps_or_aa() {
    let settings = AppSettings {
        camera: CameraSettings {
            fov_deg: 35.0,
            distance_scale: 0.75,
            yaw_snap_deg: 5.0,
            roll_snap_deg: 17.5,
            pitch_snap_deg: 30.0,
            ..CameraSettings::default()
        },
        rendering: RenderSettings {
            msaa: AntiAliasingPreference::X8,
            smaa_enabled: true,
            ..RenderSettings::default()
        },
        ..AppSettings::default()
    };
    let mut widget = Widget::new_with_settings_path(test_config(), settings, None);
    widget.set_camera_adjustments(CameraRuntimeAdjustments {
        fov_delta_deg: 8.0,
        distance_scale_delta: 0.4,
        pan_ndc: Vec2::new(0.05, -0.04),
        yaw_deg: 33.0,
        roll_deg: -21.0,
        pitch_deg: 12.0,
    });

    let snapshot = widget.apply_control_action(ControlAction::ResetRuntimeCamera);
    assert_eq!(
        widget.camera_controls.adjustments(),
        CameraRuntimeAdjustments::default()
    );
    assert_eq!(snapshot.effective_fov_deg(), 35.0);
    assert_eq!(snapshot.effective_distance_scale(), 0.75);
    assert_eq!(snapshot.yaw_deg(), 0.0);
    assert_eq!(snapshot.pitch_deg(), 0.0);
    assert_eq!(snapshot.roll_deg(), 0.0);

    // Persisted snaps and rendering preferences survive the reset.
    assert_eq!(widget.settings.camera.yaw_snap_deg, 5.0);
    assert_eq!(widget.settings.camera.roll_snap_deg, 17.5);
    assert_eq!(widget.settings.camera.pitch_snap_deg, 30.0);
    assert_eq!(snapshot.yaw_snap_deg(), 5.0);
    assert_eq!(snapshot.roll_snap_deg(), 17.5);
    assert_eq!(snapshot.pitch_snap_deg(), 30.0);
    assert_eq!(widget.settings.rendering.msaa, AntiAliasingPreference::X8);
    assert!(widget.settings.rendering.smaa_enabled);
    assert_eq!(snapshot.requested_msaa(), AntiAliasingPreference::X8);
    assert!(snapshot.requested_smaa());
}

#[test]
fn linked_snap_update_is_atomic_and_leaves_no_link_in_persisted_settings() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("settings.json");
    let mut widget =
        Widget::new_with_settings_path(test_config(), AppSettings::default(), Some(path.clone()));
    assert_eq!(widget.save_count, 0);

    let snapshot = widget.apply_control_action(ControlAction::SetAllSnaps {
        yaw_deg: 5.0,
        pitch_deg: 10.0,
        roll_deg: 20.0,
    });
    assert_eq!(snapshot.yaw_snap_deg(), 5.0);
    assert_eq!(snapshot.pitch_snap_deg(), 10.0);
    assert_eq!(snapshot.roll_snap_deg(), 20.0);
    assert_eq!(
        widget.save_count, 1,
        "linked edit must cause one save, not three transient updates"
    );

    let persisted = AppSettings::load_from_path(&path);
    assert_eq!(persisted.camera.yaw_snap_deg, 5.0);
    assert_eq!(persisted.camera.pitch_snap_deg, 10.0);
    assert_eq!(persisted.camera.roll_snap_deg, 20.0);
    let json = std::fs::read_to_string(&path).unwrap();
    assert!(
        !json.to_ascii_lowercase().contains("link"),
        "Link is UI-only and must not appear in persisted JSON: {json}"
    );
}

#[test]
fn malformed_values_are_sanitized_through_authoritative_policy() {
    let mut widget = test_widget();

    let expected_fov = CameraSettings {
        fov_deg: f32::NAN,
        ..CameraSettings::default()
    }
    .sanitized()
    .fov_deg;
    let snapshot = widget.apply_control_action(ControlAction::SetBaseFov(f32::NAN));
    assert_eq!(snapshot.base_fov_deg(), expected_fov);
    assert_eq!(snapshot.base_fov_deg(), 40.0);

    let snapshot = widget.apply_control_action(ControlAction::SetBaseFov(1000.0));
    assert_eq!(snapshot.base_fov_deg(), 179.0);
    let snapshot = widget.apply_control_action(ControlAction::SetBaseFov(-50.0));
    assert_eq!(snapshot.base_fov_deg(), 1.0);

    let expected_distance = CameraSettings {
        distance_scale: f32::INFINITY,
        ..CameraSettings::default()
    }
    .sanitized()
    .distance_scale;
    let snapshot = widget.apply_control_action(ControlAction::SetBaseDistance(f32::INFINITY));
    assert_eq!(snapshot.base_distance_scale(), expected_distance);
    assert_eq!(snapshot.base_distance_scale(), 0.6);

    let snapshot = widget.apply_control_action(ControlAction::SetBaseDistance(-5.0));
    assert_eq!(snapshot.base_distance_scale(), 0.1);
    let snapshot = widget.apply_control_action(ControlAction::SetBaseDistance(100.0));
    assert_eq!(snapshot.base_distance_scale(), 10.0);

    let snapshot = widget.apply_control_action(ControlAction::SetYawSnap(f32::NAN));
    assert_eq!(snapshot.yaw_snap_deg(), 15.0);
    let snapshot = widget.apply_control_action(ControlAction::SetYawSnap(0.0));
    assert_eq!(snapshot.yaw_snap_deg(), 0.1);
    let snapshot = widget.apply_control_action(ControlAction::SetRollSnap(500.0));
    assert_eq!(snapshot.roll_snap_deg(), 90.0);

    let all = widget.apply_control_action(ControlAction::SetAllSnaps {
        yaw_deg: f32::NAN,
        pitch_deg: f32::INFINITY,
        roll_deg: -1000.0,
    });
    assert_eq!(all.yaw_snap_deg(), 15.0);
    assert_eq!(all.pitch_snap_deg(), 15.0);
    assert_eq!(all.roll_snap_deg(), 0.1);
}

#[test]
fn requested_and_effective_msaa_can_differ() {
    let mut widget = test_widget();
    assert_eq!(
        widget.controls_snapshot().requested_msaa(),
        AntiAliasingPreference::Off
    );

    // Pending window: requested moves immediately, effective waits for the
    // between-frame renderer application.
    widget.apply_control_action(ControlAction::RequestMsaa(AntiAliasingPreference::X8));
    let snapshot = widget.controls_snapshot();
    assert_eq!(snapshot.requested_msaa(), AntiAliasingPreference::X8);
    assert!(snapshot.msaa_pending());

    // Hardware fallback window: simulate a renderer that accepted 8x but can
    // only realize 4x. Requested stays 8x while effective is 4x.
    widget.aa.initialize_msaa_from_renderer(8, 4);
    let snapshot = widget.controls_snapshot();
    assert_eq!(snapshot.requested_msaa(), AntiAliasingPreference::X8);
    assert_eq!(snapshot.effective_msaa(), 4);
    assert_ne!(
        snapshot.requested_msaa().samples().unwrap_or(1),
        snapshot.effective_msaa()
    );
}

#[test]
fn requested_and_effective_smaa_can_differ() {
    let mut widget = test_widget();
    assert!(!widget.controls_snapshot().requested_smaa());
    assert!(!widget.controls_snapshot().effective_smaa());

    // Pending window: requested flips immediately, renderer-observed waits for
    // the between-frame application. The snapshot must not collapse them.
    widget.apply_control_action(ControlAction::RequestSmaa(true));
    let snapshot = widget.controls_snapshot();
    assert!(snapshot.requested_smaa());
    assert!(!snapshot.effective_smaa());
    assert!(snapshot.smaa_pending());
    assert_ne!(snapshot.requested_smaa(), snapshot.effective_smaa());

    // Renderer-observed window: simulate observed still off while requested on.
    widget.aa.initialize_smaa_from_renderer(true, false);
    let snapshot = widget.controls_snapshot();
    assert!(snapshot.requested_smaa());
    assert!(!snapshot.effective_smaa());

    widget.aa.initialize_smaa_from_renderer(true, true);
    let snapshot = widget.controls_snapshot();
    assert!(snapshot.requested_smaa());
    assert!(snapshot.effective_smaa());
}

#[test]
fn persistence_occurs_once_per_committed_settings_action() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("settings.json");
    let mut widget =
        Widget::new_with_settings_path(test_config(), AppSettings::default(), Some(path.clone()));
    assert_eq!(widget.save_count, 0);

    widget.apply_control_action(ControlAction::SetBaseFov(50.0));
    assert_eq!(widget.save_count, 1);
    assert_eq!(AppSettings::load_from_path(&path).camera.fov_deg, 50.0);

    // Identical re-request commits nothing and persists nothing.
    widget.apply_control_action(ControlAction::SetBaseFov(50.0));
    assert_eq!(widget.save_count, 1);

    // Session-only pose never persists.
    widget.apply_control_action(ControlAction::SetYaw(25.0));
    widget.apply_control_action(ControlAction::SetPitch(10.0));
    widget.apply_control_action(ControlAction::SetRoll(-12.0));
    widget.apply_control_action(ControlAction::ResetRuntimeCamera);
    assert_eq!(widget.save_count, 1);

    // One snap change is one save.
    widget.apply_control_action(ControlAction::SetYawSnap(7.0));
    assert_eq!(widget.save_count, 2);
    assert_eq!(AppSettings::load_from_path(&path).camera.yaw_snap_deg, 7.0);

    // Linked edit is still one save, not three.
    widget.apply_control_action(ControlAction::SetAllSnaps {
        yaw_deg: 6.0,
        pitch_deg: 12.0,
        roll_deg: 18.0,
    });
    assert_eq!(widget.save_count, 3);
    let persisted = AppSettings::load_from_path(&path);
    assert_eq!(persisted.camera.yaw_snap_deg, 6.0);
    assert_eq!(persisted.camera.pitch_snap_deg, 12.0);
    assert_eq!(persisted.camera.roll_snap_deg, 18.0);

    // AA requests queue renderer work without immediate persistence; the
    // existing between-frame accepted-only persistence is preserved.
    widget.apply_control_action(ControlAction::RequestMsaa(AntiAliasingPreference::X8));
    widget.apply_control_action(ControlAction::RequestSmaa(true));
    assert_eq!(widget.save_count, 3);
    let snapshot = widget.controls_snapshot();
    assert!(snapshot.msaa_pending());
    assert!(snapshot.smaa_pending());
    assert_eq!(
        AppSettings::load_from_path(&path).rendering.msaa,
        AntiAliasingPreference::default()
    );
}

#[test]
fn controls_snapshot_is_immutable_value_state() {
    let widget = test_widget();
    let snapshot: ControlsSnapshot = widget.controls_snapshot();
    let copy = snapshot;
    assert_eq!(snapshot, copy);
    // No setters exist: the only way to change state is another action.
    let mut widget = widget;
    let next = widget.apply_control_action(ControlAction::SetBaseFov(48.0));
    assert_eq!(next.base_fov_deg(), 48.0);
    assert_eq!(snapshot.base_fov_deg(), 40.0);
}
