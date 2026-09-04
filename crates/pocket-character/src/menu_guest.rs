//! The `menu` surface: a private PocketUI guest rendered as an overlay.
//!
//! A separate QuickJS realm (its own [`pocket_mod::Guest`] runtime) hosting a
//! `@pocketjs/framework` TSX app, mirroring the vendored reference boot in
//! `engine/pocket3d/examples/uihost`:
//!
//!   UiSurface::new_with_density → feed_pak → Guest::new → surface.mount → guest.eval
//!
//! Deliberately independent of [`crate::guest::CharacterGuest`]: the two
//! guests share no realm, namespace, or state. This pass is render-only —
//! the guest turn runs with zero input (`Guest::frame(0)`), and the draw
//! data reaches the overlay pass through [`UiRenderer`] with `LoadOp::Load`
//! over the already-rendered character.

use anyhow::{Context, Result, anyhow, ensure};
use pocket_mod::Guest;
use pocket_ui_wgpu::{UiRenderer, UiSurface};
use pocket3d::gpu::Gpu;

/// The framework bundle bakes its tick rate (build default 60) and refuses a
/// host running another; the widget's fixed tick runs at the same rate.
const MENU_TICK_HZ: u32 = 60;
/// Must match the `--density=2` menu build in `scripts/build-ui.ts`.
const MENU_RASTER_DENSITY: u32 = 2;

/// svc service name the menu guest probes (`ui.svcOpen("controls")`, the
/// note-app dialect). Declared before `mount`, which publishes it.
const MENU_SVC: &str = "controls";

/// One-way host→guest controls facts for the render-only menu.
///
/// The private host→guest wire shape. Base values are persisted settings;
/// effective values include live keyboard/session adjustments. Serialized as
/// one JSON line per tick on the svc channel; TSX performs display formatting
/// only, with no validation or clamping — camera policy stays canonical in
/// Rust and is never duplicated there.
#[derive(serde::Serialize)]
struct MenuState {
    /// Line discriminator (the channel multiplexes by `t`, per the note-app
    /// dialect); the menu ignores any other `t`.
    t: &'static str,
    base_fov_deg: f32,
    base_distance_scale: f32,
    effective_fov_deg: f32,
    effective_distance_scale: f32,
}

pub struct MenuGuest {
    guest: Guest,
    surface: UiSurface,
    /// Overlay pipeline for the draw list; rebuilt if the target format ever
    /// differs from the one it was created for.
    renderer: UiRenderer,
    renderer_format: wgpu::TextureFormat,
}

impl MenuGuest {
    /// Boot the UI guest: feed the pak, mount `globalThis.ui`, eval the
    /// bundle. `viewport` is the initial logical UI size; `target_format`
    /// must be the format of the view the overlay pass will draw into (the
    /// renderer's color format — the transparent window's output matches it).
    pub fn boot(
        gpu: &Gpu,
        bundle: &str,
        pak: &[u8],
        viewport: (f32, f32),
        target_format: wgpu::TextureFormat,
    ) -> Result<MenuGuest> {
        let surface = UiSurface::new_with_density(viewport, MENU_RASTER_DENSITY);
        ensure!(
            surface.set_tick_rate(MENU_TICK_HZ),
            "menu ui surface rejected tick rate {MENU_TICK_HZ}"
        );
        surface.feed_pak(pak);
        surface.set_svc_allowlist([MENU_SVC]);
        let guest = Guest::new()?;
        surface.mount(&guest)?;
        guest.eval("menu", bundle)?;
        if !guest.has_frame() {
            return Err(anyhow!(
                "menu bundle evaluated but installed no frame() — is this a @pocketjs/framework app?"
            ));
        }
        log::info!(
            "menu guest booted ({} bytes js, {} bytes pak, viewport {}x{})",
            bundle.len(),
            pak.len(),
            viewport.0,
            viewport.1
        );
        Ok(MenuGuest {
            guest,
            surface,
            renderer: UiRenderer::new(gpu, target_format),
            renderer_format: target_format,
        })
    }

