//! The `menu` surface: a private PocketUI guest rendered as an overlay.
//!
//! A separate QuickJS realm (its own [`pocket_mod::Guest`] runtime) hosting a
//! `@pocketjs/framework` TSX app, mirroring the vendored reference boot in
//! `engine/pocket3d/examples/uihost`:
//!
//!   UiSurface::new_with_density → feed_pak → Guest::new → surface.mount → guest.eval
//!
//! Deliberately independent of [`crate::guest::CharacterGuest`]: the two
//! guests share no realm, namespace, or state. The guest turn runs with zero
//! packed controller input (`Guest::frame(0)`); desktop pointer facts and
//! semantic action lines travel over the private svc channel. Draw data
//! reaches the overlay pass through [`UiRenderer`] with `LoadOp::Load` over
//! the already-rendered character.

use anyhow::{Context, Result, anyhow, ensure};
use glam::Vec2;
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

fn menu_scale_factor(scale_factor: f64) -> f32 {
    let scale = scale_factor as f32;
    if scale_factor.is_finite() && scale_factor > 0.0 && scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    }
}

fn logical_pointer(cursor: Option<Vec2>, scale_factor: f64) -> Option<(f32, f32)> {
    let scale = menu_scale_factor(scale_factor);
    cursor
        .filter(|cursor| cursor.x.is_finite() && cursor.y.is_finite())
        .map(|cursor| (cursor.x / scale, cursor.y / scale))
}

/// Discrete intents accepted from the PocketUI controls guest. The guest only
/// names an operation; the widget resolves it against the current base values
/// before constructing the authoritative [`ControlAction`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MenuAction {
    DistanceDecrement,
    DistanceIncrement,
    FovDecrement,
    FovIncrement,
    ResetRuntimeCamera,
}

/// Private guest→host action wire type. Keep this separate from the public
/// controls model so malformed or future messages can be ignored without
/// exposing arbitrary values to the widget.
#[derive(serde::Deserialize)]
struct MenuActionWire {
    t: String,
    action: String,
}

fn decode_menu_action(line: &str) -> Option<MenuAction> {
    let wire = serde_json::from_str::<MenuActionWire>(line).ok()?;
    if wire.t != "action" {
        return None;
    }
    match wire.action.as_str() {
        "distance_decrement" => Some(MenuAction::DistanceDecrement),
        "distance_increment" => Some(MenuAction::DistanceIncrement),
        "fov_decrement" => Some(MenuAction::FovDecrement),
        "fov_increment" => Some(MenuAction::FovIncrement),
        "reset_runtime_camera" => Some(MenuAction::ResetRuntimeCamera),
        _ => None,
    }
}

