use glam::{Vec2, Vec3};
use pocket3d::input::Input;
use pocket3d::winit::keyboard::KeyCode;

use crate::settings::CameraSettings;

use super::{CameraRuntimeAdjustments, EffectiveCameraValues, admit_pan_input, finite_value};

const CAMERA_FOV_RATE_DEG_PER_SEC: f32 = 45.0;
const CAMERA_DISTANCE_RATE_PER_SEC: f32 = 0.75;
/// Temporary keyboard tuning in rest-center NDC units per second. This
/// approximately preserves the previous manual pan feel for the default
/// character frame and is deliberately separate from the safety boundary.
const CAMERA_PAN_WITNESS_NDC_RATE_PER_SEC: f32 = 0.75;
const CAMERA_YAW_RATE_DEG_PER_SEC: f32 = 90.0;
const CAMERA_ROLL_RATE_DEG_PER_SEC: f32 = 90.0;
const HORIZONTAL_SNAP_DEG: f32 = 15.0;
const ROLL_SNAP_DEG: f32 = HORIZONTAL_SNAP_DEG;
const YAW_SNAP_DEG: f32 = HORIZONTAL_SNAP_DEG;
const PITCH_SNAP_DEG: f32 = HORIZONTAL_SNAP_DEG;
const SNAP_REPEAT_DELAY_SEC: f32 = 0.30;
const SNAP_REPEAT_INTERVAL_SEC: f32 = 0.10;
#[cfg(test)]
const ROLL_SNAP_REPEAT_DELAY_SEC: f32 = SNAP_REPEAT_DELAY_SEC;
#[cfg(test)]
const ROLL_SNAP_REPEAT_INTERVAL_SEC: f32 = SNAP_REPEAT_INTERVAL_SEC;
#[cfg(test)]
const HORIZONTAL_SNAP_REPEAT_DELAY_SEC: f32 = SNAP_REPEAT_DELAY_SEC;
#[cfg(test)]
const HORIZONTAL_SNAP_REPEAT_INTERVAL_SEC: f32 = SNAP_REPEAT_INTERVAL_SEC;
#[cfg(test)]
const VERTICAL_SNAP_REPEAT_DELAY_SEC: f32 = SNAP_REPEAT_DELAY_SEC;
#[cfg(test)]
const VERTICAL_SNAP_REPEAT_INTERVAL_SEC: f32 = SNAP_REPEAT_INTERVAL_SEC;
const CAMERA_PITCH_RATE_DEG_PER_SEC: f32 = 60.0;
pub(crate) const CAMERA_CONTROL_HELP: [&str; 10] = [
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
];

fn axis(input: &Input, positive: KeyCode, negative: KeyCode) -> f32 {
    (input.key_down(positive) as i8 - input.key_down(negative) as i8) as f32
}

fn modifier_down(input: &Input, left: KeyCode, right: KeyCode) -> bool {
    input.key_down(left) || input.key_down(right)
}

