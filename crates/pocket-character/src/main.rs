//! pocket-character: the airi-parity character widget on the Pocket runtime.
//!
//! Windowed mode is the product: a transparent, undecorated, always-on-top
//! 450×600 window (airi's stage geometry) rendering the VRM character.
//! `--headless-shot` drives the same [`Game`] object without a window and
//! saves an RGBA screenshot — CI-friendly parity checks.

mod guest;
mod menu_guest;
mod settings;
mod widget;

use std::path::PathBuf;

use anyhow::Result;
use pocket3d::app::{AppConfig, Game};
use pocket3d::gpu::{Gpu, OffscreenTarget};
use pocket3d::input::Input;
use pocket3d::renderer::Renderer;

use settings::AppSettings;
use widget::{Widget, WidgetConfig};

const SIZE: (u32, u32) = (450, 600);
const TICK_HZ: f32 = 60.0;

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == name)
        .and_then(|index| args.get(index + 1).cloned())
}

fn explicit_max_fps(args: &[String]) -> Option<f32> {
    flag(args, "--max-fps")
        .and_then(|value| value.parse().ok())
        .filter(|value: &f32| value.is_finite())
}

fn apply_cli_overrides(mut settings: AppSettings, args: &[String]) -> AppSettings {
    // `--max-fps` is the only existing CLI option equivalent to a persisted
    // setting. Unlike a parser default, this is only applied when present.
    if let Some(max_fps) = explicit_max_fps(args) {
        settings.rendering.max_fps = max_fps;
    }
    settings.sanitized()
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args: Vec<String> = std::env::args().collect();
    let root = std::env::var("POCKET_CHARACTER_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."));

    let mut cfg = WidgetConfig {
        model_path: flag(&args, "--model")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join("assets/AvatarSample_A.vrm")),
        vrma_path: flag(&args, "--vrma")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join("assets/idle_loop.vrma")),
        bundle_path: flag(&args, "--bundle")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join("dist/character.js")),
        menu_bundle_path: flag(&args, "--menu-bundle")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join("dist/menu.js")),
        menu_pak_path: flag(&args, "--menu-pak")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join("dist/menu.pak")),
        size: SIZE,
        frames: flag(&args, "--frames").and_then(|value| value.parse().ok()),
    };

    if let Some(out) = flag(&args, "--headless-shot") {
        let ticks: u32 = flag(&args, "--ticks")
            .and_then(|value| value.parse().ok())
            .unwrap_or(60);
        return headless_shot(cfg, ticks, PathBuf::from(out));
    }
    if let Some(dir) = flag(&args, "--headless-seq") {
        let ticks: u32 = flag(&args, "--ticks")
            .and_then(|value| value.parse().ok())
            .unwrap_or(300);
        let skip: u32 = flag(&args, "--skip")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        return headless_seq(cfg, ticks, skip, PathBuf::from(dir));
    }

    // Desktop settings are deliberately resolved only after headless exits.
    // Headless verification has a fixed 1x renderer and must not inherit a
    // user's interactive AA preferences from %APPDATA%.
    let settings_path = AppSettings::path();
    let settings = apply_cli_overrides(AppSettings::load(), &args);
    cfg.size = (settings.window.width, settings.window.height);
    let widget = Widget::new_with_settings_path(cfg, settings.clone(), settings_path);
    pocket3d::app::run(
        AppConfig {
            title: "pocket-character".into(),
            size: (settings.window.width, settings.window.height),
            tick_hz: TICK_HZ,
            capture_mouse: false,
            transparent: true,
            decorations: false,
            always_on_top: settings.window.always_on_top,
            resizable: settings.window.resizable,
            max_fps: Some(settings.rendering.max_fps),
            drag_window: true,
            requested_sample_count: settings.rendering.msaa.samples().unwrap_or(1),
        },
        widget,
    )
}