/// Host→guest controls facts for the menu.
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
    /// Last logical pointer tuple sent to the guest. The position is retained
    /// while a button is held; if release arrives after `CursorLeft`, the
    /// bridge deliberately sends an outside sentinel instead of replaying it.
    last_pointer: Option<(f32, f32, bool)>,
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
            last_pointer: None,
            renderer: UiRenderer::new(gpu, target_format),
            renderer_format: target_format,
        })
    }

    /// Queue the latest controls facts for the guest's next `svcPoll`
    /// (host→guest; call once per tick, before `step()`, so the framework
    /// frame that follows observes them).
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

    /// Queue one pointer transition in logical pixels. The widget host calls
    /// this only while draining its frame-level transition buffer, so a fixed
    /// tick cannot replay a render-frame edge.
    pub fn push_pointer_transition(
        &mut self,
        cursor: Option<Vec2>,
        scale_factor: f64,
        button_down: bool,
    ) {
        let position = logical_pointer(cursor, scale_factor).or_else(|| {
            if button_down {
                self.last_pointer.map(|(x, y, _)| (x, y))
            } else if self.last_pointer.is_some_and(|(_, _, was_down)| was_down) {
                // CursorLeft clears Input::cursor(). A release in that state
                // must be outside every painted/focusable node so a
                // press-drag-release cannot activate the old target.
                Some((-1.0, -1.0))
            } else {
                None
            }
        });
        let Some((x, y)) = position else {
            return;
        };
        let next = (x, y, button_down);
        if self.last_pointer != Some(next) {
            self.surface.svc_push(
                serde_json::json!({"t": "mouse", "x": x, "y": y, "d": button_down}).to_string(),
            );
            self.last_pointer = Some(next);
        }
    }

    /// Cancel the guest-side press capture, even if the native input layer
    /// only reports focus loss and retains its last cursor position.
    pub fn cancel_pointer(&mut self) {
        self.surface.svc_push(
            serde_json::json!({"t": "mouse", "x": -1.0, "y": -1.0, "d": false}).to_string(),
        );
        self.last_pointer = None;
    }

    /// Drain accepted semantic actions from the guest. Every line is consumed
    /// in this tick, including malformed/unknown lines, so a bad guest cannot
    /// grow the svc queue indefinitely.
    pub fn drain_actions(&self) -> Vec<MenuAction> {
        self.surface
            .svc_drain()
            .into_iter()
            .filter_map(|line| decode_menu_action(&line))
            .collect()
    }

    /// Query the same retained PocketUI hit-test geometry used by the
    /// framework's `hitFocusable` path. A painted menu node owns the press;
    /// the widget never carries a second menu rectangle model.
    pub fn pointer_owns(&mut self, cursor: Vec2, scale_factor: f64) -> bool {
        let Some((x, y)) = logical_pointer(Some(cursor), scale_factor) else {
            return false;
        };
        self.surface.with_ui(|ui| ui.hit_test(x, y) != 0)
    }

    /// One UI turn: svc facts/pointer are observed by the framework frame,
    /// then the retained UI core advances one fixed tick. Call once per host
    /// tick.
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
    use super::{MenuAction, MenuState, decode_menu_action};
    use glam::Vec2;

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

    #[test]
    fn menu_action_decoder_accepts_only_known_semantic_actions() {
        let cases = [
            (
                r#"{"t":"action","action":"distance_decrement"}"#,
                MenuAction::DistanceDecrement,
            ),
            (
                r#"{"t":"action","action":"distance_increment"}"#,
                MenuAction::DistanceIncrement,
            ),
            (
                r#"{"t":"action","action":"fov_decrement"}"#,
                MenuAction::FovDecrement,
            ),
            (
                r#"{"t":"action","action":"fov_increment"}"#,
                MenuAction::FovIncrement,
            ),
            (
                r#"{"t":"action","action":"reset_runtime_camera"}"#,
                MenuAction::ResetRuntimeCamera,
            ),
        ];

        for (line, expected) in cases {
            assert_eq!(decode_menu_action(line), Some(expected), "{line}");
        }
    }

    #[test]
    fn malformed_and_unknown_menu_action_lines_are_ignored() {
        for line in [
            "",
            "not-json",
            r#"{"t":"state","action":"fov_increment"}"#,
            r#"{"t":"action"}"#,
            r#"{"t":"action","action":null}"#,
            r#"{"t":"action","action":"future_value"}"#,
        ] {
            assert_eq!(decode_menu_action(line), None, "{line}");
        }
    }

    #[test]
    fn desktop_pointer_coordinates_use_the_window_scale_factor() {
        assert_eq!(
            super::logical_pointer(Some(Vec2::new(300.0, 150.0)), 2.0),
            Some((150.0, 75.0))
        );
        let mapped = super::logical_pointer(Some(Vec2::new(301.0, 151.0)), 1.5).unwrap();
        assert!((mapped.0 - 200.66667).abs() < 1.0e-4);
        assert!((mapped.1 - 100.66667).abs() < 1.0e-4);
        assert_eq!(super::logical_pointer(None, 2.0), None);
        assert_eq!(
            super::logical_pointer(Some(Vec2::new(f32::NAN, 1.0)), 2.0),
            None
        );
    }
}
