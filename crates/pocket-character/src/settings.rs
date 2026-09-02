//! Persistent, character-owned Pocket Character settings.
//!
//! The JSON representation is intentionally small and UI-independent. The
//! stored MSAA value is a requested preference and is serialized as one of
//! the strings `"off"`, `"2x"`, `"4x"`, or `"8x"`. Numeric values `1`,
//! `2`, `4`, and `8` are accepted when loading for convenience, with `1`
//! meaning `"off"`; values outside that set fall back to `Off`.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;

use anyhow::{Context, Result};
use directories::BaseDirs;
use serde::de::Deserializer;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const CURRENT_SCHEMA_VERSION: u32 = 1;

const DEFAULT_WINDOW_WIDTH: u32 = 450;
const DEFAULT_WINDOW_HEIGHT: u32 = 600;
const DEFAULT_FOV_DEG: f32 = 40.0;
const DEFAULT_DISTANCE_SCALE: f32 = 0.6;
const DEFAULT_HEADROOM: f32 = 0.05;
const DEFAULT_SNAP_DEG: f32 = 15.0;
const DEFAULT_MAX_FPS: f32 = 60.0;

const MIN_WINDOW_WIDTH: u32 = 160;
const MAX_WINDOW_WIDTH: u32 = 7680;
const MIN_WINDOW_HEIGHT: u32 = 160;
const MAX_WINDOW_HEIGHT: u32 = 4320;
const MIN_MAX_FPS: f32 = 1.0;
const MAX_MAX_FPS: f32 = 240.0;
const MIN_SNAP_DEG: f32 = 0.1;
const MAX_SNAP_DEG: f32 = 90.0;

#[allow(dead_code)]
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum AntiAliasingPreference {
    #[serde(rename = "off")]
    Off,
    #[serde(rename = "2x")]
    X2,
    #[serde(rename = "4x")]
    X4,
    #[serde(rename = "8x")]
    X8,
}

impl Default for AntiAliasingPreference {
    fn default() -> Self {
        // This is only the requested interactive preference. Pocket3D may
        // fall down to a lower hardware-effective count, and headless
        // rendering ignores it.
        Self::X4
    }
}

impl AntiAliasingPreference {
    pub fn samples(self) -> Option<u32> {
        match self {
            Self::Off => None,
            Self::X2 => Some(2),
            Self::X4 => Some(4),
            Self::X8 => Some(8),
        }
    }

    pub fn from_samples(samples: u32) -> Option<Self> {
        match samples {
            1 => Some(Self::Off),
            2 => Some(Self::X2),
            4 => Some(Self::X4),
            8 => Some(Self::X8),
            _ => None,
        }
    }

    fn from_json_value(value: &Value) -> Option<Self> {
        if let Some(samples) = value.as_u64() {
            return match samples {
                1 => Some(Self::Off),
                2 => Some(Self::X2),
                4 => Some(Self::X4),
                8 => Some(Self::X8),
                _ => None,
            };
        }

        let text = value.as_str()?.trim().to_ascii_lowercase();
        match text.as_str() {
            "off" | "1" | "1x" => Some(Self::Off),
            "2" | "2x" => Some(Self::X2),
            "4" | "4x" => Some(Self::X4),
            "8" | "8x" => Some(Self::X8),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for AntiAliasingPreference {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        Ok(Self::from_json_value(&value).unwrap_or(Self::Off))
    }
}

/// Character-specific camera framing policy.
///
/// `distance_scale` is relative to the model's AABB height, while `headroom`
/// is the normalized fraction of the vertical viewport below its top edge.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CameraSettings {
    #[serde(default = "default_fov_deg", deserialize_with = "deserialize_fov_deg")]
    pub fov_deg: f32,
    #[serde(
        default = "default_distance_scale",
        deserialize_with = "deserialize_distance_scale"
    )]
    pub distance_scale: f32,
    #[serde(
        default = "default_headroom",
        deserialize_with = "deserialize_headroom"
    )]
    pub headroom: f32,
    #[serde(
        default = "default_snap_deg",
        deserialize_with = "deserialize_snap_deg"
    )]
    pub yaw_snap_deg: f32,
    #[serde(
        default = "default_snap_deg",
        deserialize_with = "deserialize_snap_deg"
    )]
    pub roll_snap_deg: f32,
    #[serde(
        default = "default_snap_deg",
        deserialize_with = "deserialize_snap_deg"
    )]
    pub pitch_snap_deg: f32,
}

