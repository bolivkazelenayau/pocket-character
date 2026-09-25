//! Application-private controls contract for the PocketUI menu.
//!
//! This is an adapter boundary, not a framework: the guest stays dumb by
//! sending `ControlAction`s and rendering `ControlsSnapshot`s. It never sees
//! runtime adjustment structs, the dynamic pan solver, projected bounds,
//! `PAN_*` internals, or renderer internals. Snapshots expose user-facing
//! requested, pending, and effective state only.
//!
//! Link (locking yaw/pitch/roll snap sliders together) is future UI state
//! only and is deliberately absent from persisted camera settings. The atomic
//! `SetAllSnaps` variant lets a linked edit land as one settings update/save
//! instead of three transient ones.

use crate::settings::{AntiAliasingPreference, MtoonRenderMode};

/// One authoritative settings/action boundary for the controls menu.
///
/// FOV/distance step actions mutate the live/session optic deltas using the
/// same policy as keyboard controls. `SaveCamera` commits the representable
/// live optics to persisted camera settings and clears only those deltas.
/// Pan/yaw/pitch/roll remain session-only because `CameraSettings` has no
/// corresponding runtime pose fields. `ResetRuntimeCamera` returns to saved
/// base framing with zero runtime pan/yaw/pitch/roll without overwriting
/// persisted camera or rendering settings. The same semantic reset is used by
/// the keyboard `R` shortcut and the PocketUI Reset Camera action.
/// Window actions update persisted `AppSettings` through Rust sanitization and
/// queue the corresponding live `WindowRuntimeRequest` for Pocket3D.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ControlAction {
    AdjustDistance(i8),
    AdjustFov(i8),
    SetEffectiveDistance(f32),
    SetEffectiveFov(f32),
    SetYaw(f32),
    SetPitch(f32),
    SetRoll(f32),
    SetHeadroom(f32),
    SaveCamera,
    ResetRuntimeCamera,
    SetYawSnap(f32),
    SetPitchSnap(f32),
    SetRollSnap(f32),
    SetAllSnaps {
        yaw_deg: f32,
        pitch_deg: f32,
        roll_deg: f32,
    },
    SetWindowWidth(f32),
    SetWindowHeight(f32),
    SetWindowResizable(bool),
    SetWindowAlwaysOnTop(bool),
    SetMaxFps(f32),
    SettingsOpened,
    SettingsClosed,
    RequestMsaa(AntiAliasingPreference),
    RequestSmaa(bool),
    SetMtoonRenderMode(MtoonRenderMode),
    SetLookAt(bool),
    SetAutoBlink(bool),
    RestoreDefaults,
}

/// Immutable user-facing requested/pending/effective state for the menu.
///
/// Requested vs. effective AA are kept independent: requested MSAA can exceed
/// hardware-effective MSAA, and requested SMAA can differ from the
/// renderer-observed value while a request is pending. Pending flags expose
/// the between-frame application window without leaking renderer internals.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ControlsSnapshot {
    base_fov_deg: f32,
    base_distance_scale: f32,
    headroom: f32,
    yaw_deg: f32,
    pitch_deg: f32,
    roll_deg: f32,
    yaw_snap_deg: f32,
    pitch_snap_deg: f32,
    roll_snap_deg: f32,
    effective_fov_deg: f32,
    effective_distance_scale: f32,
    requested_msaa: AntiAliasingPreference,
    effective_msaa: u32,
    requested_smaa: bool,
    effective_smaa: bool,
    msaa_pending: bool,
    smaa_pending: bool,
}

impl ControlsSnapshot {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        base_fov_deg: f32,
        base_distance_scale: f32,
        headroom: f32,
        yaw_deg: f32,
        pitch_deg: f32,
        roll_deg: f32,
        yaw_snap_deg: f32,
        pitch_snap_deg: f32,
        roll_snap_deg: f32,
        effective_fov_deg: f32,
        effective_distance_scale: f32,
        requested_msaa: AntiAliasingPreference,
        effective_msaa: u32,
        requested_smaa: bool,
        effective_smaa: bool,
        msaa_pending: bool,
        smaa_pending: bool,
    ) -> Self {
        Self {
            base_fov_deg,
            base_distance_scale,
            headroom,
            yaw_deg,
            pitch_deg,
            roll_deg,
            yaw_snap_deg,
            pitch_snap_deg,
            roll_snap_deg,
            effective_fov_deg,
            effective_distance_scale,
            requested_msaa,
            effective_msaa,
            requested_smaa,
            effective_smaa,
            msaa_pending,
            smaa_pending,
        }
    }

    pub(crate) fn base_fov_deg(self) -> f32 {
        self.base_fov_deg
    }

    pub(crate) fn base_distance_scale(self) -> f32 {
        self.base_distance_scale
    }

    pub(crate) fn headroom(self) -> f32 {
        self.headroom
    }

    pub(crate) fn yaw_deg(self) -> f32 {
        self.yaw_deg
    }

    pub(crate) fn pitch_deg(self) -> f32 {
        self.pitch_deg
    }

    pub(crate) fn roll_deg(self) -> f32 {
        self.roll_deg
    }

    pub(crate) fn yaw_snap_deg(self) -> f32 {
        self.yaw_snap_deg
    }

    pub(crate) fn pitch_snap_deg(self) -> f32 {
        self.pitch_snap_deg
    }

    pub(crate) fn roll_snap_deg(self) -> f32 {
        self.roll_snap_deg
    }

    pub(crate) fn effective_fov_deg(self) -> f32 {
        self.effective_fov_deg
    }

    pub(crate) fn effective_distance_scale(self) -> f32 {
        self.effective_distance_scale
    }

    pub(crate) fn requested_msaa(self) -> AntiAliasingPreference {
        self.requested_msaa
    }

    pub(crate) fn effective_msaa(self) -> u32 {
        self.effective_msaa
    }

    pub(crate) fn requested_smaa(self) -> bool {
        self.requested_smaa
    }

    pub(crate) fn effective_smaa(self) -> bool {
        self.effective_smaa
    }

    pub(crate) fn msaa_pending(self) -> bool {
        self.msaa_pending
    }

    pub(crate) fn smaa_pending(self) -> bool {
        self.smaa_pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_exposes_requested_and_effective_aa_independently() {
        let snapshot = ControlsSnapshot::new(
            40.0,
            0.6,
            0.05,
            0.0,
            0.0,
            0.0,
            15.0,
            15.0,
            15.0,
            40.0,
            0.6,
            AntiAliasingPreference::X8,
            4,
            true,
            false,
            true,
            true,
        );

        assert_eq!(snapshot.requested_msaa(), AntiAliasingPreference::X8);
        assert_eq!(snapshot.effective_msaa(), 4);
        assert!(snapshot.requested_smaa());
        assert!(!snapshot.effective_smaa());
        assert!(snapshot.msaa_pending());
        assert!(snapshot.smaa_pending());
    }
}
