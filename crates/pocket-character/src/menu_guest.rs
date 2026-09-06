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
//! packed controller input (`Guest::frame(0)`); desktop pointer, generic text
//! input, and semantic action lines travel over the private svc channel. Draw
//! data reaches the overlay pass through [`UiRenderer`] with `LoadOp::Load`
//! over the already-rendered character.

use anyhow::{Context, Result, anyhow, ensure};
use glam::Vec2;
use pocket_mod::Guest;
use pocket_ui_wgpu::{UiRenderer, UiSurface};
use pocket3d::gpu::Gpu;
use pocket3d::input::{EditKey, ImeInput};

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

/// The generic host→guest input frame. `edits` and `ime` remain separate
/// ordered streams because Pocket3D's `Input` exposes them that way; neither
/// stream is reconstructed from the other.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MenuInputFrame {
    pub(crate) edits: Vec<EditKey>,
    pub(crate) ime: Vec<ImeInput>,
    pub(crate) modifiers: MenuInputModifiers,
    pub(crate) cancelled: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct MenuInputModifiers {
    pub(crate) shift: bool,
    pub(crate) control: bool,
    pub(crate) alt: bool,
    pub(crate) super_key: bool,
}

/// Capture geometry is authored in PocketUI logical pixels. The Widget maps
/// it through the desktop scale factor before returning the physical-pixel
/// `pocket3d::app::TextInputRequest` to the Pocket3D window loop.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct MenuTextInputCapture {
    pub(crate) active: bool,
    pub(crate) cursor_area_logical_px: Option<(f32, f32, f32, f32)>,
}

fn encode_edit(edit: EditKey) -> serde_json::Value {
    match edit {
        EditKey::Char(value) => serde_json::json!({"kind": "char", "text": value.to_string()}),
        EditKey::Backspace => serde_json::json!({"kind": "key", "key": "backspace"}),
        EditKey::Delete => serde_json::json!({"kind": "key", "key": "delete"}),
        EditKey::Enter => serde_json::json!({"kind": "key", "key": "enter"}),
        EditKey::Tab => serde_json::json!({"kind": "key", "key": "tab"}),
        EditKey::Left => serde_json::json!({"kind": "key", "key": "left"}),
        EditKey::Right => serde_json::json!({"kind": "key", "key": "right"}),
        EditKey::Up => serde_json::json!({"kind": "key", "key": "up"}),
        EditKey::Down => serde_json::json!({"kind": "key", "key": "down"}),
        EditKey::Home => serde_json::json!({"kind": "key", "key": "home"}),
        EditKey::End => serde_json::json!({"kind": "key", "key": "end"}),
        EditKey::PageUp => serde_json::json!({"kind": "key", "key": "page_up"}),
        EditKey::PageDown => serde_json::json!({"kind": "key", "key": "page_down"}),
        EditKey::Escape => serde_json::json!({"kind": "key", "key": "escape"}),
    }
}

fn encode_ime(event: &ImeInput) -> serde_json::Value {
    match event {
        ImeInput::Enabled => serde_json::json!({"kind": "enabled"}),
        ImeInput::Preedit(text, range) => {
            serde_json::json!({"kind": "preedit", "text": text, "range_bytes": range})
        }
        ImeInput::Commit(text) => serde_json::json!({"kind": "commit", "text": text}),
        ImeInput::Disabled => serde_json::json!({"kind": "disabled"}),
    }
}

fn encode_input_frame(frame: &MenuInputFrame) -> serde_json::Value {
    serde_json::json!({
        "t": "input",
        "edits": frame.edits.iter().copied().map(encode_edit).collect::<Vec<_>>(),
        "ime": frame.ime.iter().map(encode_ime).collect::<Vec<_>>(),
        "modifiers": {
            "shift": frame.modifiers.shift,
            "control": frame.modifiers.control,
            "alt": frame.modifiers.alt,
            "super": frame.modifiers.super_key,
        },
        "cancelled": frame.cancelled,
    })
}

#[derive(serde::Deserialize)]
struct TextInputAreaWire {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

#[derive(serde::Deserialize)]
struct TextInputStateWire {
    t: String,
    active: bool,
    cursor_area_logical_px: Option<TextInputAreaWire>,
}

fn decode_text_input_state(line: &str) -> Option<MenuTextInputCapture> {
    let wire = serde_json::from_str::<TextInputStateWire>(line).ok()?;
    if wire.t != "text-input-state" {
        return None;
    }
    if !wire.active {
        return Some(MenuTextInputCapture::default());
    }
    let Some(area) = wire.cursor_area_logical_px else {
        // A syntactically valid but unusable active report must not leave a
        // stale native capture enabled.
        return Some(MenuTextInputCapture::default());
    };
    let values = [area.x, area.y, area.width, area.height];
    if !values.iter().all(|value| value.is_finite()) {
        return Some(MenuTextInputCapture::default());
    }
    Some(MenuTextInputCapture {
        active: true,
        cursor_area_logical_px: Some((area.x, area.y, area.width, area.height)),
    })
}

/// Discrete intents accepted from the PocketUI controls guest. The guest only
/// names an operation; the widget applies it to the authoritative live camera
/// or routes an explicit Save through the persistence boundary.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum MenuAction {
    DistanceDecrement,
    DistanceIncrement,
    FovDecrement,
    FovIncrement,
    SetEffectiveDistance(f32),
    SetEffectiveFov(f32),
    SaveCamera,
    ResetRuntimeCamera,
}