impl Default for CameraSettings {
    fn default() -> Self {
        Self {
            fov_deg: DEFAULT_FOV_DEG,
            distance_scale: DEFAULT_DISTANCE_SCALE,
            headroom: DEFAULT_HEADROOM,
            yaw_snap_deg: DEFAULT_SNAP_DEG,
            roll_snap_deg: DEFAULT_SNAP_DEG,
            pitch_snap_deg: DEFAULT_SNAP_DEG,
        }
    }
}

impl CameraSettings {
    /// Keep live and persisted settings in the same safe range used by the
    /// W04.1 camera framing math.
    pub fn sanitized(self) -> Self {
        let defaults = Self::default();
        Self {
            fov_deg: if self.fov_deg.is_finite() {
                self.fov_deg.clamp(1.0, 179.0)
            } else {
                defaults.fov_deg
            },
            distance_scale: if self.distance_scale.is_finite() {
                self.distance_scale.clamp(0.1, 10.0)
            } else {
                defaults.distance_scale
            },
            // More than half a viewport of headroom would put the target
            // above the top of the frame and is not useful framing policy.
            headroom: if self.headroom.is_finite() {
                self.headroom.clamp(0.0, 0.49)
            } else {
                defaults.headroom
            },
            yaw_snap_deg: sanitize_snap_deg(self.yaw_snap_deg, defaults.yaw_snap_deg),
            roll_snap_deg: sanitize_snap_deg(self.roll_snap_deg, defaults.roll_snap_deg),
            pitch_snap_deg: sanitize_snap_deg(self.pitch_snap_deg, defaults.pitch_snap_deg),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowSettings {
    #[serde(
        default = "default_window_width",
        deserialize_with = "deserialize_window_width"
    )]
    pub width: u32,
    #[serde(
        default = "default_window_height",
        deserialize_with = "deserialize_window_height"
    )]
    pub height: u32,
    #[serde(
        default = "default_resizable",
        deserialize_with = "deserialize_resizable"
    )]
    pub resizable: bool,
    #[serde(
        default = "default_always_on_top",
        deserialize_with = "deserialize_always_on_top"
    )]
    pub always_on_top: bool,
}

impl Default for WindowSettings {
    fn default() -> Self {
        Self {
            width: DEFAULT_WINDOW_WIDTH,
            height: DEFAULT_WINDOW_HEIGHT,
            resizable: false,
            always_on_top: true,
        }
    }
}

impl WindowSettings {
    pub(crate) fn sanitized(self) -> Self {
        Self {
            width: self.width.clamp(MIN_WINDOW_WIDTH, MAX_WINDOW_WIDTH),
            height: self.height.clamp(MIN_WINDOW_HEIGHT, MAX_WINDOW_HEIGHT),
            resizable: self.resizable,
            always_on_top: self.always_on_top,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RenderSettings {
    pub msaa: AntiAliasingPreference,
    #[serde(default = "default_max_fps", deserialize_with = "deserialize_max_fps")]
    pub max_fps: f32,
    #[serde(
        default = "default_smaa_enabled",
        deserialize_with = "deserialize_smaa_enabled"
    )]
    pub smaa_enabled: bool,
}

#[derive(Deserialize)]
struct RenderSettingsInput {
    #[serde(default)]
    msaa: Option<Value>,
    #[serde(default)]
    anti_aliasing: Option<Value>,
    #[serde(default = "default_max_fps", deserialize_with = "deserialize_max_fps")]
    max_fps: f32,
    #[serde(
        default = "default_smaa_enabled",
        deserialize_with = "deserialize_smaa_enabled"
    )]
    smaa_enabled: bool,
}

