use std::time::Instant;

use super::camera::EffectiveCameraValues;
use super::camera::controls::CAMERA_CONTROL_HELP;

/// Rolling frame stats fed to the guest and to the measurement harness.
pub(super) struct FrameStats {
    frames: u32,
    cpu_ms_acc: f32,
    window_start: Instant,
    fps: f32,
    frame_ms: f32,
}

impl FrameStats {
    pub(super) fn new() -> Self {
        Self {
            frames: 0,
            cpu_ms_acc: 0.0,
            window_start: Instant::now(),
            fps: 0.0,
            frame_ms: 0.0,
        }
    }

    pub(super) fn record(&mut self, cpu_ms: f32) {
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

    pub(super) fn fps(&self) -> f32 {
        self.fps
    }

    pub(super) fn frame_ms(&self) -> f32 {
        self.frame_ms
    }
}

/// Rolling FPS measured at the rendered-frame cadence (`Widget::frame`).
pub(super) struct RenderFps {
    frames: u32,
    elapsed: f32,
    fps: f32,
}

impl RenderFps {
    pub(super) fn new() -> Self {
        Self {
            frames: 0,
            elapsed: 0.0,
            fps: 0.0,
        }
    }

    pub(super) fn record(&mut self, dt: f32) {
        self.frames += 1;
        self.elapsed += if dt.is_finite() && dt >= 0.0 { dt } else { 0.0 };
        if self.elapsed >= 1.0 {
            self.fps = self.frames as f32 / self.elapsed;
            self.frames = 0;
            self.elapsed = 0.0;
        }
    }

    pub(super) fn fps(&self) -> f32 {
        self.fps
    }
}

pub(super) struct DebugHudText {
    pub(super) title: &'static str,
    pub(super) lines: Vec<String>,
}

pub(super) fn format_debug_hud(
    size: (u32, u32),
    stats: &FrameStats,
    render_fps: &RenderFps,
    gpu_name: &str,
    backend: &str,
    requested_msaa: u32,
    effective_msaa: u32,
    smaa_enabled: bool,
    camera: EffectiveCameraValues,
    camera_controls_enabled: bool,
) -> DebugHudText {
    let camera_controls = if camera_controls_enabled {
        "Camera controls: on"
    } else {
        "Camera controls: off"
    };

    DebugHudText {
        title: "Pocket3D HUD",
        lines: vec![
            format!("FPS: {:.1}", render_fps.fps()),
            format!("Tick CPU: {:.2} ms", stats.frame_ms()),
            format!("GPU: {gpu_name}"),
            format!("Backend: {backend}"),
            format_msaa_hud_line(requested_msaa, effective_msaa),
            format_smaa_hud_line(smaa_enabled),
            format!("Cam FOV: {:.1} deg", camera.settings.fov_deg),
            format!(
                "Cam distance: {:.3}x height",
                camera.settings.distance_scale
            ),
            format!("Cam headroom: {:.3}", camera.settings.headroom),
            format!("Cam pan X: {:.3} NDC", camera.pan_ndc.x),
            format!("Cam pan Y: {:.3} NDC", camera.pan_ndc.y),
            format!("Cam yaw: {:.1} deg", camera.yaw_deg),
            format!("Cam roll: {:.1} deg", camera.roll_deg),
            format!("Cam pitch: {:.1} deg", camera.pitch_deg),
            camera_controls.into(),
            CAMERA_CONTROL_HELP[0].into(),
            CAMERA_CONTROL_HELP[1].into(),
            CAMERA_CONTROL_HELP[2].into(),
            CAMERA_CONTROL_HELP[3].into(),
            CAMERA_CONTROL_HELP[4].into(),
            CAMERA_CONTROL_HELP[5].into(),
            CAMERA_CONTROL_HELP[6].into(),
            CAMERA_CONTROL_HELP[7].into(),
            CAMERA_CONTROL_HELP[8].into(),
            CAMERA_CONTROL_HELP[9].into(),
            format!("Frame: {}x{}", size.0, size.1),
        ],
    }
}

pub(super) fn format_msaa_count(sample_count: u32) -> String {
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

    fn approx_eq(actual: f32, expected: f32) {
        assert!((actual - expected).abs() < 1.0e-5, "{actual} != {expected}");
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
        let mut render_fps = RenderFps::new();

        for _ in 0..4 {
            render_fps.record(0.25);
        }

        approx_eq(render_fps.fps(), 4.0);
    }
}