    /// Queue the latest controls facts for the guest's next `svcPoll`
    /// (one-way host→guest; call once per tick, before `step()`, so the
    /// framework frame that follows observes them).
    pub fn push_state(
        &self,
        base_fov_deg: f32,
        base_distance_scale: f32,
        effective_fov_deg: f32,
        effective_distance_scale: f32,
    ) -> Result<()> {
        let state = MenuState {
            t: "state",
            base_fov_deg,
            base_distance_scale,
            effective_fov_deg,
            effective_distance_scale,
        };
        let line = serde_json::to_string(&state).context("serialize menu state")?;
        self.surface.svc_push(line);
        Ok(())
    }

    /// One UI turn: framework frame with zero input (no controls wired in
    /// this pass), then one fixed-step core tick. Call once per host tick.
    pub fn step(&mut self) -> Result<()> {
        self.guest.frame(0)?;
        self.surface.tick();
        Ok(())
    }

    /// Live-viewport resize in logical pixels: relayout the core, then run the
    /// framework's installed resize hook so the mounted layers follow (the
    /// vendored desktop-host dialect). Safe to call every frame; work happens
    /// only on an actual change.
    pub fn set_viewport(&mut self, w: f32, h: f32) -> Result<()> {
        let changed = self.surface.with_ui(|ui| {
            let (vw, vh) = ui.viewport();
            let changed = vw != w || vh != h;
            if changed {
                ui.set_viewport(w, h);
            }
            changed
        });
        if changed {
            self.guest.eval(
                "resize-hook",
                &format!(
                    "globalThis.__pocketResizeViewport && globalThis.__pocketResizeViewport({w}, {h});"
                ),
            )?;
        }
        Ok(())
    }

    /// Record the overlay pass: the logical UI draw list is scaled into the
    /// physical `view` and alpha-blended over the finished frame
    /// (`LoadOp::Load` — never clear). `format` must match `view`'s format;
    /// the pipeline is rebuilt if it changed.
    pub fn render(
        &mut self,
        gpu: &Gpu,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        format: wgpu::TextureFormat,
        physical_size: (u32, u32),
        scale_factor: f32,
    ) -> Result<()> {
        if self.renderer_format != format {
            log::info!("menu overlay: rebuilding pipeline for {format:?}");
            self.renderer = UiRenderer::new(gpu, format);
            self.renderer_format = format;
        }
        self.surface.with_ui(|ui| {
            let words = ui.draw().words.clone();
            self.renderer.render_words_scaled(
                gpu,
                ui,
                &words,
                encoder,
                view,
                physical_size,
                scale_factor,
                wgpu::LoadOp::Load,
            )
        })?;
        Ok(())
    }
}

/// Read the built menu artifacts (bundle + pak) from their generated paths.
pub fn load_menu_assets(
    bundle_path: &std::path::Path,
    pak_path: &std::path::Path,
) -> Result<(String, Vec<u8>)> {
    let build_hint = "build it first: bun scripts/build-ui.ts";
    let bundle = std::fs::read_to_string(bundle_path).with_context(|| {
        format!(
            "reading menu bundle {}: {build_hint}",
            bundle_path.display()
        )
    })?;
    let pak = std::fs::read(pak_path)
        .with_context(|| format!("reading menu pak {}: {build_hint}", pak_path.display()))?;
    Ok((bundle, pak))
}

#[cfg(test)]
mod tests {
    use super::MenuState;

    #[test]
    fn menu_state_wire_keeps_base_and_effective_camera_values_explicit() {
        let value = serde_json::to_value(MenuState {
            t: "state",
            base_fov_deg: 40.0,
            base_distance_scale: 0.6,
            effective_fov_deg: 44.0,
            effective_distance_scale: 0.55,
        })
        .unwrap();

        for (name, expected) in [
            ("base_fov_deg", 40.0),
            ("base_distance_scale", 0.6),
            ("effective_fov_deg", 44.0),
            ("effective_distance_scale", 0.55),
        ] {
            let actual = value[name].as_f64().unwrap();
            assert!(
                (actual - expected).abs() < 1.0e-6,
                "{name}: {actual} != {expected}"
            );
        }
    }
}