impl<'de> Deserialize<'de> for RenderSettings {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let input = RenderSettingsInput::deserialize(deserializer)?;
        let msaa = match input.msaa.or(input.anti_aliasing) {
            Some(value) => AntiAliasingPreference::from_json_value(&value)
                .unwrap_or(AntiAliasingPreference::Off),
            None => AntiAliasingPreference::default(),
        };

        Ok(Self {
            msaa,
            max_fps: input.max_fps,
            smaa_enabled: input.smaa_enabled,
        })
    }
}

impl Default for RenderSettings {
    fn default() -> Self {
        Self {
            msaa: AntiAliasingPreference::default(),
            max_fps: DEFAULT_MAX_FPS,
            smaa_enabled: false,
        }
    }
}

impl RenderSettings {
    fn sanitized(self) -> Self {
        let max_fps = if self.max_fps.is_finite() {
            self.max_fps.clamp(MIN_MAX_FPS, MAX_MAX_FPS)
        } else {
            DEFAULT_MAX_FPS
        };
        Self {
            msaa: self.msaa,
            max_fps,
            smaa_enabled: self.smaa_enabled,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AppSettings {
    #[serde(
        default = "default_schema_version",
        deserialize_with = "deserialize_schema_version"
    )]
    pub schema_version: u32,
    #[serde(default)]
    pub window: WindowSettings,
    #[serde(default)]
    pub camera: CameraSettings,
    #[serde(default)]
    pub rendering: RenderSettings,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            window: WindowSettings::default(),
            camera: CameraSettings::default(),
            rendering: RenderSettings::default(),
        }
    }
}

impl AppSettings {
    /// Load the platform settings file. A missing, unreadable, corrupt, or
    /// unsupported file never prevents application startup.
    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            log::warn!("unable to resolve the platform config directory; using default settings");
            return Self::default();
        };
        Self::load_from_path(&path)
    }

    /// Load settings from an explicit path. This is also used by tests so no
    /// test needs to read or write the real user configuration directory.
    pub fn load_from_path(path: &Path) -> Self {
        let contents = match fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(error) => {
                log::warn!(
                    "unable to read settings from {}; using defaults: {error}",
                    path.display()
                );
                return Self::default();
            }
        };

        match Self::from_json(&contents) {
            Ok(settings) => settings,
            Err(error) => {
                log::warn!(
                    "invalid settings in {}; using defaults: {error}",
                    path.display()
                );
                Self::default()
            }
        }
    }

    /// Parse, migrate, and sanitize a JSON settings document.
    pub fn from_json(json: &str) -> std::result::Result<Self, String> {
        let settings: Self = serde_json::from_str(json).map_err(|error| error.to_string())?;
        Self::migrate(settings).map(|settings| settings.sanitized())
    }

    /// Return the platform config path, normally `%APPDATA%\\pocket-character\\settings.json`
    /// on Windows.
    pub fn path() -> Option<PathBuf> {
        BaseDirs::new().map(|dirs| {
            dirs.config_dir()
                .join("pocket-character")
                .join("settings.json")
        })
    }

    /// Save to the platform config path using the same atomic writer as tests.
    #[allow(dead_code)]
    pub fn save(&self) -> Result<()> {
        let path = Self::path().context("unable to resolve the platform config directory")?;
        self.save_to_path(&path)
    }

    /// Atomically save settings to an explicit path.
    #[allow(dead_code)]
    pub fn save_to_path(&self, path: &Path) -> Result<()> {
        let settings = self.clone().sanitized();
        let bytes = serde_json::to_vec_pretty(&settings).context("serializing settings")?;

        if let Ok(existing) = fs::read(path) {
            if existing == bytes {
                return Ok(());
            }
        }

        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent)
            .with_context(|| format!("creating settings directory {}", parent.display()))?;

        let counter = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp_path = parent.join(format!(
            ".settings.json.{}.{}.tmp",
            std::process::id(),
            counter
        ));

        let write_result = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temp_path)
                .with_context(|| {
                    format!("creating temporary settings file {}", temp_path.display())
                })?;
            file.write_all(&bytes)
                .context("writing temporary settings file")?;
            file.sync_all()
                .context("flushing temporary settings file")?;
            Ok(())
        })();
        if let Err(error) = write_result {
            let _ = fs::remove_file(&temp_path);
            return Err(error);
        }

        if let Err(error) = commit_temp_file(&temp_path, path) {
            let _ = fs::remove_file(&temp_path);
            return Err(error)
                .with_context(|| format!("atomically replacing settings file {}", path.display()));
        }

        // Directory fsync is useful on Unix. It is not available on every
        // platform/filesystem, so a failure here does not invalidate the save.
        if let Ok(directory) = File::open(parent) {
            let _ = directory.sync_all();
        }
        Ok(())
    }

    pub(crate) fn sanitized(self) -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            window: self.window.sanitized(),
            camera: self.camera.sanitized(),
            rendering: self.rendering.sanitized(),
        }
    }

    fn migrate(settings: Self) -> std::result::Result<Self, String> {
        match settings.schema_version {
            CURRENT_SCHEMA_VERSION => Ok(settings),
            version => Err(format!("unsupported settings schema version {version}")),
        }
    }
}