/// Private guest→host action wire type. Keep this separate from the public
/// controls model so malformed or future messages can be ignored without
/// exposing arbitrary values to the widget.
#[derive(serde::Deserialize)]
struct MenuActionWire {
    t: String,
    action: String,
    value: Option<f32>,
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
        "set_effective_distance" => wire
            .value
            .filter(|value| value.is_finite())
            .map(MenuAction::SetEffectiveDistance),
        "set_effective_fov" => wire
            .value
            .filter(|value| value.is_finite())
            .map(MenuAction::SetEffectiveFov),
        "save_camera" => Some(MenuAction::SaveCamera),
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
    text_input_capture: MenuTextInputCapture,
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
            text_input_capture: MenuTextInputCapture::default(),
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

    /// Queue one generic text-input frame. The frame is consumed by the same
    /// guest turn and svc queue as pointer/state messages, so the host does
    /// not introduce another input event representation or poller.
    pub(crate) fn push_input_frame(&self, frame: &MenuInputFrame) {
        self.surface.svc_push(encode_input_frame(frame).to_string());
    }

    /// Drain accepted semantic actions from the guest. Every line is consumed
    /// in this tick, including malformed/unknown lines, so a bad guest cannot
    /// grow the svc queue indefinitely.
    pub fn drain_actions(&mut self) -> Vec<MenuAction> {
        let mut actions = Vec::new();
        for line in self.surface.svc_drain() {
            if let Some(capture) = decode_text_input_state(&line) {
                self.text_input_capture = capture;
            } else if let Some(action) = decode_menu_action(&line) {
                actions.push(action);
            }
        }
        actions
    }

    pub(crate) fn text_input_capture(&self) -> MenuTextInputCapture {
        self.text_input_capture
    }

    pub(crate) fn clear_text_input_state(&mut self) {
        self.text_input_capture = MenuTextInputCapture::default();
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
    use super::{
        MenuAction, MenuInputFrame, MenuInputModifiers, MenuState, MenuTextInputCapture,
        decode_menu_action, decode_text_input_state, encode_input_frame,
    };
    use glam::Vec2;
    use pocket3d::input::{EditKey, ImeInput};

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
                r#"{"t":"action","action":"set_effective_distance","value":9.5}"#,
                MenuAction::SetEffectiveDistance(9.5),
            ),
            (
                r#"{"t":"action","action":"set_effective_fov","value":120.2}"#,
                MenuAction::SetEffectiveFov(120.2),
            ),
            (
                r#"{"t":"action","action":"save_camera"}"#,
                MenuAction::SaveCamera,
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
            r#"{"t":"action","action":"set_effective_fov"}"#,
            r#"{"t":"action","action":"set_effective_distance","value":null}"#,
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

    #[test]
    fn input_wire_preserves_edit_and_ime_order_and_raw_preedit_range() {
        let value = encode_input_frame(&MenuInputFrame {
            edits: vec![EditKey::Char('é'), EditKey::Backspace, EditKey::Enter],
            ime: vec![
                ImeInput::Enabled,
                ImeInput::Preedit("候補".into(), Some((3, 6))),
                ImeInput::Commit("候補".into()),
                ImeInput::Disabled,
            ],
            modifiers: MenuInputModifiers {
                shift: true,
                control: false,
                alt: true,
                super_key: false,
            },
            cancelled: false,
        });

        assert_eq!(value["edits"][0]["kind"], "char");
        assert_eq!(value["edits"][0]["text"], "é");
        assert_eq!(value["edits"][1]["key"], "backspace");
        assert_eq!(value["edits"][2]["key"], "enter");
        assert_eq!(value["ime"][1]["range_bytes"], serde_json::json!([3, 6]));
        assert_eq!(value["modifiers"]["shift"], true);
        assert_eq!(value["modifiers"]["alt"], true);
    }

    #[test]
    fn text_input_state_requires_finite_cursor_geometry_when_active() {
        assert_eq!(
            decode_text_input_state(
                r#"{"t":"text-input-state","active":true,"cursor_area_logical_px":{"x":10,"y":20,"width":1,"height":16}}"#
            ),
            Some(MenuTextInputCapture {
                active: true,
                cursor_area_logical_px: Some((10.0, 20.0, 1.0, 16.0)),
            })
        );
        assert_eq!(
            decode_text_input_state(
                r#"{"t":"text-input-state","active":false,"cursor_area_logical_px":{"x":10,"y":20,"width":1,"height":16}}"#
            ),
            Some(MenuTextInputCapture::default())
        );
        assert_eq!(
            decode_text_input_state(
                r#"{"t":"text-input-state","active":true,"cursor_area_logical_px":null}"#
            ),
            Some(MenuTextInputCapture::default())
        );
    }
}
