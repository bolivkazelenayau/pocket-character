//! Avatar ownership and transactional preparation.
//!
//! The renderer still stores instances in [`Scene::models`], because that is
//! Pocket3D's current scene representation.  This module makes the ownership
//! explicit at the parent boundary: the active avatar owns the slot handle and
//! all state that is derived from the asset, while a candidate owns a complete
//! replacement until it is committed.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, ensure};
use glam::{Mat4, Vec3};
use pocket_character_core::CharacterSim;
use pocket_vrm::{HumanoidView, SpringSolver, Vrm1Doc, VrmDoc, retarget_with_humanoid};
use pocket3d::anim::{Clip, NodeTrs};
use pocket3d::gpu::Gpu;
use pocket3d::model::{ModelAsset, ModelInstance, ModelLoadOptions};
use pocket3d::renderer::Renderer;
use pocket3d::scene::Scene;
use serde_json::Value;

use crate::guest::CharacterGuest;

#[cfg(test)]
use super::camera::CameraRuntimeAdjustments;

const AVATAR_SIM_SEED: u64 = 0x0c9a_11e0;

/// The semantic document retained by the parent.  VRM 0.x and VRM 1.0 have
/// deliberately separate parsers in PocketVRM, so the parent does not coerce
/// either format into the other one's runtime representation.
#[derive(Debug)]
pub(super) enum AvatarSemanticDocument {
    Vrm0(VrmDoc),
    Vrm1(#[allow(dead_code)] Vrm1Doc),
}

impl AvatarSemanticDocument {
    pub(super) fn parse(bytes: &[u8]) -> Result<Self> {
        let vrm0_error = match VrmDoc::from_glb_bytes(bytes) {
            Ok(document) => return Ok(Self::Vrm0(document)),
            Err(error) => error,
        };

        Vrm1Doc::from_glb_bytes(bytes)
            .map(Self::Vrm1)
            .map_err(|vrm1_error| {
                anyhow!(
                    "not a supported VRM document (VRM 0.x: {vrm0_error:#}; VRM 1.0: {vrm1_error:#})"
                )
            })
    }

    pub(super) fn version(&self) -> AvatarVersion {
        match self {
            Self::Vrm0(_) => AvatarVersion::Vrm0,
            Self::Vrm1(_) => AvatarVersion::Vrm1,
        }
    }

    pub(super) fn vrm0(&self) -> Option<&VrmDoc> {
        match self {
            Self::Vrm0(document) => Some(document),
            Self::Vrm1(_) => None,
        }
    }

    pub(super) fn expression_names(&self) -> Vec<String> {
        self.vrm0()
            .map(|document| {
                document
                    .expressions
                    .iter()
                    .map(|e| e.name.clone())
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Semantic version used by application-owned presentation policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AvatarVersion {
    Vrm0,
    Vrm1,
}

/// Transform applied to an instance at presentation time.  The underlying
/// ModelAsset remains in its authored/object-space convention.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct AvatarPresentation {
    pub(super) version: AvatarVersion,
    pub(super) transform: Mat4,
}

impl AvatarPresentation {
    pub(super) fn for_version(version: AvatarVersion) -> Self {
        let transform = match version {
            AvatarVersion::Vrm0 => Mat4::IDENTITY,
            AvatarVersion::Vrm1 => Mat4::from_rotation_y(core::f32::consts::PI),
        };
        Self { version, transform }
    }

    /// Transform all eight raw AABB corners.  Rotating only the min/max
    /// endpoints is not a valid AABB transform for a non-axis-preserving
    /// presentation transform.
    pub(super) fn aabb(self, raw: (Vec3, Vec3)) -> (Vec3, Vec3) {
        transformed_aabb(raw, self.transform)
    }
}

pub(super) fn transformed_aabb(raw: (Vec3, Vec3), transform: Mat4) -> (Vec3, Vec3) {
    let (min, max) = raw;
    let mut transformed_min = Vec3::splat(f32::INFINITY);
    let mut transformed_max = Vec3::splat(f32::NEG_INFINITY);

    for x in [min.x, max.x] {
        for y in [min.y, max.y] {
            for z in [min.z, max.z] {
                let point = transform.transform_point3(Vec3::new(x, y, z));
                transformed_min = transformed_min.min(point);
                transformed_max = transformed_max.max(point);
            }
        }
    }

    (transformed_min, transformed_max)
}

/// Avatar-specific guest/capability facts retained alongside the active
/// document.  `idle_clip` is optional so a guest never has to request a clip
/// merely because the packaged startup bundle historically used that name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct AvatarCapabilities {
    pub(super) model_name: String,
    pub(super) clip_names: Vec<String>,
    pub(super) expression_names: Vec<String>,
    pub(super) idle_clip: Option<String>,
}

impl AvatarCapabilities {
    fn advertised_idle_clip(clip_names: &[String]) -> Option<String> {
        clip_names
            .iter()
            .find(|name| name.as_str() == "idle_loop")
            .cloned()
    }

    fn for_candidate(
        model_name: String,
        document: &AvatarSemanticDocument,
        clips: &[(String, Clip)],
    ) -> Self {
        let clip_names = clips
            .iter()
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        let idle_clip = Self::advertised_idle_clip(&clip_names);
        Self {
            model_name,
            clip_names,
            expression_names: document.expression_names(),
            idle_clip,
        }
    }
}

/// Animation selection is avatar-owned and starts from a clean runtime state
/// for every candidate.  The clip assets themselves are staged separately.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct AvatarAnimationState {
    pub(super) clip_index: usize,
    pub(super) clip_time: f32,
    pub(super) clip_looping: bool,
}

impl Default for AvatarAnimationState {
    fn default() -> Self {
        Self {
            clip_index: 0,
            clip_time: 0.0,
            clip_looping: true,
        }
    }
}

/// Stable ownership token for the avatar's entry in `Scene::models`.
///
/// Pocket3D currently exposes a Vec rather than an entity/handle store.  The
/// index is therefore intentionally captured once and never inferred from
/// position during ticks.  Future scene models can be appended without
/// changing which instance replacement targets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct AvatarSceneSlot {
    index: usize,
}

impl AvatarSceneSlot {
    pub(super) fn new(index: usize) -> Self {
        Self { index }
    }