#[cfg(windows)]
fn replace_existing_file(temp_path: &Path, path: &Path) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{REPLACEFILE_WRITE_THROUGH, ReplaceFileW};

    let path = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let temp_path = temp_path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let replaced = unsafe {
        ReplaceFileW(
            path.as_ptr(),
            temp_path.as_ptr(),
            std::ptr::null(),
            REPLACEFILE_WRITE_THROUGH,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if replaced == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn commit_temp_file(temp_path: &Path, path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        // ReplaceFileW is the Windows atomic replacement primitive. It only
        // accepts an existing destination, so use rename for first creation.
        match replace_existing_file(temp_path, path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match fs::rename(temp_path, path) {
                    Ok(()) => Ok(()),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        replace_existing_file(temp_path, path)
                    }
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        }
    }

    #[cfg(not(windows))]
    {
        fs::rename(temp_path, path)
    }
}

fn default_schema_version() -> u32 {
    CURRENT_SCHEMA_VERSION
}

fn default_window_width() -> u32 {
    DEFAULT_WINDOW_WIDTH
}

fn default_window_height() -> u32 {
    DEFAULT_WINDOW_HEIGHT
}

fn default_resizable() -> bool {
    false
}

fn default_always_on_top() -> bool {
    true
}

fn default_fov_deg() -> f32 {
    DEFAULT_FOV_DEG
}

fn default_distance_scale() -> f32 {
    DEFAULT_DISTANCE_SCALE
}

fn default_headroom() -> f32 {
    DEFAULT_HEADROOM
}

fn default_snap_deg() -> f32 {
    DEFAULT_SNAP_DEG
}

fn default_max_fps() -> f32 {
    DEFAULT_MAX_FPS
}

fn default_smaa_enabled() -> bool {
    false
}

fn deserialize_schema_version<'de, D>(deserializer: D) -> std::result::Result<u32, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    Ok(value
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(0))
}

fn deserialize_u32_or<'de, D>(deserializer: D, fallback: u32) -> std::result::Result<u32, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    Ok(value
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(fallback))
}

fn deserialize_f32_or<'de, D>(deserializer: D, fallback: f32) -> std::result::Result<f32, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    let Some(value) = value.as_f64() else {
        return Ok(fallback);
    };
    let value = value as f32;
    Ok(value.is_finite().then_some(value).unwrap_or(fallback))
}

fn deserialize_bool_or<'de, D>(
    deserializer: D,
    fallback: bool,
) -> std::result::Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    Ok(value.as_bool().unwrap_or(fallback))
}