/// Like `headless_shot`, but renders EVERY tick after `skip` into
/// `dir/frame-%05d.png` — filmstrips and videos for docs come from this.
fn headless_seq(cfg: WidgetConfig, ticks: u32, skip: u32, dir: PathBuf) -> Result<()> {
    let size = cfg.size;
    let gpu = Gpu::new_headless()?;
    let mut renderer = Renderer::new(&gpu, pocket3d::gpu::OFFSCREEN_FORMAT)?;
    let mut widget = Widget::new(cfg);
    widget.init(&gpu, &mut renderer)?;
    std::fs::create_dir_all(&dir)?;

    let input = Input::default();
    let dt = 1.0 / TICK_HZ;
    let target = OffscreenTarget::new(&gpu, size.0, size.1);
    for i in 0..(skip + ticks) {
        widget.frame(dt, &input);
        widget.tick(dt, &input);
        if i < skip {
            continue;
        }
        let (scene, camera, hud) = widget.compose(0.0, i as f32 * dt, size);
        renderer.render(&gpu, &target.view, size, scene, camera, hud);
        overlay_offscreen(&gpu, &mut widget, &target.view, size);
        target.save_png(&gpu, &dir.join(format!("frame-{:05}.png", i - skip)))?;
    }
    println!("wrote {} frames to {}", ticks, dir.display());
    Ok(())
}

/// The windowed loop records the menu overlay after the scene pass and
/// before present; the headless drives replay the same presentation step so
/// captures include the PocketUI overlay.
fn overlay_offscreen(gpu: &Gpu, widget: &mut Widget, view: &wgpu::TextureView, size: (u32, u32)) {
    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    widget.overlay(
        gpu,
        &mut encoder,
        view,
        pocket3d::gpu::OFFSCREEN_FORMAT,
        size,
    );
    gpu.queue.submit([encoder.finish()]);
}

/// Drive the widget for `ticks` fixed steps without a window, render one
/// frame offscreen, save it (alpha preserved — the transparent background
/// stays transparent in the PNG).
fn headless_shot(cfg: WidgetConfig, ticks: u32, out: PathBuf) -> Result<()> {
    let size = cfg.size;
    let gpu = Gpu::new_headless()?;
    let mut renderer = Renderer::new(&gpu, pocket3d::gpu::OFFSCREEN_FORMAT)?;
    let mut widget = Widget::new(cfg);
    widget.init(&gpu, &mut renderer)?;

    let input = Input::default();
    let dt = 1.0 / TICK_HZ;
    for _ in 0..ticks {
        widget.frame(dt, &input);
        widget.tick(dt, &input);
    }
    let (scene, camera, hud) = widget.compose(0.0, ticks as f32 * dt, size);
    let target = OffscreenTarget::new(&gpu, size.0, size.1);
    renderer.render(&gpu, &target.view, size, scene, camera, hud);
    overlay_offscreen(&gpu, &mut widget, &target.view, size);
    target.save_png(&gpu, &out)?;
    println!("wrote {}", out.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use settings::RenderSettings;

    #[test]
    fn persisted_max_fps_wins_when_cli_is_absent() {
        let persisted = AppSettings {
            rendering: RenderSettings {
                max_fps: 30.0,
                ..RenderSettings::default()
            },
            ..AppSettings::default()
        };

        let merged = apply_cli_overrides(persisted, &[]);
        assert_eq!(merged.rendering.max_fps, 30.0);
    }

    #[test]
    fn explicit_max_fps_overrides_persisted_and_default_values() {
        let persisted = AppSettings {
            rendering: RenderSettings {
                max_fps: 30.0,
                ..RenderSettings::default()
            },
            ..AppSettings::default()
        };

        let merged = apply_cli_overrides(
            persisted,
            &["pocket-character".into(), "--max-fps".into(), "120".into()],
        );
        assert_eq!(merged.rendering.max_fps, 120.0);

        let defaults = apply_cli_overrides(AppSettings::default(), &[]);
        assert_eq!(defaults.rendering.max_fps, 60.0);
    }
}