fn fov_axis(input: &Input) -> f32 {
    let increase = input.key_down(KeyCode::KeyE) || input.key_down(KeyCode::BracketRight);
    let decrease = input.key_down(KeyCode::KeyQ) || input.key_down(KeyCode::BracketLeft);
    (increase as i8 - decrease as i8) as f32
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VerticalCameraAction {
    Pan,
    Zoom,
    Pitch,
    SnapPitch,
}

fn vertical_camera_action(input: &Input) -> VerticalCameraAction {
    let ctrl = modifier_down(input, KeyCode::ControlLeft, KeyCode::ControlRight);
    let alt = modifier_down(input, KeyCode::AltLeft, KeyCode::AltRight);
    if alt && ctrl {
        VerticalCameraAction::SnapPitch
    } else if ctrl {
        VerticalCameraAction::Pitch
    } else if modifier_down(input, KeyCode::ShiftLeft, KeyCode::ShiftRight) {
        VerticalCameraAction::Zoom
    } else {
        VerticalCameraAction::Pan
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HorizontalCameraAction {
    SnapRoll,
    Yaw,
    Pan,
    Roll,
    SnapYaw,
}

fn horizontal_camera_action(input: &Input) -> HorizontalCameraAction {
    let ctrl = modifier_down(input, KeyCode::ControlLeft, KeyCode::ControlRight);
    let alt = modifier_down(input, KeyCode::AltLeft, KeyCode::AltRight);
    let shift = modifier_down(input, KeyCode::ShiftLeft, KeyCode::ShiftRight);
    if alt && ctrl {
        HorizontalCameraAction::SnapRoll
    } else if ctrl {
        HorizontalCameraAction::Roll
    } else if alt && shift {
        HorizontalCameraAction::SnapYaw
    } else if shift {
        HorizontalCameraAction::Yaw
    } else {
        HorizontalCameraAction::Pan
    }
}

fn snap_degrees(current_deg: f32, direction: i8, snap_deg: f32) -> f32 {
    let current_deg = super::normalize_degrees(current_deg);
    let snapped = match direction.cmp(&0) {
        std::cmp::Ordering::Greater => (current_deg / snap_deg).floor() + 1.0,
        std::cmp::Ordering::Less => (current_deg / snap_deg).ceil() - 1.0,
        std::cmp::Ordering::Equal => return current_deg,
    } * snap_deg;
    super::normalize_degrees(snapped)
}

#[cfg(test)]
fn snap_roll_degrees(current_deg: f32, direction: i8) -> f32 {
    snap_degrees(current_deg, direction, ROLL_SNAP_DEG)
}

#[cfg(test)]
fn snap_yaw_degrees(current_deg: f32, direction: i8) -> f32 {
    snap_degrees(current_deg, direction, YAW_SNAP_DEG)
}

fn apply_snap_steps(current_deg: f32, direction: i8, steps: u32, snap_deg: f32) -> f32 {
    if steps == 0 {
        return super::normalize_degrees(current_deg);
    }

    let first_step = snap_degrees(current_deg, direction, snap_deg);
    let steps_per_turn = (360.0 / snap_deg).round() as u32;
    let additional_steps = (steps - 1) % steps_per_turn;
    super::normalize_degrees(first_step + direction as f32 * additional_steps as f32 * snap_deg)
}

fn apply_roll_snap_steps(current_deg: f32, direction: i8, steps: u32) -> f32 {
    apply_snap_steps(current_deg, direction, steps, ROLL_SNAP_DEG)
}

fn apply_yaw_snap_steps(current_deg: f32, direction: i8, steps: u32) -> f32 {
    apply_snap_steps(current_deg, direction, steps, YAW_SNAP_DEG)
}

fn sanitize_pitch_degrees(pitch_deg: f32) -> f32 {
    CameraRuntimeAdjustments {
        pitch_deg,
        ..CameraRuntimeAdjustments::default()
    }
    .sanitized()
    .pitch_deg
}

fn snap_pitch_degrees(current_deg: f32, direction: i8) -> f32 {
    let current_deg = sanitize_pitch_degrees(current_deg);
    let snapped = match direction.cmp(&0) {
        std::cmp::Ordering::Greater => (current_deg / PITCH_SNAP_DEG).floor() + 1.0,
        std::cmp::Ordering::Less => (current_deg / PITCH_SNAP_DEG).ceil() - 1.0,
        std::cmp::Ordering::Equal => return current_deg,
    } * PITCH_SNAP_DEG;
    sanitize_pitch_degrees(snapped)
}

fn apply_pitch_snap_steps(current_deg: f32, direction: i8, steps: u32) -> f32 {
    if steps == 0 {
        return sanitize_pitch_degrees(current_deg);
    }

    let first_step = snap_pitch_degrees(current_deg, direction);
    let additional_steps = steps.saturating_sub(1) as f32;
    sanitize_pitch_degrees(first_step + direction as f32 * additional_steps * PITCH_SNAP_DEG)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum HorizontalSnapMode {
    #[default]
    None,
    Roll,
    Yaw,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct SnapRepeatState<M> {
    active_mode: M,
    active_direction: i8,
    held_duration_sec: f32,
    repeat_steps_emitted: u32,
}

type HorizontalSnapRepeatState = SnapRepeatState<HorizontalSnapMode>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum VerticalSnapMode {
    #[default]
    None,
    Pitch,
}

type VerticalSnapRepeatState = SnapRepeatState<VerticalSnapMode>;

fn horizontal_snap_mode(action: HorizontalCameraAction) -> HorizontalSnapMode {
    match action {
        HorizontalCameraAction::SnapRoll => HorizontalSnapMode::Roll,
        HorizontalCameraAction::SnapYaw => HorizontalSnapMode::Yaw,
        _ => HorizontalSnapMode::None,
    }
}

fn scheduled_snap_repeats(held_duration_sec: f32) -> u32 {
    const TIME_EPSILON_SEC: f32 = 1.0e-6;

    if held_duration_sec + TIME_EPSILON_SEC < SNAP_REPEAT_DELAY_SEC {
        return 0;
    }

    (((held_duration_sec + TIME_EPSILON_SEC - SNAP_REPEAT_DELAY_SEC) / SNAP_REPEAT_INTERVAL_SEC)
        .floor() as u32)
        .saturating_add(1)
}

fn requested_snap_steps<M>(
    state: &mut SnapRepeatState<M>,
    snap_mode: M,
    snap_selected: bool,
    held_direction: i8,
    dt: f32,
) -> (i8, u32)
where
    M: Copy + Default + PartialEq,
{
    if !snap_selected || held_direction == 0 {
        *state = SnapRepeatState::default();
        return (0, 0);
    }

    if state.active_mode != snap_mode || state.active_direction != held_direction {
        *state = SnapRepeatState::default();
        state.active_mode = snap_mode;
        state.active_direction = held_direction;
        return (held_direction, 1);
    }

    state.held_duration_sec += dt;
    let scheduled_repeats = scheduled_snap_repeats(state.held_duration_sec);
    let due_repeats = scheduled_repeats.saturating_sub(state.repeat_steps_emitted);
    state.repeat_steps_emitted = scheduled_repeats;

    (held_direction, due_repeats)
}

fn requested_horizontal_snap_steps(
    state: &mut HorizontalSnapRepeatState,
    input: &Input,
    horizontal_action: HorizontalCameraAction,
    dt: f32,
) -> (i8, u32) {
    let snap_mode = horizontal_snap_mode(horizontal_action);
    let held_direction = match snap_mode {
        HorizontalSnapMode::Roll => axis(input, KeyCode::ArrowRight, KeyCode::ArrowLeft) as i8,
        HorizontalSnapMode::Yaw => axis(input, KeyCode::ArrowLeft, KeyCode::ArrowRight) as i8,
        HorizontalSnapMode::None => 0,
    };
    requested_snap_steps(
        state,
        snap_mode,
        snap_mode != HorizontalSnapMode::None,
        held_direction,
        dt,
    )
}

fn vertical_snap_mode(action: VerticalCameraAction) -> VerticalSnapMode {
    match action {
        VerticalCameraAction::SnapPitch => VerticalSnapMode::Pitch,
        _ => VerticalSnapMode::None,
    }
}

fn requested_vertical_snap_steps(
    state: &mut VerticalSnapRepeatState,
    input: &Input,
    vertical_action: VerticalCameraAction,
    dt: f32,
) -> (i8, u32) {
    let snap_mode = vertical_snap_mode(vertical_action);
    let held_direction = match snap_mode {
        VerticalSnapMode::Pitch => axis(input, KeyCode::ArrowUp, KeyCode::ArrowDown) as i8,
        VerticalSnapMode::None => 0,
    };
    requested_snap_steps(
        state,
        snap_mode,
        snap_mode != VerticalSnapMode::None,
        held_direction,
        dt,
    )
}

fn requested_pan_witness_delta(
    input: &Input,
    dt: f32,
    horizontal_action: HorizontalCameraAction,
    vertical_action: VerticalCameraAction,
) -> Vec2 {
    Vec2::new(
        if horizontal_action == HorizontalCameraAction::Pan {
            axis(input, KeyCode::ArrowRight, KeyCode::ArrowLeft)
        } else {
            0.0
        },
        if vertical_action == VerticalCameraAction::Pan {
            axis(input, KeyCode::ArrowUp, KeyCode::ArrowDown)
        } else {
            0.0
        },
    ) * CAMERA_PAN_WITNESS_NDC_RATE_PER_SEC
        * dt
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CameraControls {
    camera_controls_enabled: bool,
    camera_adjustments: CameraRuntimeAdjustments,
    horizontal_snap_repeat: HorizontalSnapRepeatState,
    vertical_snap_repeat: VerticalSnapRepeatState,
}

impl CameraControls {
    pub(crate) fn adjustments(&self) -> CameraRuntimeAdjustments {
        self.camera_adjustments
    }

    pub(crate) fn effective_camera_values(
        &self,
        camera_settings: CameraSettings,
    ) -> EffectiveCameraValues {
        self.camera_adjustments.effective(camera_settings)
    }

    pub(crate) fn camera_controls_enabled(&self) -> bool {
        self.camera_controls_enabled
    }

    #[cfg(test)]
    pub(in crate::widget) fn set_adjustments(&mut self, adjustments: CameraRuntimeAdjustments) {
        self.camera_adjustments = adjustments.sanitized();
    }

    pub(crate) fn reset_adjustments(&mut self) {
        self.horizontal_snap_repeat = HorizontalSnapRepeatState::default();
        self.vertical_snap_repeat = VerticalSnapRepeatState::default();
        self.camera_adjustments = CameraRuntimeAdjustments::default();
    }

    pub(crate) fn apply_frame(
        &mut self,
        dt: f32,
        input: &Input,
        aabb: Option<(Vec3, Vec3)>,
        camera_settings: CameraSettings,
        viewport_aspect: f32,
    ) -> bool {
        if input.key_pressed(KeyCode::F8) {
            self.camera_controls_enabled = !self.camera_controls_enabled;
            if !self.camera_controls_enabled {
                self.horizontal_snap_repeat = HorizontalSnapRepeatState::default();
                self.vertical_snap_repeat = VerticalSnapRepeatState::default();
            }
        }

        if self.camera_controls_enabled && input.key_pressed(KeyCode::KeyR) {
            self.reset_adjustments();
            true
        } else if self.camera_controls_enabled {
            self.apply_keyboard_controls(dt, input, aabb, camera_settings, viewport_aspect)
        } else {
            false
        }
    }

    fn apply_keyboard_controls(
        &mut self,
        dt: f32,
        input: &Input,
        aabb: Option<(Vec3, Vec3)>,
        camera_settings: CameraSettings,
        viewport_aspect: f32,
    ) -> bool {
        let repeat_dt = finite_value(dt, 0.0).max(0.0);
        let dt = super::finite_clamped(dt, 0.0, 0.25, 0.0);
        let horizontal_action = horizontal_camera_action(input);
        let (snap_direction, snap_steps) = requested_horizontal_snap_steps(
            &mut self.horizontal_snap_repeat,
            input,
            horizontal_action,
            repeat_dt,
        );
        let action = vertical_camera_action(input);
        let (pitch_snap_direction, pitch_snap_steps) =
            requested_vertical_snap_steps(&mut self.vertical_snap_repeat, input, action, repeat_dt);
        if dt == 0.0 && snap_steps == 0 && pitch_snap_steps == 0 {
            return false;
        }

        let previous_adjustments = self.camera_adjustments;
        let mut adjustments = self.camera_adjustments;
        let requested_pitch_delta = if action == VerticalCameraAction::Pitch {
            axis(input, KeyCode::ArrowUp, KeyCode::ArrowDown) * CAMERA_PITCH_RATE_DEG_PER_SEC * dt
        } else {
            0.0
        };
        let requested_pan = requested_pan_witness_delta(input, dt, horizontal_action, action);
        adjustments.fov_delta_deg += fov_axis(input) * CAMERA_FOV_RATE_DEG_PER_SEC * dt;
        match horizontal_action {
            HorizontalCameraAction::SnapRoll => {
                adjustments.roll_deg =
                    apply_roll_snap_steps(adjustments.roll_deg, snap_direction, snap_steps);
            }
            HorizontalCameraAction::SnapYaw => {
                adjustments.yaw_deg =
                    apply_yaw_snap_steps(adjustments.yaw_deg, snap_direction, snap_steps);
            }
            HorizontalCameraAction::Yaw => {
                adjustments.yaw_deg += axis(input, KeyCode::ArrowLeft, KeyCode::ArrowRight)
                    * CAMERA_YAW_RATE_DEG_PER_SEC
                    * dt;
            }
            HorizontalCameraAction::Pan => {
                // Applied after all orientation/reframing deltas so the
                // witness is evaluated in the camera produced by this input.
            }
            HorizontalCameraAction::Roll => {
                adjustments.roll_deg += axis(input, KeyCode::ArrowRight, KeyCode::ArrowLeft)
                    * CAMERA_ROLL_RATE_DEG_PER_SEC
                    * dt;
            }
        }
        match action {
            VerticalCameraAction::Zoom => {
                // Up is zoom in (closer), down is zoom out (farther).
                adjustments.distance_scale_delta +=
                    axis(input, KeyCode::ArrowDown, KeyCode::ArrowUp)
                        * CAMERA_DISTANCE_RATE_PER_SEC
                        * dt;
            }
            VerticalCameraAction::Pan => {
                // Applied below with the horizontal pan transition.
            }
            VerticalCameraAction::Pitch => {
                adjustments.pitch_deg += requested_pitch_delta;
            }
            VerticalCameraAction::SnapPitch => {
                adjustments.pitch_deg = apply_pitch_snap_steps(
                    adjustments.pitch_deg,
                    pitch_snap_direction,
                    pitch_snap_steps,
                );
            }
        }

        if requested_pan != Vec2::ZERO {
            if let Some(aabb) = aabb {
                adjustments.pan_ndc = admit_pan_input(
                    aabb,
                    camera_settings,
                    adjustments,
                    viewport_aspect,
                    requested_pan,
                );
            }
        }

        if adjustments != previous_adjustments {
            self.camera_adjustments = adjustments.sanitized();
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
impl CameraControls {
    pub(in crate::widget) fn roll_snap_repeat_is_active(&self) -> bool {
        self.horizontal_snap_repeat.active_direction != 0
    }

    fn set_camera_adjustments(&mut self, adjustments: CameraRuntimeAdjustments) {
        self.set_adjustments(adjustments);
    }

    fn apply_camera_keyboard_controls(&mut self, dt: f32, input: &Input) {
        self.apply_keyboard_controls(
            dt,
            input,
            None,
            CameraSettings::default(),
            super::DEFAULT_VIEWPORT_ASPECT,
        );
    }
}

#[cfg(test)]
mod tests;