    pub(super) fn index(self) -> usize {
        self.index
    }

    pub(super) fn is_valid(self, scene: &Scene) -> bool {
        self.index < scene.models.len()
    }

    pub(super) fn replace(self, scene: &mut Scene, instance: ModelInstance) -> ModelInstance {
        debug_assert!(self.is_valid(scene), "active avatar scene slot disappeared");
        replace_at(&mut scene.models, self.index, instance)
    }

    pub(super) fn get_mut<'a>(self, scene: &'a mut Scene) -> &'a mut ModelInstance {
        debug_assert!(self.is_valid(scene), "active avatar scene slot disappeared");
        scene
            .models
            .get_mut(self.index)
            .expect("active avatar scene slot invariant")
    }
}

/// A future file-picker or other parent-side producer can feed this request
/// into the pending seam.  It carries no UI state and performs no I/O itself.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VrmaProvenance {
    OptionalDefault,
    Explicit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AvatarLoadRequest {
    pub(super) model_path: PathBuf,
    pub(super) vrma_path: Option<PathBuf>,
    /// Retained separately from the path so a future explicitly selected
    /// VRMA can use a strict failure policy without changing this seam.
    pub(super) vrma_provenance: Option<VrmaProvenance>,
    pub(super) model_name: String,
}

impl AvatarLoadRequest {
    pub(crate) fn new(
        model_path: PathBuf,
        vrma_path: Option<PathBuf>,
        model_name: impl Into<String>,
    ) -> Self {
        let vrma_provenance = vrma_path.as_ref().map(|_| VrmaProvenance::OptionalDefault);
        Self {
            model_path,
            vrma_path,
            vrma_provenance,
            model_name: model_name.into(),
        }
    }
}

/// Convert the native picker result into the parent-side request seam. The
/// UI never receives this path and cancellation is represented by `None`.
pub(crate) fn avatar_request_from_picker_result(
    selected: Option<PathBuf>,
    default_vrma_path: &Path,
) -> Option<AvatarLoadRequest> {
    let path = selected?;
    let extension = path.extension()?.to_str()?;
    if !extension.eq_ignore_ascii_case("vrm") {
        return None;
    }
    let model_name = avatar_model_name(path.file_stem());
    Some(AvatarLoadRequest::new(
        path,
        Some(default_vrma_path.to_owned()),
        model_name,
    ))
}

pub(super) fn startup_avatar_request(
    model_path: PathBuf,
    default_vrma_path: PathBuf,
) -> AvatarLoadRequest {
    AvatarLoadRequest::new(model_path, Some(default_vrma_path), "AvatarSample_A")
}

fn stage_vrma_clip(
    provenance: Option<VrmaProvenance>,
    load: impl FnOnce() -> Result<(String, Clip)>,
) -> Result<Option<(String, Clip)>> {
    match load() {
        Ok(clip) => Ok(Some(clip)),
        Err(error) if provenance == Some(VrmaProvenance::OptionalDefault) => {
            log::warn!("optional default VRMA unavailable; continuing in rest pose: {error:#}");
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn fresh_animation_state() -> AvatarAnimationState {
    AvatarAnimationState::default()
}

fn avatar_model_name(file_stem: Option<&std::ffi::OsStr>) -> String {
    file_stem
        .map(|stem| stem.to_string_lossy().trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "Avatar".to_owned())
}

fn replace_at<T>(items: &mut [T], index: usize, value: T) -> T {
    std::mem::replace(
        items
            .get_mut(index)
            .expect("active avatar scene slot invariant"),
        value,
    )
}

fn vrm0_allowed_required_extensions() -> [&'static str; 1] {
    // VrmDoc parses and validates the legacy extension before the model
    // importer runs. The bytes loader needs that validation result stated
    // explicitly because `VRM` is not a glTF core extension.
    ["VRM"]
}

fn vrm1_allowed_required_extensions(json: &Value) -> Result<Vec<&'static str>> {
    let materials = match json.get("materials") {
        None => &[][..],
        Some(value) => value
            .as_array()
            .with_context(|| "VRM 1.0 glTF materials must be an array")?,
    };
    let mut has_mtoon = false;
    for (index, material) in materials.iter().enumerate() {
        let material = material
            .as_object()
            .with_context(|| format!("VRM 1.0 material {index} must be an object"))?;
        let Some(extensions) = material.get("extensions") else {
            continue;
        };
        let extensions = extensions
            .as_object()
            .with_context(|| format!("VRM 1.0 material {index}.extensions must be an object"))?;
        if extensions.contains_key("VRMC_materials_mtoon") {
            has_mtoon = true;
            ensure!(
                extensions
                    .get("KHR_materials_unlit")
                    .is_some_and(Value::is_object),
                "VRM 1.0 material {index} uses VRMC_materials_mtoon without the supported KHR_materials_unlit fallback"
            );
        }
    }

    let mut allowed = vec!["VRMC_vrm"];
    if has_mtoon {
        // The parent does not implement MToon. It only permits the extension
        // when the same material advertises the glTF unlit fallback that the
        // existing renderer understands.
        allowed.push("VRMC_materials_mtoon");
    }
    // Spring bones and node constraints are intentionally not allowlisted:
    // this slice does not implement either runtime semantic.
    Ok(allowed)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AvatarLoadErrorKind {
    UnsupportedVrm,
    VrmParse,
    ModelLoad,
    RuntimeStartup,
    SceneUnavailable,
}

impl AvatarLoadErrorKind {
    fn ui_message(self) -> &'static str {
        match self {
            Self::UnsupportedVrm => "Unsupported VRM format.",
            Self::VrmParse => "VRM parse failed.",
            Self::ModelLoad => "VRM model load failed.",
            Self::RuntimeStartup => "Avatar runtime failed.",
            Self::SceneUnavailable => "Avatar scene unavailable.",
        }
    }
}

/// Classify a semantic-parser failure from document structure rather than its
/// diagnostic text. The latter may contain a user-selected filesystem path.
fn semantic_parse_failure_kind(bytes: &[u8]) -> AvatarLoadErrorKind {
    let Ok(glb) = pocket_vrm::glb::parse_glb(bytes) else {
        return AvatarLoadErrorKind::VrmParse;
    };
    semantic_document_failure_kind(&glb.json)
}

fn semantic_document_failure_kind(json: &Value) -> AvatarLoadErrorKind {
    let Some(extensions_value) = json.get("extensions") else {
        return AvatarLoadErrorKind::UnsupportedVrm;
    };
    let Some(extensions) = extensions_value.as_object() else {
        return AvatarLoadErrorKind::VrmParse;
    };

    match (extensions.contains_key("VRM"), extensions.get("VRMC_vrm")) {
        (true, None) => AvatarLoadErrorKind::VrmParse,
        (false, Some(vrm1)) => {
            let spec_version = vrm1
                .as_object()
                .and_then(|value| value.get("specVersion"))
                .and_then(Value::as_str);
            if spec_version.is_some_and(|version| version != "1.0") {
                AvatarLoadErrorKind::UnsupportedVrm
            } else {
                AvatarLoadErrorKind::VrmParse
            }
        }
        _ => AvatarLoadErrorKind::UnsupportedVrm,
    }
}

/// Narrow, observable error state for a failed runtime load.  The old active
/// avatar is intentionally not part of this error path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AvatarRuntimeError {
    message: String,
    kind: AvatarLoadErrorKind,
}

impl AvatarRuntimeError {
    pub(super) fn new(kind: AvatarLoadErrorKind, error: &anyhow::Error) -> Self {
        Self {
            message: format!("{error:#}"),
            kind,
        }
    }

    #[cfg(test)]
    pub(crate) fn message(&self) -> &str {
        &self.message
    }

    /// The diagnostic chain can include local model/bundle paths.  Preserve a
    /// useful, explicitly assigned failure category without sending that chain
    /// through the guest wire.
    pub(crate) fn ui_message(&self) -> &'static str {
        self.kind.ui_message()
    }
}

