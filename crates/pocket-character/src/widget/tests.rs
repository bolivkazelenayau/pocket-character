use super::*;
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
    widget.set_camera_settings(CameraSettings {
        fov_deg: 35.0,
        distance_scale: 0.75,
        headroom: 0.08,
        ..CameraSettings::default()
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
    assert!(widget.camera_controls.camera_controls_enabled());
    assert_eq!(widget.camera_settings, CameraSettings::default());

    input.inject_key(KeyCode::F8, false);
    input.end_frame();
    input.inject_key(KeyCode::KeyE, true);
    widget.frame(1.0 / 60.0, &input);
    assert!(widget.effective_camera_values().settings.fov_deg > 40.0);
    assert_eq!(widget.camera_settings, CameraSettings::default());
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