fn deserialize_window_width<'de, D>(deserializer: D) -> std::result::Result<u32, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_u32_or(deserializer, DEFAULT_WINDOW_WIDTH)
}

fn deserialize_window_height<'de, D>(deserializer: D) -> std::result::Result<u32, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_u32_or(deserializer, DEFAULT_WINDOW_HEIGHT)
}

fn deserialize_resizable<'de, D>(deserializer: D) -> std::result::Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_bool_or(deserializer, false)
}

fn deserialize_always_on_top<'de, D>(deserializer: D) -> std::result::Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_bool_or(deserializer, true)
}

fn deserialize_fov_deg<'de, D>(deserializer: D) -> std::result::Result<f32, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_f32_or(deserializer, DEFAULT_FOV_DEG)
}

fn deserialize_distance_scale<'de, D>(deserializer: D) -> std::result::Result<f32, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_f32_or(deserializer, DEFAULT_DISTANCE_SCALE)
}

fn deserialize_headroom<'de, D>(deserializer: D) -> std::result::Result<f32, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_f32_or(deserializer, DEFAULT_HEADROOM)
}

fn deserialize_snap_deg<'de, D>(deserializer: D) -> std::result::Result<f32, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_f32_or(deserializer, DEFAULT_SNAP_DEG)
}

fn sanitize_snap_deg(value: f32, default: f32) -> f32 {
    if value.is_finite() {
        value.clamp(MIN_SNAP_DEG, MAX_SNAP_DEG)
    } else {
        default
    }
}

fn deserialize_max_fps<'de, D>(deserializer: D) -> std::result::Result<f32, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_f32_or(deserializer, DEFAULT_MAX_FPS)
}