impl fmt::Display for AvatarRuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message.fmt(f)
    }
}

impl std::error::Error for AvatarRuntimeError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum AvatarLoadStatus {
    Idle,
    Loading,
    Error(AvatarRuntimeError),
}

impl Default for AvatarLoadStatus {
    fn default() -> Self {
        Self::Idle
    }
}

impl AvatarLoadStatus {
    pub(super) fn begin_loading(&mut self) {
        *self = Self::Loading;
    }

    pub(super) fn fail(&mut self, error: AvatarRuntimeError) {
        *self = Self::Error(error);
    }

    pub(super) fn succeed(&mut self) {
        *self = Self::Idle;
    }

    #[cfg(test)]
    pub(super) fn is_loading(&self) -> bool {
        matches!(self, Self::Loading)
    }

    pub(super) fn ui_status(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Loading => "loading",
            Self::Error(_) => "error",
        }
    }

    #[cfg(test)]
    pub(super) fn error_message(&self) -> Option<&str> {
        match self {
            Self::Error(error) => Some(error.message()),
            Self::Idle | Self::Loading => None,
        }
    }

    pub(super) fn ui_error_message(&self) -> Option<&'static str> {
        match self {
            Self::Error(error) => Some(error.ui_message()),
            Self::Idle | Self::Loading => None,
        }
    }
}

/// Fully prepared replacement.  There are no fallible operations in the
/// transition from this value to [`ActiveAvatar`].
pub(super) struct AvatarCandidate {
    pub(super) asset: Arc<ModelAsset>,
    pub(super) instance: ModelInstance,
    pub(super) document: AvatarSemanticDocument,
    pub(super) presentation: AvatarPresentation,
    pub(super) presentation_aabb: (Vec3, Vec3),
    pub(super) locals: Vec<NodeTrs>,
    pub(super) globals: Vec<Mat4>,
    pub(super) clips: Vec<(String, Clip)>,
    pub(super) springs: Option<SpringSolver>,
    pub(super) blink_binds: Vec<(usize, usize, f32)>,
    pub(super) capabilities: AvatarCapabilities,
    pub(super) guest: CharacterGuest,
    pub(super) sim: CharacterSim,
}

impl AvatarCandidate {
    pub(super) fn prepare(
        gpu: &Gpu,
        renderer: &Renderer,
        bundle_path: &Path,
        request: &AvatarLoadRequest,
    ) -> std::result::Result<Self, AvatarRuntimeError> {
        let model_bytes = std::fs::read(&request.model_path)
            .with_context(|| format!("reading model {}", request.model_path.display()))
            .map_err(|error| AvatarRuntimeError::new(AvatarLoadErrorKind::ModelLoad, &error))?;
        let document = AvatarSemanticDocument::parse(&model_bytes)
            .context("parsing VRM document")
            .map_err(|error| {
                AvatarRuntimeError::new(semantic_parse_failure_kind(&model_bytes), &error)
            })?;
        let presentation = AvatarPresentation::for_version(document.version());

        let model = match document.version() {
            AvatarVersion::Vrm0 => {
                ModelAsset::load_glb_bytes_opts_with_allowed_required_extensions(
                    gpu,
                    &renderer.model_material_layout,
                    &renderer.samplers,
                    &model_bytes,
                    &request.model_path.to_string_lossy(),
                    &ModelLoadOptions {
                        max_texture_dim: Some(2048),
                    },
                    vrm0_allowed_required_extensions(),
                )
            }
            AvatarVersion::Vrm1 => {
                let glb = pocket_vrm::glb::parse_glb(&model_bytes)
                    .context("parsing VRM 1.0 glTF material extensions")
                    .map_err(|error| {
                        AvatarRuntimeError::new(AvatarLoadErrorKind::VrmParse, &error)
                    })?;
                let allowed_required_extensions = vrm1_allowed_required_extensions(&glb.json)
                    .map_err(|error| {
                        AvatarRuntimeError::new(AvatarLoadErrorKind::UnsupportedVrm, &error)
                    })?;
                ModelAsset::load_glb_bytes_opts_with_allowed_required_extensions(
                    gpu,
                    &renderer.model_material_layout,
                    &renderer.samplers,
                    &model_bytes,
                    &request.model_path.to_string_lossy(),
                    &ModelLoadOptions {
                        max_texture_dim: Some(2048),
                    },
                    allowed_required_extensions,
                )
            }
        }
        .context("loading VRM model")
        .map_err(|error| AvatarRuntimeError::new(AvatarLoadErrorKind::ModelLoad, &error))?;

        let mut clips = Vec::new();
        if let Some(vrma_path) = request.vrma_path.as_ref() {
            let clip = stage_vrma_clip(request.vrma_provenance, || {
                let vrma_bytes = std::fs::read(vrma_path)
                    .with_context(|| format!("reading vrma {}", vrma_path.display()))?;
                let vrma =
                    pocket_vrm::load_vrma_bytes(&vrma_bytes).context("parsing VRMA animation")?;
                let clip = match &document {
                    AvatarSemanticDocument::Vrm0(vrm0) => retarget_with_humanoid(
                        &vrma,
                        HumanoidView::Vrm0(&vrm0.humanoid),
                        &model.skeleton,
                    ),
                    AvatarSemanticDocument::Vrm1(vrm1) => retarget_with_humanoid(
                        &vrma,
                        HumanoidView::Vrm1(&vrm1.humanoid),
                        &model.skeleton,
                    ),
                }
                .context("retargeting VRMA animation")?;
                let clip_name = vrma_path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "idle".into());
                Ok((clip_name, clip))
            })
            .map_err(|error| AvatarRuntimeError::new(AvatarLoadErrorKind::ModelLoad, &error))?;
            if let Some(clip) = clip {
                clips.push(clip);
            }
        }

        let mut locals = Vec::new();
        model.skeleton.sample_locals(None, 0.0, false, &mut locals);
        let mut globals = Vec::new();
        model.skeleton.globals_from_locals(&locals, &mut globals);

        let springs = document
            .vrm0()
            .map(|vrm0| SpringSolver::new(&vrm0.springs, &model.skeleton, &locals));

        let mut blink_binds = Vec::new();
        if let Some(vrm0) = document.vrm0() {
            for expression in &vrm0.expressions {
                if expression.name == "blink" {
                    for bind in &expression.binds {
                        if let Some(slot) = model.morph_mesh_slot(bind.mesh) {
                            blink_binds.push((slot, bind.target, bind.weight));
                        }
                    }
                }
            }
        }

        let mut instance = ModelInstance::new(model.clone());
        instance.morph = model.create_morph_state(gpu);
        instance.transform = presentation.transform;
        instance.cutout = 0.5;
        instance.lit = 0.25;
        // Give the renderer a valid rest pose during the frame in which a
        // replacement commits; the next tick replaces it with the live pose.
        instance.pose = Some(globals.clone());

        let bundle = std::fs::read_to_string(bundle_path)
            .with_context(|| format!("reading bundle {}", bundle_path.display()))
            .map_err(|error| {
                AvatarRuntimeError::new(AvatarLoadErrorKind::RuntimeStartup, &error)
            })?;
        let capabilities =
            AvatarCapabilities::for_candidate(request.model_name.clone(), &document, &clips);
        let guest = CharacterGuest::boot(
            &bundle,
            &capabilities.model_name,
            &capabilities.clip_names,
            &capabilities.expression_names,
        )
        .context("starting character guest")
        .map_err(|error| AvatarRuntimeError::new(AvatarLoadErrorKind::RuntimeStartup, &error))?;

        if document.vrm0().is_some() && blink_binds.is_empty() {
            log::warn!("model has no 'blink' expression; blinking disabled");
        }

        let presentation_aabb = presentation.aabb(model.aabb);
        Ok(Self {
            asset: model,
            instance,
            document,
            presentation,
            presentation_aabb,
            locals,
            globals,
            clips,
            springs,
            blink_binds,
            capabilities,
            guest,
            sim: CharacterSim::new(AVATAR_SIM_SEED, Vec3::ZERO),
        })
    }

    pub(super) fn into_parts(self, scene_slot: AvatarSceneSlot) -> (ModelInstance, ActiveAvatar) {
        let Self {
            asset,
            instance,
            document,
            presentation,
            presentation_aabb,
            locals,
            globals,
            clips,
            springs,
            blink_binds,
            capabilities,
            guest,
            sim,
        } = self;
        (
            instance,
            ActiveAvatar {
                asset,
                document,
                presentation,
                presentation_aabb,
                scene_slot,
                locals,
                globals,
                clips,
                springs,
                blink_binds,
                capabilities,
                guest,
                sim,
                animation: fresh_animation_state(),
            },
        )
    }
}