fn deserialize_smaa_enabled<'de, D>(deserializer: D) -> std::result::Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_bool_or(deserializer, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn defaults_match_current_product_policy() {
        let settings = AppSettings::default();

        assert_eq!(settings.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(settings.window.width, 450);
        assert_eq!(settings.window.height, 600);
        assert!(!settings.window.resizable);
        assert!(settings.window.always_on_top);
        assert_eq!(settings.camera, CameraSettings::default());
        assert_eq!(settings.camera.yaw_snap_deg, 15.0);
        assert_eq!(settings.camera.roll_snap_deg, 15.0);
        assert_eq!(settings.camera.pitch_snap_deg, 15.0);
        assert_eq!(settings.rendering.msaa, AntiAliasingPreference::X4);
        assert_eq!(settings.rendering.max_fps, 60.0);
        assert!(!settings.rendering.smaa_enabled);
    }

    #[test]
    fn round_trip_serialization_uses_canonical_aa_strings() {
        let settings = AppSettings {
            window: WindowSettings {
                width: 720,
                height: 480,
                resizable: true,
                always_on_top: false,
            },
            camera: CameraSettings {
                fov_deg: 35.0,
                distance_scale: 0.75,
                headroom: 0.08,
                yaw_snap_deg: 7.5,
                roll_snap_deg: 22.5,
                pitch_snap_deg: 60.0,
            },
            rendering: RenderSettings {
                msaa: AntiAliasingPreference::X8,
                max_fps: 144.0,
                smaa_enabled: true,
            },
            ..AppSettings::default()
        };

        let json = serde_json::to_string(&settings).unwrap();
        assert!(json.contains(r#""msaa":"8x""#));
        assert!(!json.contains("anti_aliasing"));
        assert!(json.contains(r#""smaa_enabled":true"#));
        assert_eq!(AppSettings::from_json(&json).unwrap(), settings);
    }

    #[test]
    fn missing_fields_use_defaults() {
        let settings =
            AppSettings::from_json(r#"{"schema_version":1,"window":{"width":720}}"#).unwrap();

        assert_eq!(settings.window.width, 720);
        assert_eq!(settings.window.height, 600);
        assert_eq!(settings.window.resizable, false);
        assert_eq!(settings.camera, CameraSettings::default());
        assert_eq!(settings.rendering, RenderSettings::default());
    }

    #[test]
    fn old_camera_settings_json_defaults_missing_snap_steps() {
        let settings = AppSettings::from_json(
            r#"{
                "window":{"width":720,"height":480,"resizable":true,"always_on_top":false},
                "camera":{"fov_deg":35.0,"distance_scale":0.75,"headroom":0.08},
                "rendering":{"msaa":"8x","max_fps":144.0,"smaa_enabled":true}
            }"#,
        )
        .unwrap();

        assert_eq!(
            settings.camera,
            CameraSettings {
                fov_deg: 35.0,
                distance_scale: 0.75,
                headroom: 0.08,
                ..CameraSettings::default()
            }
        );
        assert_eq!(settings.window.width, 720);
        assert_eq!(settings.window.height, 480);
        assert!(settings.window.resizable);
        assert!(!settings.window.always_on_top);
        assert_eq!(settings.rendering.msaa, AntiAliasingPreference::X8);
        assert_eq!(settings.rendering.max_fps, 144.0);
        assert!(settings.rendering.smaa_enabled);
    }

    #[test]
    fn camera_snap_steps_sanitize_each_field_independently() {
        let sanitized = AppSettings::from_json(
            r#"{
                "camera":{"yaw_snap_deg":0.0,"roll_snap_deg":42.0,"pitch_snap_deg":120.0}
            }"#,
        )
        .unwrap()
        .camera;

        assert_eq!(sanitized.yaw_snap_deg, MIN_SNAP_DEG);
        assert_eq!(sanitized.roll_snap_deg, 42.0);
        assert_eq!(sanitized.pitch_snap_deg, MAX_SNAP_DEG);

        let sanitized = CameraSettings {
            yaw_snap_deg: f32::INFINITY,
            roll_snap_deg: 42.0,
            pitch_snap_deg: -f32::INFINITY,
            ..CameraSettings::default()
        }
        .sanitized();

        assert_eq!(sanitized.yaw_snap_deg, 15.0);
        assert_eq!(sanitized.roll_snap_deg, 42.0);
        assert_eq!(sanitized.pitch_snap_deg, 15.0);
    }

    #[test]
    fn partial_aa_settings_default_only_missing_aa_field() {
        let msaa_only =
            AppSettings::from_json(r#"{"schema_version":1,"rendering":{"anti_aliasing":"8x"}}"#)
                .unwrap();
        assert_eq!(msaa_only.rendering.msaa, AntiAliasingPreference::X8);
        assert!(!msaa_only.rendering.smaa_enabled);

        let smaa_only =
            AppSettings::from_json(r#"{"schema_version":1,"rendering":{"smaa_enabled":true}}"#)
                .unwrap();
        assert_eq!(smaa_only.rendering.msaa, AntiAliasingPreference::default());
        assert!(smaa_only.rendering.smaa_enabled);
    }

    #[test]
    fn canonical_msaa_wins_over_legacy_anti_aliasing() {
        let settings =
            AppSettings::from_json(r#"{"rendering":{"msaa":"2x","anti_aliasing":"8x"}}"#).unwrap();

        assert_eq!(settings.rendering.msaa, AntiAliasingPreference::X2);
    }

    #[test]
    fn corrupt_json_loads_defaults() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, b"{ definitely not json").unwrap();

        assert_eq!(AppSettings::load_from_path(&path), AppSettings::default());
    }

    #[test]
    fn missing_file_loads_defaults() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("missing-settings.json");

        assert_eq!(AppSettings::load_from_path(&path), AppSettings::default());
    }

    #[test]
    fn invalid_numeric_values_are_sanitized_without_aborting_load() {
        let settings = AppSettings::from_json(
            r#"{
                "window":{"width":1,"height":99999},
                "camera":{"fov_deg":null,"distance_scale":-1,"headroom":2},
                "rendering":{"max_fps":0}
            }"#,
        )
        .unwrap();

        assert_eq!(settings.window.width, MIN_WINDOW_WIDTH);
        assert_eq!(settings.window.height, MAX_WINDOW_HEIGHT);
        assert_eq!(settings.camera.fov_deg, DEFAULT_FOV_DEG);
        assert_eq!(settings.camera.distance_scale, 0.1);
        assert_eq!(settings.camera.headroom, 0.49);
        assert_eq!(settings.rendering.max_fps, MIN_MAX_FPS);
    }

    #[test]
    fn invalid_aa_preference_falls_back_but_valid_aliases_are_accepted() {
        let invalid = AppSettings::from_json(r#"{"rendering":{"msaa":"32x"}}"#).unwrap();
        assert_eq!(invalid.rendering.msaa, AntiAliasingPreference::Off);

        let numeric = AppSettings::from_json(r#"{"rendering":{"anti_aliasing":4}}"#).unwrap();
        assert_eq!(numeric.rendering.msaa, AntiAliasingPreference::X4);

        let off = AppSettings::from_json(r#"{"rendering":{"anti_aliasing":1}}"#).unwrap();
        assert_eq!(off.rendering.msaa, AntiAliasingPreference::Off);

        let off_canonical = AppSettings::from_json(r#"{"rendering":{"msaa":"off"}}"#).unwrap();
        assert_eq!(off_canonical.rendering.msaa.samples().unwrap_or(1), 1);

        let canonical_numeric = AppSettings::from_json(r#"{"rendering":{"msaa":4}}"#).unwrap();
        assert_eq!(canonical_numeric.rendering.msaa, AntiAliasingPreference::X4);

        let invalid_smaa =
            AppSettings::from_json(r#"{"rendering":{"smaa_enabled":"enabled"}}"#).unwrap();
        assert!(!invalid_smaa.rendering.smaa_enabled);
    }

    #[test]
    fn requested_msaa_stays_8x_when_capability_negotiation_falls_back_to_4x() {
        let settings = AppSettings {
            rendering: RenderSettings {
                msaa: AntiAliasingPreference::X8,
                ..RenderSettings::default()
            },
            ..AppSettings::default()
        };
        let effective = pocket3d::renderer::select_effective_sample_count(
            settings.rendering.msaa.samples().unwrap_or(1),
            &[1, 2, 4],
        );

        assert_eq!(effective, 4);
        assert_eq!(settings.rendering.msaa, AntiAliasingPreference::X8);
        let reloaded = AppSettings::from_json(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!(reloaded.rendering.msaa, AntiAliasingPreference::X8);
    }

    #[test]
    fn unsupported_schema_version_is_rejected_and_load_falls_back() {
        assert!(AppSettings::from_json(r#"{"schema_version":2}"#).is_err());

        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, br#"{"schema_version":2,"window":{"width":900}}"#).unwrap();
        assert_eq!(AppSettings::load_from_path(&path), AppSettings::default());
    }

    #[test]
    fn atomic_replace_existing_settings_uses_temp_directory() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested").join("settings.json");
        let settings_a = AppSettings {
            window: WindowSettings {
                width: 900,
                height: 700,
                ..WindowSettings::default()
            },
            rendering: RenderSettings {
                msaa: AntiAliasingPreference::X2,
                max_fps: 30.0,
                smaa_enabled: false,
            },
            ..AppSettings::default()
        };
        let settings_b = AppSettings {
            rendering: RenderSettings {
                max_fps: 30.0,
                ..settings_a.rendering.clone()
            },
            ..settings_a.clone()
        };

        settings_a.save_to_path(&path).unwrap();
        settings_b.save_to_path(&path).unwrap();
        assert_eq!(AppSettings::load_from_path(&path), settings_b);

        let temp_prefix = format!(".settings.json.{}.", std::process::id());
        let temporary_files: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| {
                let name = name.to_string_lossy();
                name.starts_with(&temp_prefix) && name.ends_with(".tmp")
            })
            .collect();
        assert!(
            temporary_files.is_empty(),
            "temporary settings files remain: {temporary_files:?}"
        );
    }
}