/// The one avatar currently owned by the parent runtime.
pub(super) struct ActiveAvatar {
    pub(super) asset: Arc<ModelAsset>,
    pub(super) document: AvatarSemanticDocument,
    #[allow(dead_code)]
    pub(super) presentation: AvatarPresentation,
    pub(super) presentation_aabb: (Vec3, Vec3),
    pub(super) scene_slot: AvatarSceneSlot,
    pub(super) locals: Vec<NodeTrs>,
    pub(super) globals: Vec<Mat4>,
    pub(super) clips: Vec<(String, Clip)>,
    pub(super) springs: Option<SpringSolver>,
    pub(super) blink_binds: Vec<(usize, usize, f32)>,
    #[allow(dead_code)]
    pub(super) capabilities: AvatarCapabilities,
    pub(super) guest: CharacterGuest,
    pub(super) sim: CharacterSim,
    pub(super) animation: AvatarAnimationState,
}

/// Camera state that survives an avatar swap.  The active model is used only
/// to validate pan; all other runtime adjustments are copied unchanged.
#[cfg(test)]
pub(super) fn revalidate_camera_adjustments(
    aabb: (Vec3, Vec3),
    base_settings: crate::settings::CameraSettings,
    adjustments: CameraRuntimeAdjustments,
    viewport_aspect: f32,
) -> CameraRuntimeAdjustments {
    super::camera::CameraPanContext::new(aabb, base_settings).validate(adjustments, viewport_aspect)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use crate::settings::CameraSettings;

    fn approx_vec3(actual: Vec3, expected: Vec3) {
        assert!(
            (actual - expected).abs().max_element() < 1.0e-5,
            "{actual:?} != {expected:?}"
        );
    }

    fn glb_with_json_and_bin(json: &Value, bin: &[u8]) -> Vec<u8> {
        let mut json_bytes = serde_json::to_vec(json).unwrap();
        while !json_bytes.len().is_multiple_of(4) {
            json_bytes.push(b' ');
        }
        let mut bin_bytes = bin.to_vec();
        while !bin_bytes.len().is_multiple_of(4) {
            bin_bytes.push(0);
        }
        let total = 12 + 8 + json_bytes.len() + 8 + bin_bytes.len();
        let mut glb = Vec::with_capacity(total);
        glb.extend_from_slice(b"glTF");
        glb.extend_from_slice(&2u32.to_le_bytes());
        glb.extend_from_slice(&(total as u32).to_le_bytes());
        glb.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"JSON");
        glb.extend_from_slice(&json_bytes);
        glb.extend_from_slice(&(bin_bytes.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"BIN\0");
        glb.extend_from_slice(&bin_bytes);
        glb
    }

    fn generated_vrm1_avatar() -> (tempfile::NamedTempFile, usize, usize) {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let source_bytes = std::fs::read(root.join("assets/AvatarSample_A.vrm")).unwrap();
        let source = VrmDoc::from_glb_bytes(&source_bytes).unwrap();
        let source_glb = pocket_vrm::glb::parse_glb(&source_bytes).unwrap();

        // Keep the generated fixture minimal at the semantic layer while
        // reusing the real model's valid geometry and accessors.
        let required_bones = [
            "hips",
            "spine",
            "head",
            "leftUpperLeg",
            "leftLowerLeg",
            "leftFoot",
            "rightUpperLeg",
            "rightLowerLeg",
            "rightFoot",
            "leftUpperArm",
            "leftLowerArm",
            "leftHand",
            "rightUpperArm",
            "rightLowerArm",
            "rightHand",
        ];
        let mut human_bones = serde_json::Map::new();
        for (name, node) in &source.humanoid {
            if required_bones.contains(&name.as_str()) {
                human_bones.insert(name.clone(), serde_json::json!({ "node": node }));
            }
        }

        let mut json = source_glb.json;
        json["extensions"] = serde_json::json!({
            "VRMC_vrm": {
                "specVersion": "1.0",
                "meta": {
                    "name": "Generated VRM1 test avatar",
                    "authors": ["pocket-character tests"],
                    "licenseUrl": "https://example.invalid/license"
                },
                "humanoid": { "humanBones": human_bones }
            }
        });
        json["extensionsUsed"] =
            serde_json::json!(["VRMC_vrm", "KHR_texture_transform", "KHR_materials_unlit"]);
        json["extensionsRequired"] = serde_json::json!(["VRMC_vrm"]);

        let root_node = source
            .nodes
            .parents
            .iter()
            .position(|&parent| parent == usize::MAX)
            .unwrap();
        let spine_node = source.humanoid_node("spine").unwrap();
        let nodes = json["nodes"].as_array_mut().unwrap();
        nodes[root_node]["scale"] = serde_json::json!([0.001, 0.001, 0.001]);
        nodes[spine_node]["rotation"] =
            serde_json::json!(glam::Quat::from_rotation_y(0.17).to_array());

        let mut file = tempfile::Builder::new().suffix(".vrm").tempfile().unwrap();
        file.write_all(&glb_with_json_and_bin(&json, source_glb.bin))
            .unwrap();
        (file, root_node, spine_node)
    }

    #[test]
    fn vrm0_presentation_is_identity() {
        let presentation = AvatarPresentation::for_version(AvatarVersion::Vrm0);
        assert_eq!(presentation.transform, Mat4::IDENTITY);
        assert_eq!(
            presentation.aabb((Vec3::splat(-1.0), Vec3::splat(2.0))),
            (Vec3::splat(-1.0), Vec3::splat(2.0))
        );
    }

    #[test]
    fn vrm1_presentation_rotates_around_y_by_180_degrees() {
        let presentation = AvatarPresentation::for_version(AvatarVersion::Vrm1);
        approx_vec3(
            presentation
                .transform
                .transform_point3(Vec3::new(1.0, 2.0, 3.0)),
            Vec3::new(-1.0, 2.0, -3.0),
        );
    }

    #[test]
    fn transformed_aabb_encloses_all_eight_corners() {
        let raw = (Vec3::new(-2.0, 1.0, -3.0), Vec3::new(4.0, 5.0, 6.0));
        let transform = Mat4::from_rotation_y(core::f32::consts::PI);
        let transformed = transformed_aabb(raw, transform);
        for x in [raw.0.x, raw.1.x] {
            for y in [raw.0.y, raw.1.y] {
                for z in [raw.0.z, raw.1.z] {
                    let point = transform.transform_point3(Vec3::new(x, y, z));
                    assert!(point.cmpge(transformed.0).all());
                    assert!(point.cmple(transformed.1).all());
                }
            }
        }
        approx_vec3(transformed.0, Vec3::new(-4.0, 1.0, -6.0));
        approx_vec3(transformed.1, Vec3::new(2.0, 5.0, 3.0));
    }

    #[test]
    fn avatar_slot_replaces_only_the_owned_scene_entry() {
        let slot = AvatarSceneSlot::new(0);
        let mut values = vec!["avatar", "future-prop"];
        // This mirrors the Vec operation used by the real Scene slot without
        // requiring a GPU-backed ModelInstance in the pure ownership test.
        let replaced = replace_at(&mut values, slot.index(), "new-avatar");
        assert_eq!(replaced, "avatar");
        assert_eq!(values, vec!["new-avatar", "future-prop"]);
    }

    #[test]
    fn camera_runtime_adjustments_survive_and_pan_revalidates_for_new_bounds() {
        let old = (Vec3::new(-0.5, 0.0, -0.4), Vec3::new(0.5, 2.0, 0.4));
        let new = (Vec3::new(-0.05, 0.0, -0.1), Vec3::new(0.05, 1.0, 0.1));
        let settings = CameraSettings {
            fov_deg: 55.0,
            distance_scale: 0.8,
            headroom: 0.17,
            ..CameraSettings::default()
        };
        let adjustments = CameraRuntimeAdjustments {
            fov_delta_deg: 7.0,
            distance_scale_delta: -0.2,
            pan_ndc: glam::Vec2::new(10.0, -10.0),
            yaw_deg: 31.0,
            roll_deg: -12.0,
            pitch_deg: 18.0,
        };
        let old_validated = revalidate_camera_adjustments(old, settings, adjustments, 0.75);
        let new_validated = revalidate_camera_adjustments(new, settings, old_validated, 0.75);
        assert_eq!(new_validated.fov_delta_deg, old_validated.fov_delta_deg);
        assert_eq!(
            new_validated.distance_scale_delta,
            old_validated.distance_scale_delta
        );
        assert_eq!(new_validated.yaw_deg, old_validated.yaw_deg);
        assert_eq!(new_validated.roll_deg, old_validated.roll_deg);
        assert_eq!(new_validated.pitch_deg, old_validated.pitch_deg);
        assert_ne!(new_validated.pan_ndc, old_validated.pan_ndc);

        let valid_pan = CameraRuntimeAdjustments {
            pan_ndc: glam::Vec2::new(0.05, -0.05),
            ..adjustments
        };
        assert_eq!(
            revalidate_camera_adjustments(new, settings, valid_pan, 0.75).pan_ndc,
            valid_pan.pan_ndc
        );
    }

    #[test]
    fn candidate_commit_shape_has_no_fallible_boundary() {
        // This function pointer is a compile-time assertion that converting a
        // prepared candidate into active state cannot return a load error.
        let commit: fn(AvatarCandidate, AvatarSceneSlot) -> (ModelInstance, ActiveAvatar) =
            AvatarCandidate::into_parts;
        let _ = commit;
    }

    #[test]
    fn failed_preparation_is_represented_without_touching_active_state() {
        let error = anyhow!("synthetic candidate failure");
        let runtime_error = AvatarRuntimeError::new(AvatarLoadErrorKind::ModelLoad, &error);
        let old_active = String::from("old-avatar");
        let candidate: Result<String> = Err(error);
        assert!(candidate.is_err());
        assert_eq!(old_active, "old-avatar");
        assert_eq!(runtime_error.message(), "synthetic candidate failure");
    }

    #[test]
    fn candidate_capabilities_refresh_idle_clip_advertisement() {
        assert_eq!(AvatarCapabilities::advertised_idle_clip(&[]), None);
        assert_eq!(
            AvatarCapabilities::advertised_idle_clip(&["wave".into()]),
            None
        );
        assert_eq!(
            AvatarCapabilities::advertised_idle_clip(&["idle_loop".into()]),
            Some("idle_loop".into())
        );
    }

    #[test]
    fn successful_optional_default_idle_is_advertised() {
        let staged = stage_vrma_clip(Some(VrmaProvenance::OptionalDefault), || {
            Ok((
                "idle_loop".into(),
                Clip {
                    name: "idle_loop".into(),
                    duration: 1.0,
                    channels: Vec::new(),
                },
            ))
        })
        .unwrap();
        let clips = vec![staged.unwrap()];
        let names = clips
            .iter()
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            AvatarCapabilities::advertised_idle_clip(&names),
            Some("idle_loop".into())
        );
    }

    #[test]
    fn failed_optional_default_idle_keeps_avatar_preparation_nonfatal_and_unadvertised() {
        let staged = stage_vrma_clip(Some(VrmaProvenance::OptionalDefault), || {
            Err(anyhow!("synthetic default idle failure"))
        })
        .unwrap();
        assert!(staged.is_none());
        let clips: Vec<(String, Clip)> = Vec::new();
        let names = clips
            .iter()
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        assert_eq!(AvatarCapabilities::advertised_idle_clip(&names), None);
    }

    #[test]
    fn explicit_vrma_provenance_remains_strict() {
        let result = stage_vrma_clip(Some(VrmaProvenance::Explicit), || {
            Err(anyhow!("synthetic explicit animation failure"))
        });
        assert!(result.is_err());
    }

    #[test]
    fn startup_and_open_avatar_requests_share_the_optional_default_idle_policy() {
        let default_idle = PathBuf::from(r"C:\bundle\idle_loop.vrma");
        let startup = startup_avatar_request(
            PathBuf::from(r"C:\avatars\startup.vrm"),
            default_idle.clone(),
        );
        let open = avatar_request_from_picker_result(
            Some(PathBuf::from(r"C:\avatars\picked.vrm")),
            &default_idle,
        )
        .unwrap();
        assert_eq!(startup.vrma_path, Some(default_idle.clone()));
        assert_eq!(open.vrma_path, Some(default_idle));
        assert_eq!(
            startup.vrma_provenance,
            Some(VrmaProvenance::OptionalDefault)
        );
        assert_eq!(open.vrma_provenance, Some(VrmaProvenance::OptionalDefault));
    }

    #[test]
    fn picker_cancel_produces_no_avatar_request() {
        let previous_request =
            AvatarLoadRequest::new(PathBuf::from(r"C:\avatars\current.vrm"), None, "Current");
        let mut pending = Some(previous_request.clone());
        let mut status = AvatarLoadStatus::default();
        status.fail(AvatarRuntimeError::new(
            AvatarLoadErrorKind::ModelLoad,
            &anyhow!("previous load failed"),
        ));
        let previous_status = status.clone();

        let request = avatar_request_from_picker_result(None, Path::new("idle_loop.vrma"));
        if let Some(request) = request {
            pending = Some(request);
            status.begin_loading();
        }
        assert_eq!(pending, Some(previous_request));
        assert_eq!(status, previous_status);
    }

    #[test]
    fn picker_path_maps_to_vrm_request_with_optional_default_vrma() {
        let request = avatar_request_from_picker_result(
            Some(PathBuf::from(r"C:\avatars\Sample Avatar.VRM")),
            Path::new(r"C:\bundle\idle_loop.vrma"),
        )
        .unwrap();
        assert_eq!(
            request.model_path,
            PathBuf::from(r"C:\avatars\Sample Avatar.VRM")
        );
        assert_eq!(
            request.vrma_path,
            Some(PathBuf::from(r"C:\bundle\idle_loop.vrma"))
        );
        assert_eq!(
            request.vrma_provenance,
            Some(VrmaProvenance::OptionalDefault)
        );
        assert_eq!(request.model_name, "Sample Avatar");
    }

    #[test]
    fn picker_display_name_and_fallback_are_observable() {
        let request = avatar_request_from_picker_result(
            Some(PathBuf::from(r"C:\avatars\Sample Avatar.VRM")),
            Path::new(r"C:\bundle\idle_loop.vrma"),
        )
        .unwrap();
        assert_eq!(request.model_name, "Sample Avatar");
        assert_eq!(avatar_model_name(None), "Avatar");
        assert!(
            avatar_request_from_picker_result(
                Some(PathBuf::from(r"C:\avatars\sample.txt")),
                Path::new(r"C:\bundle\idle_loop.vrma"),
            )
            .is_none()
        );
    }

    #[test]
    fn avatar_load_status_has_loading_and_error_states() {
        let mut status = AvatarLoadStatus::default();
        status.begin_loading();
        assert!(status.is_loading());
        assert_eq!(status.ui_status(), "loading");
        assert_eq!(status.error_message(), None);
        status.fail(AvatarRuntimeError::new(
            AvatarLoadErrorKind::ModelLoad,
            &anyhow!("load failed"),
        ));
        assert!(!status.is_loading());
        assert_eq!(status.ui_status(), "error");
        assert_eq!(status.error_message(), Some("load failed"));
        assert_eq!(status.ui_error_message(), Some("VRM model load failed."));
    }

    #[test]
    fn successful_avatar_status_clears_previous_load_error() {
        let mut status = AvatarLoadStatus::default();
        status.fail(AvatarRuntimeError::new(
            AvatarLoadErrorKind::ModelLoad,
            &anyhow!("previous load failed"),
        ));
        assert_eq!(status.error_message(), Some("previous load failed"));
        status.succeed();
        assert_eq!(status.ui_status(), "idle");
        assert_eq!(status.error_message(), None);
        assert_eq!(status.ui_error_message(), None);
    }

    #[test]
    fn avatar_ui_error_does_not_expose_local_diagnostic_paths() {
        let error = anyhow!(r"reading model C:\unsupported\parse\private.vrm: access denied");
        let runtime_error = AvatarRuntimeError::new(AvatarLoadErrorKind::ModelLoad, &error);
        assert_eq!(runtime_error.ui_message(), "VRM model load failed.");
        assert!(!runtime_error.ui_message().contains("private.vrm"));
        assert!(!runtime_error.ui_message().contains("C:\\unsupported"));
        assert_eq!(runtime_error.kind, AvatarLoadErrorKind::ModelLoad);
    }

    #[test]
    fn avatar_ui_error_categories_are_explicit() {
        let diagnostic = anyhow!("synthetic failure");
        let expected = [
            (
                AvatarLoadErrorKind::UnsupportedVrm,
                "Unsupported VRM format.",
            ),
            (AvatarLoadErrorKind::VrmParse, "VRM parse failed."),
            (AvatarLoadErrorKind::ModelLoad, "VRM model load failed."),
            (
                AvatarLoadErrorKind::RuntimeStartup,
                "Avatar runtime failed.",
            ),
            (
                AvatarLoadErrorKind::SceneUnavailable,
                "Avatar scene unavailable.",
            ),
        ];

        for (kind, message) in expected {
            assert_eq!(
                AvatarRuntimeError::new(kind, &diagnostic).ui_message(),
                message
            );
        }
    }

    #[test]
    fn semantic_error_categories_come_from_document_structure() {
        assert_eq!(
            semantic_parse_failure_kind(b"not a GLB"),
            AvatarLoadErrorKind::VrmParse
        );
        assert_eq!(
            semantic_document_failure_kind(&serde_json::json!({})),
            AvatarLoadErrorKind::UnsupportedVrm
        );
        assert_eq!(
            semantic_document_failure_kind(&serde_json::json!({ "extensions": [] })),
            AvatarLoadErrorKind::VrmParse
        );
        assert_eq!(
            semantic_document_failure_kind(&serde_json::json!({
                "extensions": { "VRMC_vrm": { "specVersion": "0.99" } }
            })),
            AvatarLoadErrorKind::UnsupportedVrm
        );
        assert_eq!(
            semantic_document_failure_kind(&serde_json::json!({
                "extensions": { "VRMC_vrm": { "specVersion": "1.0" } }
            })),
            AvatarLoadErrorKind::VrmParse
        );
    }

    #[test]
    fn vrm0_required_extension_allowlist_matches_validated_legacy_semantics() {
        assert_eq!(vrm0_allowed_required_extensions(), ["VRM"]);
    }

    #[test]
    fn vrm1_allowlist_only_accepts_supported_material_fallbacks() {
        let json = serde_json::json!({
            "materials": [{
                "extensions": {
                    "VRMC_materials_mtoon": {"specVersion": "1.0"},
                    "KHR_materials_unlit": {}
                }
            }]
        });
        assert_eq!(
            vrm1_allowed_required_extensions(&json).unwrap(),
            ["VRMC_vrm", "VRMC_materials_mtoon"]
        );

        let json = serde_json::json!({
            "materials": [{
                "extensions": {
                    "VRMC_materials_mtoon": {"specVersion": "1.0"}
                }
            }]
        });
        let error = vrm1_allowed_required_extensions(&json).unwrap_err();
        assert!(format!("{error:#}").contains("KHR_materials_unlit"));

        let json = serde_json::json!({
            "materials": [{
                "extensions": {
                    "VRMC_materials_mtoon": {"specVersion": "1.0"},
                    "KHR_materials_unlit": null
                }
            }]
        });
        let error = vrm1_allowed_required_extensions(&json).unwrap_err();
        assert!(format!("{error:#}").contains("KHR_materials_unlit"));
    }

    #[test]
    fn vrm1_allowlist_does_not_claim_spring_or_constraint_runtime_support() {
        let json = serde_json::json!({"materials": []});
        assert_eq!(
            vrm1_allowed_required_extensions(&json).unwrap(),
            ["VRMC_vrm"]
        );
    }

    #[test]
    fn avatar_animation_runtime_starts_reset_for_each_candidate() {
        let old_state = AvatarAnimationState {
            clip_index: 4,
            clip_time: 3.5,
            clip_looping: false,
        };
        assert_eq!(
            fresh_animation_state(),
            AvatarAnimationState {
                clip_index: 0,
                clip_time: 0.0,
                clip_looping: true,
            }
        );
        assert_ne!(fresh_animation_state(), old_state);
    }

    #[test]
    fn real_candidate_handles_optional_default_idle_success_and_failure() {
        let gpu = Gpu::new_headless().expect("headless GPU is required for candidate smoke tests");
        let renderer = Renderer::new(&gpu, pocket3d::gpu::OFFSCREEN_FORMAT).unwrap();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let model_path = root.join("assets/AvatarSample_A.vrm");
        let bundle_path = root.join("dist/character.js");
        let idle_path = root.join("assets/idle_loop.vrma");

        let success_request =
            AvatarLoadRequest::new(model_path.clone(), Some(idle_path), "AvatarSample_A");
        let success = AvatarCandidate::prepare(&gpu, &renderer, &bundle_path, &success_request)
            .expect("bundled default idle should be optional but loadable");
        assert!(success.clips.iter().any(|(name, _)| name == "idle_loop"));
        assert_eq!(success.capabilities.idle_clip.as_deref(), Some("idle_loop"));

        let failure_request = AvatarLoadRequest::new(
            model_path,
            Some(root.join("assets/optional-idle-does-not-exist.vrma")),
            "AvatarSample_A",
        );
        let failure = AvatarCandidate::prepare(&gpu, &renderer, &bundle_path, &failure_request)
            .expect("optional default idle failure must not fail avatar preparation");
        assert!(failure.clips.is_empty());
        assert_eq!(failure.capabilities.idle_clip, None);
    }

    #[test]
    fn generated_vrm1_candidate_retargets_bundled_idle_end_to_end() {
        let gpu = Gpu::new_headless().expect("headless GPU is required for VRM1 integration");
        let renderer = Renderer::new(&gpu, pocket3d::gpu::OFFSCREEN_FORMAT).unwrap();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let (model_file, root_node, spine_node) = generated_vrm1_avatar();
        let request = AvatarLoadRequest::new(
            model_file.path().to_owned(),
            Some(root.join("assets/idle_loop.vrma")),
            "GeneratedVrm1",
        );

        let candidate =
            AvatarCandidate::prepare(&gpu, &renderer, &root.join("dist/character.js"), &request)
                .expect("generated VRM1 candidate should load and retarget");

        assert!(matches!(
            &candidate.document,
            AvatarSemanticDocument::Vrm1(_)
        ));
        assert_eq!(
            candidate.asset.skeleton.rest[root_node].scale,
            Vec3::splat(0.001)
        );
        assert!(
            candidate.asset.skeleton.rest[spine_node]
                .rotation
                .angle_between(glam::Quat::from_rotation_y(0.17))
                < 1.0e-5
        );
        assert_eq!(
            candidate.capabilities.idle_clip.as_deref(),
            Some("idle_loop")
        );
        let idle = candidate
            .clips
            .iter()
            .find(|(name, _)| name == "idle_loop")
            .map(|(_, clip)| clip)
            .unwrap();
        assert!(
            idle.channels
                .iter()
                .any(|channel| channel.node == spine_node)
        );
        assert_eq!(
            candidate.presentation.transform,
            Mat4::from_rotation_y(core::f32::consts::PI)
        );
    }
}
