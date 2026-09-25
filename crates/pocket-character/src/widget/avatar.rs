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
use pocket_character_core::{CharacterSim, SimOutputs};
use pocket_vrm::{
    HumanoidView, SpringSolver, Vrm1Doc, Vrm1LookAtRuntime, VrmDoc, retarget_with_humanoid,
};
use pocket3d::anim::{Clip, NodeTrs};
use pocket3d::gpu::Gpu;
use pocket3d::model::{ModelAsset, ModelInstance, ModelLoadOptions};
use pocket3d::renderer::Renderer;
use pocket3d::scene::Scene;
use serde_json::Value;

use crate::guest::CharacterGuest;

use super::expression::{ResolvedExpressionRuntime, resolve_vrm1};
use super::node_constraint::Vrm1NodeConstraintRuntime;

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

    pub(super) fn humanoid_node(&self, bone: &str) -> Option<usize> {
        match self {
            Self::Vrm0(document) => document.humanoid_node(bone),
            Self::Vrm1(document) => document
                .humanoid
                .human_bones
                .iter()
                .find(|(name, _)| name.as_str() == bone)
                .map(|(_, &node)| node),
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
    model_from_world: Mat4,
}

impl AvatarPresentation {
    pub(super) fn for_version(version: AvatarVersion) -> Self {
        let transform = match version {
            AvatarVersion::Vrm0 => Mat4::IDENTITY,
            AvatarVersion::Vrm1 => Mat4::from_rotation_y(core::f32::consts::PI),
        };
        Self {
            version,
            transform,
            model_from_world: transform.inverse(),
        }
    }

    /// Transform all eight raw AABB corners.  Rotating only the min/max
    /// endpoints is not a valid AABB transform for a non-axis-preserving
    /// presentation transform.
    pub(super) fn aabb(self, raw: (Vec3, Vec3)) -> (Vec3, Vec3) {
        transformed_aabb(raw, self.transform)
    }

    pub(super) fn model_point_from_world(self, world: Vec3) -> Vec3 {
        self.model_from_world.transform_point3(world)
    }

    pub(super) fn world_point_from_model(self, model: Vec3) -> Vec3 {
        self.transform.transform_point3(model)
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
        expressions: &ResolvedExpressionRuntime,
    ) -> Self {
        let clip_names = clips
            .iter()
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        let idle_clip = Self::advertised_idle_clip(&clip_names);
        Self {
            model_name,
            clip_names,
            expression_names: match expressions {
                ResolvedExpressionRuntime::Vrm0Legacy => document.expression_names(),
                ResolvedExpressionRuntime::Vrm1(runtime) => runtime.capability_names(),
            },
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
    pub(super) mtoon_render_mode: crate::settings::MtoonRenderMode,
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
            mtoon_render_mode: crate::settings::MtoonRenderMode::Auto,
        }
    }
}

/// Convert the native picker result into the parent-side request seam. The
/// UI never receives this path and cancellation is represented by `None`.
pub(crate) fn avatar_request_from_picker_result(
    selected: Option<PathBuf>,
    default_vrma_path: &Path,
) -> Option<AvatarLoadRequest> {
    avatar_request_from_path(selected?, default_vrma_path)
}

/// Shared path conversion for the native picker and window file drops.
pub(crate) fn avatar_request_from_path(
    path: PathBuf,
    default_vrma_path: &Path,
) -> Option<AvatarLoadRequest> {
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

fn vrm1_allowed_required_extensions(
    json: &Value,
    mtoon_supported: bool,
    spring_bone_supported: bool,
    node_constraint_supported: bool,
) -> Result<Vec<&'static str>> {
    let mut allowed = vec!["VRMC_vrm"];
    if extension_declaration_contains(json, "extensionsRequired", "VRMC_materials_mtoon")? {
        ensure!(
            mtoon_supported,
            "VRMC_materials_mtoon is required but its supported 1.0 semantics were not parsed"
        );
        allowed.push("VRMC_materials_mtoon");
    }
    if json
        .get("extensions")
        .and_then(Value::as_object)
        .is_some_and(|extensions| extensions.contains_key("VRMC_springBone"))
    {
        ensure!(
            spring_bone_supported,
            "VRMC_springBone is present but its supported 1.0 semantics were not parsed"
        );
        allowed.push("VRMC_springBone");
    }
    if extension_declaration_contains(json, "extensionsRequired", "VRMC_node_constraint")? {
        ensure!(
            node_constraint_supported,
            "VRMC_node_constraint is required but its supported 1.0 semantics were not parsed"
        );
        allowed.push("VRMC_node_constraint");
    }
    Ok(allowed)
}

fn extension_declaration_contains(json: &Value, key: &str, name: &str) -> Result<bool> {
    let Some(value) = json.get(key) else {
        return Ok(false);
    };
    let declarations = value
        .as_array()
        .with_context(|| format!("VRM 1.0 glTF {key} must be an array"))?;
    let mut found = false;
    for (index, value) in declarations.iter().enumerate() {
        let extension = value
            .as_str()
            .with_context(|| format!("VRM 1.0 glTF {key}[{index}] must be a string"))?;
        found |= extension == name;
    }
    Ok(found)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AvatarLoadErrorKind {
    UnsupportedFile,
    UnsupportedVrm,
    NativeMtoonUnsupported,
    VrmParse,
    ModelLoad,
    RuntimeStartup,
    SceneUnavailable,
}

impl AvatarLoadErrorKind {
    fn ui_message(self) -> &'static str {
        match self {
            Self::UnsupportedFile => "Drop a .vrm avatar file.",
            Self::UnsupportedVrm => "Unsupported VRM format.",
            Self::NativeMtoonUnsupported => "Native MToon cannot render this avatar. See log.",
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
    pub(super) request: AvatarLoadRequest,
    pub(super) asset: Arc<ModelAsset>,
    pub(super) mtoon_declared_count: usize,
    pub(super) instance: ModelInstance,
    pub(super) document: AvatarSemanticDocument,
    pub(super) presentation: AvatarPresentation,
    pub(super) presentation_aabb: (Vec3, Vec3),
    pub(super) locals: Vec<NodeTrs>,
    pub(super) globals: Vec<Mat4>,
    pub(super) clips: Vec<(String, Clip)>,
    pub(super) springs: Option<SpringSolver>,
    pub(super) blink_binds: Vec<(usize, usize, f32)>,
    pub(super) expressions: ResolvedExpressionRuntime,
    pub(super) look_at: Option<Vrm1LookAtRuntime>,
    pub(super) node_constraints: Option<Vrm1NodeConstraintRuntime>,
    pub(super) capabilities: AvatarCapabilities,
    pub(super) guest: CharacterGuest,
    pub(super) sim: CharacterSim,
    pub(super) animation: AvatarAnimationState,
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
        log::info!("MToon rendering requested: {:?}", request.mtoon_render_mode);

        let mut node_constraint_required = false;
        let mut mtoon_declared_count = 0;
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
                        ..Default::default()
                    },
                    vrm0_allowed_required_extensions(),
                )
            }
            AvatarVersion::Vrm1 => {
                let mtoon_descriptors = match &document {
                    AvatarSemanticDocument::Vrm1(document) => document.mtoon_material_descriptors(),
                    AvatarSemanticDocument::Vrm0(_) => unreachable!(),
                };
                let glb = pocket_vrm::glb::parse_glb(&model_bytes)
                    .context("parsing VRM 1.0 glTF material extensions")
                    .map_err(|error| {
                        AvatarRuntimeError::new(AvatarLoadErrorKind::VrmParse, &error)
                    })?;
                mtoon_declared_count = glb.json.get("materials")
                    .and_then(serde_json::Value::as_array)
                    .map_or(0, |materials| materials.iter().filter(|material| {
                        material.get("extensions").and_then(|extensions| extensions.get("VRMC_materials_mtoon")).is_some()
                    }).count());
                let spring_bone_supported = matches!(
                    &document,
                    AvatarSemanticDocument::Vrm1(document)
                        if document.spring_bone_semantics.is_some()
                );
                let node_constraint_supported = matches!(
                    &document,
                    AvatarSemanticDocument::Vrm1(document)
                        if document.node_constraint_semantics.is_some()
                );
                let mtoon_supported = matches!(
                    &document,
                    AvatarSemanticDocument::Vrm1(document) if document.materials_mtoon.present
                );
                if request.mtoon_render_mode == crate::settings::MtoonRenderMode::Native
                    && !mtoon_supported
                    && mtoon_declared_count > 0
                {
                    return Err(AvatarRuntimeError::new(
                        AvatarLoadErrorKind::NativeMtoonUnsupported,
                        &anyhow::anyhow!("Native MToon requested, but the avatar's optional VRMC_materials_mtoon semantics cannot be represented by the native renderer"),
                    ));
                }
                node_constraint_required = extension_declaration_contains(
                    &glb.json,
                    "extensionsRequired",
                    "VRMC_node_constraint",
                )
                .map_err(|error| AvatarRuntimeError::new(AvatarLoadErrorKind::VrmParse, &error))?;
                let allowed_required_extensions = vrm1_allowed_required_extensions(
                    &glb.json,
                    mtoon_supported,
                    spring_bone_supported,
                    node_constraint_supported,
                )
                .map_err(|error| {
                    AvatarRuntimeError::new(AvatarLoadErrorKind::UnsupportedVrm, &error)
                })?;
                ModelAsset::load_glb_bytes_opts_with_native_mtoon(
                    gpu,
                    &renderer.model_material_layout,
                    &renderer.mtoon_material_layout,
                    &renderer.samplers,
                    &model_bytes,
                    &request.model_path.to_string_lossy(),
                    &ModelLoadOptions {
                        max_texture_dim: Some(2048),
                        mtoon_render_mode: match request.mtoon_render_mode {
                            crate::settings::MtoonRenderMode::Auto => pocket3d::model::MtoonRenderMode::Auto,
                            crate::settings::MtoonRenderMode::Native => pocket3d::model::MtoonRenderMode::Native,
                            crate::settings::MtoonRenderMode::Fallback => pocket3d::model::MtoonRenderMode::Fallback,
                        },
                    },
                    allowed_required_extensions,
                    &mtoon_descriptors,
                )
            }
        }
        .context("loading VRM model")
        .map_err(|error| AvatarRuntimeError::new(AvatarLoadErrorKind::ModelLoad, &error))?;
        if document.version() == AvatarVersion::Vrm1 {
            let native_count = model.native_mtoon_material_count;
            log::info!(
                "MToon rendering: {:?} (asset load); materials: {native_count} native / {} glTF fallback",
                request.mtoon_render_mode,
                mtoon_declared_count.saturating_sub(native_count)
            );
        }

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

        let expressions = match &document {
            AvatarSemanticDocument::Vrm0(_) => ResolvedExpressionRuntime::Vrm0Legacy,
            AvatarSemanticDocument::Vrm1(vrm1) => resolve_vrm1(vrm1, &model)
                .context("resolving VRM 1.0 expressions")
                .map_err(|error| AvatarRuntimeError::new(AvatarLoadErrorKind::ModelLoad, &error))?,
        };
        let look_at = match &document {
            AvatarSemanticDocument::Vrm0(_) => None,
            AvatarSemanticDocument::Vrm1(vrm1) => Vrm1LookAtRuntime::new(vrm1, &model.skeleton),
        };

        let mut locals = Vec::new();
        model.skeleton.sample_locals(None, 0.0, false, &mut locals);
        let mut globals = Vec::new();
        model.skeleton.globals_from_locals(&locals, &mut globals);

        let node_constraints = match &document {
            AvatarSemanticDocument::Vrm0(_) => None,
            AvatarSemanticDocument::Vrm1(vrm1) => match &vrm1.node_constraint_semantics {
                Some(semantics) => match Vrm1NodeConstraintRuntime::new(semantics, &model.skeleton)
                {
                    Ok(runtime) if runtime.is_empty() => None,
                    Ok(runtime) => Some(runtime),
                    Err(error) if node_constraint_required => {
                        return Err(AvatarRuntimeError::new(
                            AvatarLoadErrorKind::UnsupportedVrm,
                            &error.context("resolving required VRMC_node_constraint runtime"),
                        ));
                    }
                    Err(error) => {
                        log::warn!(
                            "disabling invalid optional VRMC_node_constraint extension: {error:#}"
                        );
                        None
                    }
                },
                None if node_constraint_required => {
                    let error =
                        anyhow!("required VRMC_node_constraint runtime could not be constructed");
                    return Err(AvatarRuntimeError::new(
                        AvatarLoadErrorKind::UnsupportedVrm,
                        &error,
                    ));
                }
                None => None,
            },
        };

        let springs = match &document {
            AvatarSemanticDocument::Vrm0(vrm0) => {
                Some(SpringSolver::new(&vrm0.springs, &model.skeleton, &locals))
            }
            AvatarSemanticDocument::Vrm1(vrm1) => {
                vrm1.spring_bone_semantics.as_ref().and_then(|spring_bone| {
                    SpringSolver::new_vrm1(spring_bone, &model.skeleton, &locals)
                })
            }
        };

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
        instance.mtoon_draw_route = match request.mtoon_render_mode {
            crate::settings::MtoonRenderMode::Auto => pocket3d::model::MtoonRenderMode::Auto,
            crate::settings::MtoonRenderMode::Native => pocket3d::model::MtoonRenderMode::Native,
            crate::settings::MtoonRenderMode::Fallback => {
                pocket3d::model::MtoonRenderMode::Fallback
            }
        };
        instance.morph = model.create_morph_state(gpu);
        instance.transform = presentation.transform;
        instance.cutout = 0.5;
        instance.lit = 0.25;

        let bundle = std::fs::read_to_string(bundle_path)
            .with_context(|| format!("reading bundle {}", bundle_path.display()))
            .map_err(|error| {
                AvatarRuntimeError::new(AvatarLoadErrorKind::RuntimeStartup, &error)
            })?;
        let capabilities = AvatarCapabilities::for_candidate(
            request.model_name.clone(),
            &document,
            &clips,
            &expressions,
        );
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
        let mut candidate = Self {
            request: request.clone(),
            asset: model,
            mtoon_declared_count,
            instance,
            document,
            presentation,
            presentation_aabb,
            locals,
            globals,
            clips,
            springs,
            blink_binds,
            expressions,
            look_at,
            node_constraints,
            capabilities,
            guest,
            sim: CharacterSim::new(AVATAR_SIM_SEED, Vec3::ZERO),
            animation: fresh_animation_state(),
        };
        candidate.warm_initial_pose()?;
        Ok(candidate)
    }

    fn warm_initial_pose(&mut self) -> std::result::Result<(), AvatarRuntimeError> {
        let Self {
            asset,
            instance,
            document,
            presentation,
            locals,
            globals,
            clips,
            springs,
            blink_binds,
            expressions,
            look_at,
            node_constraints,
            sim,
            animation,
            ..
        } = self;
        AvatarPoseState {
            asset,
            document,
            presentation: *presentation,
            locals,
            globals,
            clips,
            springs,
            blink_binds,
            expressions,
            look_at,
            node_constraints,
            sim,
            animation,
            look_at_enabled: true,
            auto_blink_enabled: true,
            previous_blink: 0.0,
        }
        .advance(instance, 0.0);

        self.validate_initial_pose()
    }

    fn validate_initial_pose(&self) -> std::result::Result<(), AvatarRuntimeError> {
        let pose = self
            .instance
            .pose
            .as_ref()
            .expect("pose update sets instance pose");
        if pose.len() != self.asset.skeleton.rest.len() || !pose.iter().all(|m| m.is_finite()) {
            return Err(AvatarRuntimeError::new(
                AvatarLoadErrorKind::RuntimeStartup,
                &anyhow!("initial avatar pose is invalid"),
            ));
        }
        let mut palette = Vec::new();
        self.asset.palette_from_globals(pose, &mut palette);
        if !palette.iter().all(|transform| transform.is_finite()) {
            return Err(AvatarRuntimeError::new(
                AvatarLoadErrorKind::RuntimeStartup,
                &anyhow!("initial avatar skin palette is invalid"),
            ));
        }
        Ok(())
    }

    pub(super) fn into_parts(self, scene_slot: AvatarSceneSlot) -> (ModelInstance, ActiveAvatar) {
        let Self {
            request,
            asset,
            mtoon_declared_count,
            instance,
            document,
            presentation,
            presentation_aabb,
            locals,
            globals,
            clips,
            springs,
            blink_binds,
            expressions,
            look_at,
            node_constraints,
            capabilities,
            guest,
            sim,
            animation,
        } = self;
        (
            instance,
            ActiveAvatar {
                request,
                asset,
                mtoon_declared_count,
                document,
                presentation,
                presentation_aabb,
                scene_slot,
                locals,
                globals,
                clips,
                springs,
                blink_binds,
                expressions,
                look_at,
                node_constraints,
                capabilities,
                guest,
                sim,
                animation,
            },
        )
    }
}

/// The one avatar currently owned by the parent runtime.
pub(super) struct ActiveAvatar {
    pub(super) request: AvatarLoadRequest,
    pub(super) asset: Arc<ModelAsset>,
    pub(super) mtoon_declared_count: usize,
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
    pub(super) expressions: ResolvedExpressionRuntime,
    pub(super) look_at: Option<Vrm1LookAtRuntime>,
    pub(super) node_constraints: Option<Vrm1NodeConstraintRuntime>,
    #[allow(dead_code)]
    pub(super) capabilities: AvatarCapabilities,
    pub(super) guest: CharacterGuest,
    pub(super) sim: CharacterSim,
    pub(super) animation: AvatarAnimationState,
}

impl ActiveAvatar {
    pub(super) fn has_route(&self, mode: crate::settings::MtoonRenderMode) -> bool {
        let semantics_supported = match &self.document {
            AvatarSemanticDocument::Vrm0(_) => true,
            AvatarSemanticDocument::Vrm1(document) => document.materials_mtoon.present,
        };
        match mode {
            crate::settings::MtoonRenderMode::Fallback => true,
            crate::settings::MtoonRenderMode::Auto => {
                self.asset.native_mtoon_material_count == self.asset.native_mtoon_eligible_count
            }
            crate::settings::MtoonRenderMode::Native => {
                self.mtoon_declared_count == 0
                    || (semantics_supported
                        && self.asset.native_mtoon_material_count == self.mtoon_declared_count)
            }
        }
    }

    pub(super) fn advance_pose(
        &mut self,
        instance: &mut ModelInstance,
        dt: f32,
        look_at_enabled: bool,
        auto_blink_enabled: bool,
        previous_blink: f32,
    ) -> SimOutputs {
        let Self {
            asset,
            document,
            presentation,
            locals,
            globals,
            clips,
            springs,
            blink_binds,
            expressions,
            look_at,
            node_constraints,
            sim,
            animation,
            ..
        } = self;
        AvatarPoseState {
            asset,
            document,
            presentation: *presentation,
            locals,
            globals,
            clips,
            springs,
            blink_binds,
            expressions,
            look_at,
            node_constraints,
            sim,
            animation,
            look_at_enabled,
            auto_blink_enabled,
            previous_blink,
        }
        .advance(instance, dt)
    }
}

/// Shared first-frame and runtime pose path. A candidate owns its instance
/// until this has completed, so a replacement cannot expose the rest pose.
struct AvatarPoseState<'a> {
    asset: &'a Arc<ModelAsset>,
    document: &'a AvatarSemanticDocument,
    presentation: AvatarPresentation,
    locals: &'a mut Vec<NodeTrs>,
    globals: &'a mut Vec<Mat4>,
    clips: &'a [(String, Clip)],
    springs: &'a mut Option<SpringSolver>,
    blink_binds: &'a [(usize, usize, f32)],
    expressions: &'a mut ResolvedExpressionRuntime,
    look_at: &'a Option<Vrm1LookAtRuntime>,
    node_constraints: &'a mut Option<Vrm1NodeConstraintRuntime>,
    sim: &'a mut CharacterSim,
    animation: &'a mut AvatarAnimationState,
    look_at_enabled: bool,
    auto_blink_enabled: bool,
    previous_blink: f32,
}

impl AvatarPoseState<'_> {
    fn advance(self, instance: &mut ModelInstance, dt: f32) -> SimOutputs {
        let out = self.sim.tick(dt);

        self.animation.clip_time += dt;
        let clip = self
            .clips
            .get(self.animation.clip_index)
            .map(|(_, clip)| clip);
        self.asset.skeleton.sample_locals(
            clip,
            self.animation.clip_time,
            self.animation.clip_looping,
            self.locals,
        );

        self.globals.resize(self.locals.len(), Mat4::IDENTITY);
        self.asset
            .skeleton
            .globals_from_locals(self.locals, self.globals);
        let mut procedural_look_at = pocket_vrm::Vrm1ExpressionLookAt::default();
        if self.look_at_enabled
            && let Some(vrm) = self.document.vrm0()
        {
            let head = vrm
                .humanoid_node("head")
                .map(|node| self.globals[node].w_axis.truncate());
            if let Some(head_pos) = head {
                let direction = out.look_target - head_pos;
                let yaw = (-direction.x).atan2(-direction.z).to_degrees();
                let pitch = direction
                    .y
                    .atan2(Vec3::new(direction.x, 0.0, direction.z).length())
                    .to_degrees();
                pocket_vrm::apply_eye_look(
                    self.locals,
                    &self.asset.skeleton.rest,
                    vrm.humanoid_node("leftEye"),
                    vrm.humanoid_node("rightEye"),
                    &vrm.look_at,
                    yaw,
                    pitch,
                );
            }
        }
        if self.look_at_enabled
            && let Some(look_at) = self.look_at.as_ref()
        {
            let target_model = self.presentation.model_point_from_world(out.look_target);
            let output = look_at.evaluate(target_model, self.locals, self.globals);
            output.apply_bone_rotations(self.locals);
            procedural_look_at = output.expression_weights();
        }

        let blink = if self.auto_blink_enabled {
            out.blink
        } else {
            0.0
        };
        match self.expressions {
            ResolvedExpressionRuntime::Vrm0Legacy if blink != self.previous_blink => {
                if let Some(morph) = instance.morph.as_mut() {
                    for &(slot, target, weight) in self.blink_binds {
                        morph.set_weight(slot, target, blink * weight);
                    }
                }
            }
            ResolvedExpressionRuntime::Vrm1(runtime) => {
                runtime.compose_on_instance(instance, blink, procedural_look_at);
            }
            ResolvedExpressionRuntime::Vrm0Legacy => {}
        }

        if let Some(constraints) = self.node_constraints.as_mut() {
            constraints.evaluate(&self.asset.skeleton, self.locals, self.globals);
        }
        if let Some(springs) = self.springs.as_mut() {
            springs.step(dt, &self.asset.skeleton, self.locals, Mat4::IDENTITY);
        }

        self.asset
            .skeleton
            .globals_from_locals(self.locals, self.globals);
        instance.pose = Some(self.globals.clone());
        out
    }
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
    use pocket_character_core::TrackingMode;
    use pocket3d::app::Game;
    use pocket3d::input::Input;

    use super::super::{Widget, WidgetConfig};

    fn maybe_capture_ppm(name: &str, rgba: &[u8], size: (u32, u32)) {
        let Ok(directory) = std::env::var("MTOON_STAGE_B_CAPTURE_DIR") else {
            return;
        };
        let mut bytes = format!("P6\n{} {}\n255\n", size.0, size.1).into_bytes();
        for pixel in rgba.as_chunks::<4>().0 {
            bytes.extend_from_slice(&pixel[..3]);
        }
        std::fs::write(Path::new(&directory).join(name), bytes).unwrap();
    }

    fn render_avatar_smoke(
        gpu: &Gpu,
        renderer: &mut Renderer,
        asset: Arc<ModelAsset>,
        capture_name: &str,
    ) -> usize {
        let target = pocket3d::gpu::OffscreenTarget::new(gpu, 256, 256);
        let (min, max) = asset.aabb;
        let center = (min + max) * 0.5;
        let radius = (max - min).length().max(1.0);
        let camera = pocket3d::camera::Camera {
            pos: center + Vec3::new(0.0, 0.0, radius * 1.5),
            fov_y: 45.0_f32.to_radians(),
            znear: 0.01,
            zfar: radius * 10.0,
            ..Default::default()
        };
        let mut scene = pocket3d::scene::Scene::default();
        scene
            .models
            .push(pocket3d::model::ModelInstance::new(asset));
        renderer.render(
            gpu,
            &target.view,
            target.size,
            &scene,
            &camera,
            &pocket3d::hud::Hud::default(),
        );
        gpu.device.poll(wgpu::PollType::Wait).unwrap();
        let rgba = target.read_rgba(gpu).unwrap();
        maybe_capture_ppm(capture_name, &rgba, target.size);
        let background = &rgba[..4];
        rgba.as_chunks::<4>()
            .0
            .iter()
            .filter(|pixel| pixel.iter().zip(background).any(|(a, b)| a != b))
            .count()
    }

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

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum MtoonPolicyFixture {
        OptionalNative,
        OptionalNativeWithUnlit,
        RequiredNative,
        RequiredNativeWithUnlit,
        OptionalFuture,
        RequiredFuture,
        OptionalUnsupportedUv,
        OptionalUnsupportedUvWithUnlit,
        RequiredUnsupportedUv,
        RequiredUnknownDependency,
    }

    impl MtoonPolicyFixture {
        fn is_required(self) -> bool {
            matches!(
                self,
                Self::RequiredNative
                    | Self::RequiredNativeWithUnlit
                    | Self::RequiredFuture
                    | Self::RequiredUnsupportedUv
                    | Self::RequiredUnknownDependency
            )
        }

        fn has_unlit(self) -> bool {
            matches!(
                self,
                Self::OptionalNativeWithUnlit
                    | Self::RequiredNativeWithUnlit
                    | Self::OptionalUnsupportedUvWithUnlit
            )
        }
    }

    fn generated_mtoon_policy_avatar(fixture: MtoonPolicyFixture) -> tempfile::NamedTempFile {
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
        let human_bones = required_bones
            .iter()
            .enumerate()
            .map(|(node, name)| ((*name).to_owned(), serde_json::json!({"node":node})))
            .collect::<serde_json::Map<_, _>>();
        let nodes = required_bones
            .iter()
            .enumerate()
            .map(|(index, name)| {
                if index == 0 {
                    serde_json::json!({"name":name,"mesh":0})
                } else {
                    serde_json::json!({"name":name})
                }
            })
            .collect::<Vec<_>>();

        let mut bin = Vec::new();
        for value in [-0.75_f32, -0.75, 0.0, 0.75, -0.75, 0.0, 0.0, 0.75, 0.0] {
            bin.extend_from_slice(&value.to_le_bytes());
        }
        for value in [0.0_f32, 0.0, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0] {
            bin.extend_from_slice(&value.to_le_bytes());
        }

        let spec_version = if matches!(
            fixture,
            MtoonPolicyFixture::OptionalFuture | MtoonPolicyFixture::RequiredFuture
        ) {
            "1.1"
        } else {
            "1.0"
        };
        let mut mtoon = serde_json::json!({"specVersion":spec_version});
        if matches!(
            fixture,
            MtoonPolicyFixture::OptionalUnsupportedUv
                | MtoonPolicyFixture::OptionalUnsupportedUvWithUnlit
                | MtoonPolicyFixture::RequiredUnsupportedUv
        ) {
            mtoon["shadeMultiplyTexture"] = serde_json::json!({"index":0,"texCoord":2});
        }
        let mut material_extensions =
            serde_json::Map::from_iter([("VRMC_materials_mtoon".to_owned(), mtoon)]);
        if fixture.has_unlit() {
            material_extensions.insert("KHR_materials_unlit".to_owned(), serde_json::json!({}));
        }
        let mut extensions_used = vec![
            serde_json::json!("VRMC_vrm"),
            serde_json::json!("VRMC_materials_mtoon"),
        ];
        if fixture.has_unlit() {
            extensions_used.push(serde_json::json!("KHR_materials_unlit"));
        }
        let mut extensions_required = vec![serde_json::json!("VRMC_vrm")];
        if fixture.is_required() {
            extensions_required.push(serde_json::json!("VRMC_materials_mtoon"));
        }
        if fixture == MtoonPolicyFixture::RequiredUnknownDependency {
            extensions_used.push(serde_json::json!("X_mtoon_future_dependency"));
            extensions_required.push(serde_json::json!("X_mtoon_future_dependency"));
        }

        let json = serde_json::json!({
            "asset":{"version":"2.0"},
            "scene":0,
            "scenes":[{"nodes":(0..required_bones.len()).collect::<Vec<_>>() }],
            "nodes":nodes,
            "meshes":[{"primitives":[{
                "attributes":{"POSITION":0,"NORMAL":1},
                "material":0
            }]}],
            "materials":[{
                "name":"StageH",
                "doubleSided":true,
                "pbrMetallicRoughness":{"baseColorFactor":[0.8,0.6,0.4,1.0]},
                "extensions":Value::Object(material_extensions)
            }],
            "images":[{"uri":"data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII="}],
            "textures":[{"source":0}],
            "buffers":[{"byteLength":bin.len()}],
            "bufferViews":[
                {"buffer":0,"byteOffset":0,"byteLength":36,"target":34962},
                {"buffer":0,"byteOffset":36,"byteLength":36,"target":34962}
            ],
            "accessors":[
                {"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[-0.75,-0.75,0.0],"max":[0.75,0.75,0.0]},
                {"bufferView":1,"componentType":5126,"count":3,"type":"VEC3"}
            ],
            "extensionsUsed":extensions_used,
            "extensionsRequired":extensions_required,
            "extensions":{"VRMC_vrm":{
                "specVersion":"1.0",
                "meta":{
                    "name":"Stage H policy fixture",
                    "authors":["pocket-character tests"],
                    "licenseUrl":"https://example.invalid/license"
                },
                "humanoid":{"humanBones":human_bones}
            }}
        });
        let mut file = tempfile::Builder::new().suffix(".vrm").tempfile().unwrap();
        file.write_all(&glb_with_json_and_bin(&json, &bin)).unwrap();
        file
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum ConstraintFixture {
        None,
        ValidOptional,
        ValidAimOptional,
        ValidRequired,
        MalformedOptional,
        MalformedRequired,
        CyclicOptional,
        CyclicRequired,
        UnsupportedRequired,
    }

    impl ConstraintFixture {
        fn is_present(self) -> bool {
            self != Self::None
        }

        fn is_required(self) -> bool {
            matches!(
                self,
                Self::ValidRequired
                    | Self::MalformedRequired
                    | Self::CyclicRequired
                    | Self::UnsupportedRequired
            )
        }
    }

    fn set_node_constraint(node: &mut Value, extension: Value) {
        let node = node
            .as_object_mut()
            .expect("fixture node must be an object");
        let extensions = node
            .entry("extensions")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .expect("fixture node extensions must be an object");
        extensions.insert("VRMC_node_constraint".to_owned(), extension);
    }

    fn generated_vrm1_avatar(
        with_spring_bone: bool,
        look_at_kind: Option<&str>,
        constraint_fixture: ConstraintFixture,
    ) -> (tempfile::NamedTempFile, usize, usize, usize) {
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
        if look_at_kind.is_some() {
            for name in ["leftEye", "rightEye"] {
                if let Some(node) = source.humanoid_node(name) {
                    human_bones.insert(name.into(), serde_json::json!({ "node": node }));
                }
            }
        }

        let mut json = source_glb.json;
        let expression_node =
            {
                let nodes = json["nodes"].as_array().unwrap();
                let meshes = json["meshes"].as_array().unwrap();
                let mut mesh_users = std::collections::HashMap::<usize, usize>::new();
                for node in nodes {
                    if let Some(mesh) = node.get("mesh").and_then(Value::as_u64) {
                        *mesh_users.entry(mesh as usize).or_default() += 1;
                    }
                }
                nodes
                    .iter()
                    .enumerate()
                    .find_map(|(node_index, node)| {
                        let mesh = node.get("mesh")?.as_u64()? as usize;
                        if mesh_users.get(&mesh).copied() != Some(1) {
                            return None;
                        }
                        let has_targets =
                            meshes.get(mesh)?.get("primitives")?.as_array()?.iter().any(
                                |primitive| {
                                    primitive
                                        .get("targets")
                                        .and_then(Value::as_array)
                                        .is_some_and(|targets| !targets.is_empty())
                                },
                            );
                        has_targets.then_some(node_index)
                    })
                    .expect("AvatarSample_A should contain one uniquely-instanced morph mesh")
            };
        json["extensions"] = serde_json::json!({
            "VRMC_vrm": {
                "specVersion": "1.0",
                "meta": {
                    "name": "Generated VRM1 test avatar",
                    "authors": ["pocket-character tests"],
                    "licenseUrl": "https://example.invalid/license"
                },
                "humanoid": { "humanBones": human_bones },
                "expressions": {
                    "preset": {
                        "blink": {
                            "morphTargetBinds": [
                                { "node": expression_node, "index": 1, "weight": 1.0 }
                            ]
                        }
                    }
                }
            }
        });
        if let Some(kind) = look_at_kind {
            json["extensions"]["VRMC_vrm"]["lookAt"] = if kind == "malformed" {
                serde_json::json!({ "type": "bone" })
            } else {
                serde_json::json!({
                    "type": kind,
                    "offsetFromHeadBone": [0.0, 0.05, 0.0],
                    "rangeMapHorizontalInner": { "inputMaxValue": 90.0, "outputScale": 10.0 },
                    "rangeMapHorizontalOuter": { "inputMaxValue": 90.0, "outputScale": 12.0 },
                    "rangeMapVerticalDown": { "inputMaxValue": 90.0, "outputScale": 8.0 },
                    "rangeMapVerticalUp": { "inputMaxValue": 90.0, "outputScale": 6.0 }
                })
            };
            if kind == "expression" {
                for (name, target) in [
                    ("lookUp", 1),
                    ("lookDown", 1),
                    ("lookLeft", 1),
                    ("lookRight", 1),
                ] {
                    json["extensions"]["VRMC_vrm"]["expressions"]["preset"][name] = serde_json::json!({
                        "morphTargetBinds": [
                            { "node": expression_node, "index": target, "weight": 1.0 }
                        ]
                    });
                }
            }
        }
        let root_node = source
            .nodes
            .parents
            .iter()
            .position(|&parent| parent == usize::MAX)
            .unwrap();
        let spine_node = source.humanoid_node("spine").unwrap();
        let left_hand_node = source.humanoid_node("leftHand").unwrap();
        let right_hand_node = source.humanoid_node("rightHand").unwrap();
        let nodes = json["nodes"].as_array_mut().unwrap();
        let tail_node = nodes[spine_node]["children"]
            .as_array()
            .and_then(|children| children.first())
            .and_then(Value::as_u64)
            .expect("generated spine should have a child") as usize;
        nodes[root_node]["scale"] = serde_json::json!([0.001, 0.001, 0.001]);
        nodes[spine_node]["rotation"] =
            serde_json::json!(glam::Quat::from_rotation_y(0.17).to_array());
        match constraint_fixture {
            ConstraintFixture::None => {}
            ConstraintFixture::ValidOptional | ConstraintFixture::ValidRequired => {
                set_node_constraint(
                    &mut nodes[left_hand_node],
                    serde_json::json!({
                        "specVersion": "1.0",
                        "constraint": { "rotation": { "source": right_hand_node } }
                    }),
                );
            }
            ConstraintFixture::ValidAimOptional => {
                set_node_constraint(
                    &mut nodes[left_hand_node],
                    serde_json::json!({
                        "specVersion": "1.0",
                        "constraint": {
                            "aim": {
                                "source": right_hand_node,
                                "aimAxis": "PositiveX"
                            }
                        }
                    }),
                );
            }
            ConstraintFixture::MalformedOptional | ConstraintFixture::MalformedRequired => {
                set_node_constraint(
                    &mut nodes[left_hand_node],
                    serde_json::json!({
                        "specVersion": "1.0",
                        "constraint": { "rotation": {} }
                    }),
                );
            }
            ConstraintFixture::CyclicOptional | ConstraintFixture::CyclicRequired => {
                set_node_constraint(
                    &mut nodes[left_hand_node],
                    serde_json::json!({
                        "specVersion": "1.0",
                        "constraint": { "rotation": { "source": right_hand_node } }
                    }),
                );
                set_node_constraint(
                    &mut nodes[right_hand_node],
                    serde_json::json!({
                        "specVersion": "1.0",
                        "constraint": { "rotation": { "source": left_hand_node } }
                    }),
                );
            }
            ConstraintFixture::UnsupportedRequired => {
                set_node_constraint(
                    &mut nodes[left_hand_node],
                    serde_json::json!({
                        "specVersion": "1.0",
                        "constraint": { "futureConstraint": { "source": right_hand_node } }
                    }),
                );
            }
        }

        if with_spring_bone {
            json["extensions"]["VRMC_springBone"] = serde_json::json!({
                "specVersion": "1.0",
                "springs": [{
                    "name": "generated",
                    "joints": [{ "node": spine_node }, { "node": tail_node }]
                }]
            });
        }

        let mut extensions_used = vec![
            serde_json::json!("VRMC_vrm"),
            serde_json::json!("KHR_texture_transform"),
            serde_json::json!("KHR_materials_unlit"),
        ];
        let mut extensions_required = vec![serde_json::json!("VRMC_vrm")];
        if with_spring_bone {
            extensions_used.push(serde_json::json!("VRMC_springBone"));
            extensions_required.push(serde_json::json!("VRMC_springBone"));
        }
        if constraint_fixture.is_present() {
            extensions_used.push(serde_json::json!("VRMC_node_constraint"));
        }
        if constraint_fixture.is_required() {
            extensions_required.push(serde_json::json!("VRMC_node_constraint"));
        }
        json["extensionsUsed"] = Value::Array(extensions_used);
        json["extensionsRequired"] = Value::Array(extensions_required);

        let mut file = tempfile::Builder::new().suffix(".vrm").tempfile().unwrap();
        file.write_all(&glb_with_json_and_bin(&json, source_glb.bin))
            .unwrap();
        (file, root_node, spine_node, expression_node)
    }

    fn prepare_constraint_fixture(
        gpu: &Gpu,
        renderer: &Renderer,
        bundle_path: &Path,
        fixture: ConstraintFixture,
    ) -> std::result::Result<AvatarCandidate, AvatarRuntimeError> {
        let (file, _, _, _) = generated_vrm1_avatar(false, None, fixture);
        AvatarCandidate::prepare(
            gpu,
            renderer,
            bundle_path,
            &AvatarLoadRequest::new(file.path().to_owned(), None, format!("{fixture:?}")),
        )
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
        approx_vec3(
            presentation.model_point_from_world(Vec3::new(-1.0, 2.0, -3.0)),
            Vec3::new(1.0, 2.0, 3.0),
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
                AvatarLoadErrorKind::UnsupportedFile,
                "Drop a .vrm avatar file.",
            ),
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
    fn vrm1_allowlist_accepts_required_native_mtoon_without_unlit() {
        let json = serde_json::json!({
            "extensionsUsed": ["VRMC_materials_mtoon"],
            "extensionsRequired": ["VRMC_materials_mtoon"],
            "materials": [{"extensions": {
                "VRMC_materials_mtoon": {"specVersion": "1.0"}
            }}]
        });
        assert_eq!(
            vrm1_allowed_required_extensions(&json, true, false, false).unwrap(),
            ["VRMC_vrm", "VRMC_materials_mtoon"]
        );
        let error = vrm1_allowed_required_extensions(&json, false, false, false).unwrap_err();
        assert!(format!("{error:#}").contains("supported 1.0 semantics"));
    }

    #[test]
    fn generated_stage_h_optional_required_and_fallback_policy_is_transactional() {
        let gpu = Gpu::new_headless().expect("headless GPU is required for Stage H policy tests");
        let renderer = Renderer::new(&gpu, pocket3d::gpu::OFFSCREEN_FORMAT).unwrap();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let bundle = root.join("dist/character.js");
        let prepare = |fixture| {
            let file = generated_mtoon_policy_avatar(fixture);
            AvatarCandidate::prepare(
                &gpu,
                &renderer,
                &bundle,
                &AvatarLoadRequest::new(file.path().to_owned(), None, format!("{fixture:?}")),
            )
        };

        for fixture in [
            MtoonPolicyFixture::OptionalNative,
            MtoonPolicyFixture::OptionalNativeWithUnlit,
            MtoonPolicyFixture::RequiredNative,
            MtoonPolicyFixture::RequiredNativeWithUnlit,
        ] {
            let candidate = prepare(fixture)
                .unwrap_or_else(|error| panic!("{fixture:?} must use native MToon: {error:#}"));
            assert!(matches!(
                candidate.asset.materials()[0].model,
                pocket3d::material::MaterialModel::Mtoon(_)
            ));
            assert!(candidate.asset.primitives[0].mtoon_bind_group.is_some());
        }

        for (fixture, expected_unlit) in [
            (MtoonPolicyFixture::OptionalFuture, false),
            (MtoonPolicyFixture::OptionalUnsupportedUv, false),
            (MtoonPolicyFixture::OptionalUnsupportedUvWithUnlit, true),
        ] {
            let candidate = prepare(fixture).unwrap_or_else(|error| {
                panic!("{fixture:?} must fall back to the core material: {error:#}")
            });
            let material = &candidate.asset.materials()[0];
            assert_eq!(
                matches!(material.model, pocket3d::material::MaterialModel::Unlit(_)),
                expected_unlit,
                "{fixture:?}"
            );
            assert!(!matches!(
                material.model,
                pocket3d::material::MaterialModel::Mtoon(_)
            ));
            assert!(candidate.asset.primitives[0].mtoon_bind_group.is_none());
        }

        for fixture in [
            MtoonPolicyFixture::RequiredFuture,
            MtoonPolicyFixture::RequiredUnsupportedUv,
            MtoonPolicyFixture::RequiredUnknownDependency,
        ] {
            let error = match prepare(fixture) {
                Ok(_) => panic!("{fixture:?} must be rejected"),
                Err(error) => error,
            };
            assert!(
                error.message().contains("VRMC_materials_mtoon")
                    || error.message().contains("TEXCOORD_2")
                    || error.message().contains("X_mtoon_future_dependency"),
                "{fixture:?}: {error:#}"
            );
        }

        let successful = prepare(MtoonPolicyFixture::RequiredNative).unwrap();
        let old_asset = successful.asset.clone();
        assert!(matches!(
            old_asset.materials()[0].model,
            pocket3d::material::MaterialModel::Mtoon(_)
        ));
        assert!(prepare(MtoonPolicyFixture::RequiredUnsupportedUv).is_err());
        assert!(matches!(
            old_asset.materials()[0].model,
            pocket3d::material::MaterialModel::Mtoon(_)
        ));
    }

    #[test]
    fn generated_stage_i_modes_preserve_policy_and_failed_reload() {
        use super::super::controls::ControlAction;
        use crate::settings::MtoonRenderMode;

        let gpu = Gpu::new_headless().expect("headless GPU is required for Stage I tests");
        let renderer = Renderer::new(&gpu, pocket3d::gpu::OFFSCREEN_FORMAT).unwrap();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let bundle = root.join("dist/character.js");
        for (fixture, expected_unlit) in [
            (MtoonPolicyFixture::RequiredNative, false),
            (MtoonPolicyFixture::RequiredNativeWithUnlit, true),
        ] {
            let file = generated_mtoon_policy_avatar(fixture);
            let mut request = AvatarLoadRequest::new(file.path().to_owned(), None, "StageI");
            request.mtoon_render_mode = MtoonRenderMode::Fallback;
            let candidate = AvatarCandidate::prepare(&gpu, &renderer, &bundle, &request).unwrap();
            let material = &candidate.asset.materials()[0];
            assert_eq!(
                matches!(material.model, pocket3d::material::MaterialModel::Unlit(_)),
                expected_unlit
            );
            assert_eq!(
                matches!(
                    material.model,
                    pocket3d::material::MaterialModel::PocketLit(_)
                ),
                !expected_unlit
            );
            assert!(candidate.asset.primitives[0].mtoon_bind_group.is_none());
        }
        let unsupported = generated_mtoon_policy_avatar(MtoonPolicyFixture::RequiredUnsupportedUv);
        let mut request = AvatarLoadRequest::new(unsupported.path().to_owned(), None, "StageI");
        request.mtoon_render_mode = MtoonRenderMode::Fallback;
        assert!(AvatarCandidate::prepare(&gpu, &renderer, &bundle, &request).is_err());

        let file = generated_mtoon_policy_avatar(MtoonPolicyFixture::OptionalFuture);
        let request = AvatarLoadRequest::new(file.path().to_owned(), None, "StageI");
        let candidate = AvatarCandidate::prepare(&gpu, &renderer, &bundle, &request).unwrap();
        let mut widget = Widget::new(WidgetConfig {
            model_path: file.path().to_owned(),
            vrma_path: PathBuf::new(),
            bundle_path: bundle,
            menu_bundle_path: PathBuf::new(),
            menu_pak_path: PathBuf::new(),
            size: (450, 600),
            cli_max_fps_override: None,
            frames: None,
        });
        widget.commit_avatar_candidate(candidate);
        let old_asset = widget.active_avatar.as_ref().unwrap().asset.clone();
        widget.apply_control_action(ControlAction::SetMtoonRenderMode(MtoonRenderMode::Native));
        assert_eq!(
            widget.pending_avatar_request.as_ref().unwrap().model_path,
            file.path()
        );
        widget.process_pending_avatar_request(&gpu, &renderer);
        assert!(
            widget
                .latest_avatar_load_error()
                .unwrap()
                .message()
                .contains("Native MToon requested")
        );
        assert_eq!(
            widget.avatar_load_status.ui_error_message(),
            Some("Native MToon cannot render this avatar. See log.")
        );
        assert!(Arc::ptr_eq(
            &widget.active_avatar.as_ref().unwrap().asset,
            &old_asset
        ));
        assert!(Arc::ptr_eq(&widget.scene.models[0].asset, &old_asset));
    }

    #[test]
    fn stage_i_live_dual_route_switch_and_fallback_first_upgrade() {
        use super::super::controls::ControlAction;
        use crate::settings::MtoonRenderMode;

        let gpu = Gpu::new_headless().expect("headless GPU is required for Stage I tests");
        let renderer = Renderer::new(&gpu, pocket3d::gpu::OFFSCREEN_FORMAT).unwrap();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let bundle = root.join("dist/character.js");
        let file = generated_mtoon_policy_avatar(MtoonPolicyFixture::RequiredNative);
        let make_widget = || {
            Widget::new(WidgetConfig {
                model_path: file.path().to_owned(),
                vrma_path: PathBuf::new(),
                bundle_path: bundle.clone(),
                menu_bundle_path: PathBuf::new(),
                menu_pak_path: PathBuf::new(),
                size: (450, 600),
                cli_max_fps_override: None,
                frames: None,
            })
        };

        let mut request = AvatarLoadRequest::new(file.path().to_owned(), None, "StageI");
        request.mtoon_render_mode = MtoonRenderMode::Native;
        let candidate = AvatarCandidate::prepare(&gpu, &renderer, &bundle, &request).unwrap();
        let mut widget = make_widget();
        widget.commit_avatar_candidate(candidate);
        let asset = widget.active_avatar.as_ref().unwrap().asset.clone();
        let original_instance = &widget.scene.models[0] as *const ModelInstance;
        widget.scene.models[0].pose = Some(vec![Mat4::IDENTITY]);
        widget.scene.models[0]
            .materials
            .get_mut(0)
            .unwrap()
            .base_color_factor = [0.3, 0.4, 0.5, 0.6];
        let original_material = widget.scene.models[0].materials.get(0).unwrap().clone();
        widget.scene.time = 4.25;
        widget.active_avatar.as_mut().unwrap().animation.clip_time = 4.25;
        let original_expressions = widget.active_avatar.as_ref().unwrap().expressions.clone();
        for mode in [
            MtoonRenderMode::Fallback,
            MtoonRenderMode::Native,
            MtoonRenderMode::Fallback,
        ] {
            widget.apply_control_action(ControlAction::SetMtoonRenderMode(mode));
            assert!(
                widget.pending_avatar_request.is_none(),
                "dual-route switch queued a reload"
            );
            assert_eq!(widget.avatar_prepare_count, 0);
            assert!(Arc::ptr_eq(&widget.scene.models[0].asset, &asset));
            assert_eq!(
                &widget.scene.models[0] as *const ModelInstance,
                original_instance
            );
            assert_eq!(widget.scene.models[0].pose, Some(vec![Mat4::IDENTITY]));
            assert_eq!(
                widget.scene.models[0].materials.get(0).unwrap(),
                &original_material
            );
            assert_eq!(
                widget.active_avatar.as_ref().unwrap().expressions,
                original_expressions
            );
            assert_eq!(
                widget.active_avatar.as_ref().unwrap().animation.clip_time,
                4.25
            );
            assert_eq!(widget.scene.time, 4.25);
            assert_eq!(
                widget.scene.models[0].asset.primitives[0]
                    .draw_route(widget.scene.models[0].mtoon_draw_route)
                    .native,
                mode == MtoonRenderMode::Native
            );
        }
        let mut switch_times = Vec::new();
        for index in 0..20 {
            let mode = if index % 2 == 0 {
                MtoonRenderMode::Native
            } else {
                MtoonRenderMode::Fallback
            };
            let started = std::time::Instant::now();
            widget.apply_control_action(ControlAction::SetMtoonRenderMode(mode));
            switch_times.push(started.elapsed());
            assert_eq!(widget.avatar_prepare_count, 0);
        }
        switch_times.sort();
        eprintln!(
            "dual-route settings switch median: {:?}",
            switch_times[switch_times.len() / 2]
        );

        let mut fallback_request = request;
        fallback_request.mtoon_render_mode = MtoonRenderMode::Fallback;
        let fallback =
            AvatarCandidate::prepare(&gpu, &renderer, &bundle, &fallback_request).unwrap();
        let mut fallback_widget = make_widget();
        fallback_widget.commit_avatar_candidate(fallback);
        let old_asset = fallback_widget
            .active_avatar
            .as_ref()
            .unwrap()
            .asset
            .clone();
        assert_eq!(old_asset.native_mtoon_material_count, 0);
        assert_eq!(old_asset.native_mtoon_eligible_count, 1);
        assert!(
            !fallback_widget
                .active_avatar
                .as_ref()
                .unwrap()
                .has_route(MtoonRenderMode::Auto)
        );
        fallback_widget
            .apply_control_action(ControlAction::SetMtoonRenderMode(MtoonRenderMode::Native));
        assert!(fallback_widget.pending_avatar_request.is_some());
        assert!(Arc::ptr_eq(
            &fallback_widget.scene.models[0].asset,
            &old_asset
        ));
        fallback_widget.process_pending_avatar_request(&gpu, &renderer);
        assert_eq!(fallback_widget.avatar_prepare_count, 1);
        assert!(fallback_widget.pending_avatar_request.is_none());
        let upgraded = fallback_widget
            .active_avatar
            .as_ref()
            .unwrap()
            .asset
            .clone();
        assert!(!Arc::ptr_eq(&upgraded, &old_asset));
        assert_eq!(upgraded.native_mtoon_material_count, 1);
        assert!(
            fallback_widget
                .active_avatar
                .as_ref()
                .unwrap()
                .has_route(MtoonRenderMode::Auto)
        );
        fallback_widget
            .apply_control_action(ControlAction::SetMtoonRenderMode(MtoonRenderMode::Fallback));
        assert!(fallback_widget.pending_avatar_request.is_none());
        assert_eq!(fallback_widget.avatar_prepare_count, 1);
        assert!(Arc::ptr_eq(
            &fallback_widget.scene.models[0].asset,
            &upgraded
        ));
    }

    #[test]
    fn local_avatar_sample_dual_route_switch_latency() {
        use super::super::controls::ControlAction;
        use crate::menu_guest::MenuAction;
        use crate::settings::MtoonRenderMode;

        let fixture = Path::new(r"C:\Users\Breeze\Downloads\AvatarSample_VRM1.0.vrm");
        if !fixture.is_file() {
            eprintln!(
                "skipping local route latency fixture: {}",
                fixture.display()
            );
            return;
        }
        let gpu = Gpu::new_headless().expect("headless GPU is required for route latency test");
        let renderer = Renderer::new(&gpu, pocket3d::gpu::OFFSCREEN_FORMAT).unwrap();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let bundle = root.join("dist/character.js");
        let mut request = AvatarLoadRequest::new(fixture.to_owned(), None, "AvatarSample");
        request.mtoon_render_mode = MtoonRenderMode::Native;
        let candidate = AvatarCandidate::prepare(&gpu, &renderer, &bundle, &request).unwrap();
        let mut widget = Widget::new(WidgetConfig {
            model_path: fixture.to_owned(),
            vrma_path: PathBuf::new(),
            bundle_path: bundle,
            menu_bundle_path: PathBuf::new(),
            menu_pak_path: PathBuf::new(),
            size: (450, 600),
            cli_max_fps_override: None,
            frames: None,
        });
        widget.commit_avatar_candidate(candidate);
        let asset = widget.active_avatar.as_ref().unwrap().asset.clone();
        assert!(asset.native_mtoon_material_count > 0);
        let expression_index = match &widget.active_avatar.as_ref().unwrap().expressions {
            ResolvedExpressionRuntime::Vrm1(runtime) => {
                let state = runtime.manual_state();
                assert_eq!(
                    state.len(),
                    14,
                    "AvatarSample should expose all authored expressions"
                );
                state
                    .iter()
                    .find(|expression| expression.name == "happy")
                    .unwrap()
                    .index
            }
            ResolvedExpressionRuntime::Vrm0Legacy => panic!("AvatarSample must be VRM1"),
        };
        widget.apply_menu_action(MenuAction::SetExpression(
            widget.avatar_generation,
            expression_index,
            0.7,
        ));
        widget.apply_menu_action(MenuAction::SetExpression(
            widget.avatar_generation.wrapping_sub(1),
            expression_index,
            1.0,
        ));
        let happy_bind = match &widget.active_avatar.as_ref().unwrap().expressions {
            ResolvedExpressionRuntime::Vrm1(runtime) => runtime.expressions[expression_index]
                .morph_binds
                .first()
                .map(|bind| (bind.mesh_slot, bind.target)),
            ResolvedExpressionRuntime::Vrm0Legacy => None,
        }
        .expect("AvatarSample happy must have a morph bind");
        let manual_morph_weight = widget.scene.models[0]
            .morph
            .as_ref()
            .unwrap()
            .weight(happy_bind.0, happy_bind.1);
        assert!(
            manual_morph_weight > 0.0,
            "manual happy must reach the existing morph evaluator"
        );
        let mut samples = Vec::new();
        for index in 0..40 {
            let mode = if index % 2 == 0 {
                MtoonRenderMode::Fallback
            } else {
                MtoonRenderMode::Native
            };
            let started = std::time::Instant::now();
            widget.apply_control_action(ControlAction::SetMtoonRenderMode(mode));
            samples.push(started.elapsed());
            assert_eq!(widget.avatar_prepare_count, 0);
            assert!(widget.pending_avatar_request.is_none());
            assert!(Arc::ptr_eq(&widget.scene.models[0].asset, &asset));
            let ResolvedExpressionRuntime::Vrm1(runtime) =
                &widget.active_avatar.as_ref().unwrap().expressions
            else {
                unreachable!()
            };
            assert_eq!(
                runtime
                    .manual_state()
                    .iter()
                    .find(|expression| expression.name == "happy")
                    .unwrap()
                    .weight,
                0.7
            );
        }
        widget.apply_menu_action(MenuAction::ResetExpressions(widget.avatar_generation));
        let ResolvedExpressionRuntime::Vrm1(runtime) =
            &widget.active_avatar.as_ref().unwrap().expressions
        else {
            unreachable!()
        };
        assert_eq!(
            runtime
                .manual_state()
                .iter()
                .find(|expression| expression.name == "happy")
                .unwrap()
                .weight,
            0.0
        );
        assert!(
            widget.scene.models[0]
                .morph
                .as_ref()
                .unwrap()
                .weight(happy_bind.0, happy_bind.1)
                < manual_morph_weight
        );
        samples.sort();
        eprintln!(
            "AvatarSample dual-route settings switch median: {:?}",
            samples[samples.len() / 2]
        );
    }

    #[test]
    fn vrm1_allowlist_only_claims_validated_optional_runtimes_when_required() {
        let json = serde_json::json!({"materials": []});
        assert_eq!(
            vrm1_allowed_required_extensions(&json, false, false, false).unwrap(),
            ["VRMC_vrm"]
        );

        let json = serde_json::json!({
            "extensionsUsed": ["VRMC_node_constraint"],
            "extensionsRequired": ["VRMC_node_constraint"]
        });
        assert!(vrm1_allowed_required_extensions(&json, false, false, false).is_err());
        assert_eq!(
            vrm1_allowed_required_extensions(&json, false, false, true).unwrap(),
            ["VRMC_vrm", "VRMC_node_constraint"]
        );
    }

    #[test]
    fn vrm1_allowlist_accepts_spring_only_after_semantic_parse() {
        let json = serde_json::json!({
            "extensions": {"VRMC_springBone": {"specVersion": "1.0"}}
        });
        assert!(vrm1_allowed_required_extensions(&json, false, false, false).is_err());
        assert_eq!(
            vrm1_allowed_required_extensions(&json, false, true, false).unwrap(),
            ["VRMC_vrm", "VRMC_springBone"]
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
        assert!(
            success
                .asset
                .primitives
                .iter()
                .all(|primitive| primitive.mtoon_bind_group.is_none())
        );
        let mut renderer = renderer;
        let changed = render_avatar_smoke(
            &gpu,
            &mut renderer,
            success.asset.clone(),
            "mtoon-stage-b-legacy-regression.ppm",
        );
        assert!(
            changed > 500,
            "legacy avatar frame should contain a visible avatar"
        );

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
    fn local_vrm1_mtoon_stage_f_preserves_all_native_routes() {
        let fixture = Path::new(r"C:\Users\Breeze\Downloads\AvatarSample_VRM1.0.vrm");
        if !fixture.is_file() {
            eprintln!(
                "skipping local VRM1 MToon smoke fixture: {} is unavailable",
                fixture.display()
            );
            return;
        }

        let gpu = Gpu::new_headless().expect("headless GPU is required for MToon smoke");
        let renderer = Renderer::new(&gpu, pocket3d::gpu::OFFSCREEN_FORMAT).unwrap();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let request = AvatarLoadRequest::new(fixture.to_owned(), None, "AvatarSample_VRM1");
        let candidate =
            AvatarCandidate::prepare(&gpu, &renderer, &root.join("dist/character.js"), &request)
                .expect("VRM1 MToon fixture must prepare");
        let AvatarSemanticDocument::Vrm1(document) = &candidate.document else {
            panic!("preferred fixture must parse as VRM 1.0");
        };
        assert_eq!(document.expressions.len(), 14);
        assert_eq!(
            document
                .expressions
                .iter()
                .map(|expression| expression.material_color_binds.len())
                .sum::<usize>(),
            0
        );
        assert_eq!(
            document
                .expressions
                .iter()
                .map(|expression| expression.texture_transform_binds.len())
                .sum::<usize>(),
            0
        );
        let authored = candidate.asset.materials();
        assert!(!authored.is_empty());
        assert!(
            authored
                .iter()
                .all(|material| { material.kind() == pocket3d::material::MaterialKind::Mtoon })
        );
        let native = candidate
            .asset
            .primitives
            .iter()
            .filter(|primitive| primitive.mtoon_bind_group.is_some())
            .count();
        let fallback = candidate.asset.primitives.len() - native;
        eprintln!("real VRM1 Stage F primitives: native {native}, fallback {fallback}");
        let native_materials: std::collections::HashSet<usize> = candidate
            .asset
            .primitives
            .iter()
            .filter(|primitive| primitive.mtoon_bind_group.is_some())
            .filter_map(|primitive| primitive.material_index)
            .collect();
        assert_eq!(native_materials, (0..15).collect());
        assert_eq!((native, fallback), (15, 0));
        let fallback_materials: std::collections::HashSet<usize> = candidate
            .asset
            .primitives
            .iter()
            .filter(|primitive| primitive.mtoon_bind_group.is_none())
            .filter_map(|primitive| primitive.material_index)
            .collect();
        assert!(fallback_materials.is_empty());
        let outlined = [3, 7, 8, 9, 10];
        let outlined_primitives: std::collections::HashSet<usize> = candidate
            .asset
            .primitives
            .iter()
            .filter(|primitive| primitive.mtoon_outline)
            .filter_map(|primitive| primitive.material_index)
            .collect();
        assert_eq!(outlined_primitives, outlined.into_iter().collect());
        for &index in &outlined {
            let pocket3d::material::MaterialModel::Mtoon(mtoon) = &authored[index].model else {
                unreachable!()
            };
            assert_eq!(
                mtoon.outline_width_mode,
                pocket3d::material::MtoonOutlineWidthMode::WorldCoordinates
            );
            assert!((mtoon.outline_width_factor - 0.00075).abs() < 1e-8);
            assert_eq!(
                mtoon.outline_color_factor,
                [0.061246056, 0.008568122, 0.014443838]
            );
            assert_eq!(mtoon.outline_lighting_mix_factor, 0.0);
            assert_eq!(
                authored[index].inputs.alpha_mode,
                pocket3d::material::MaterialAlphaMode::Mask
            );
            assert_eq!(authored[index].inputs.alpha_cutoff, 0.5);
            assert_eq!(mtoon.render_queue_offset_number, 0);
            assert!(!mtoon.transparent_with_z_write);
            assert_eq!(mtoon.uv_animation_scroll_x_speed_factor, 0.0);
            assert_eq!(mtoon.uv_animation_scroll_y_speed_factor, 0.0);
            assert_eq!(mtoon.uv_animation_rotation_speed_factor, 0.0);
            if index == 3 {
                let width = mtoon.outline_width_multiply_texture.as_ref().unwrap();
                assert_eq!(width.texture_index, 26);
                assert_eq!(width.effective_tex_coord(), 0);
                assert_eq!(
                    width.transform,
                    pocket3d::material::TextureTransform::default()
                );
                assert_eq!(
                    width.sampler.mag_filter,
                    Some(pocket3d::material::GltfMagFilter::Linear)
                );
                assert_eq!(
                    width.sampler.min_filter,
                    Some(pocket3d::material::GltfMinFilter::Linear)
                );
            } else {
                assert!(mtoon.outline_width_multiply_texture.is_none());
            }
        }
        assert_eq!(authored.len(), 15);
        assert_eq!(
            authored
                .iter()
                .filter(|material| {
                    material.inputs.alpha_mode == pocket3d::material::MaterialAlphaMode::Blend
                })
                .count(),
            4
        );
        assert_eq!(
            authored
                .iter()
                .filter(|material| {
                    matches!(
                        &material.model,
                        pocket3d::material::MaterialModel::Mtoon(mtoon)
                            if mtoon.outline_width_mode != pocket3d::material::MtoonOutlineWidthMode::None
                    )
                })
                .count(),
            5
        );
        let blend_routes: std::collections::BTreeMap<
            usize,
            (i32, pocket3d::material::RenderPhase),
        > = candidate
            .asset
            .primitives
            .iter()
            .filter(|primitive| {
                primitive.alpha_mode == pocket3d::material::MaterialAlphaMode::Blend
            })
            .map(|primitive| {
                (
                    primitive.material_index.unwrap(),
                    (primitive.render_queue_offset, primitive.render_phase),
                )
            })
            .collect();
        assert_eq!(
            blend_routes,
            [
                (1, (-2, pocket3d::material::RenderPhase::Blend)),
                (2, (-1, pocket3d::material::RenderPhase::Blend)),
                (5, (0, pocket3d::material::RenderPhase::Blend)),
                (6, (-2, pocket3d::material::RenderPhase::Blend)),
            ]
            .into_iter()
            .collect()
        );
        assert!(candidate.asset.primitives.iter().all(|primitive| {
            primitive.alpha_mode != pocket3d::material::MaterialAlphaMode::Blend
                || primitive.mtoon_bind_group.is_some()
        }));
        for (material_index, (queue, phase)) in &blend_routes {
            eprintln!(
                "preferred Stage D BLEND material {material_index}: queue {queue}, transparentWithZWrite false, phase {phase:?}"
            );
        }
        let mut renderer = renderer;
        let changed = render_avatar_smoke(
            &gpu,
            &mut renderer,
            candidate.asset.clone(),
            "mtoon-stage-f-preferred.ppm",
        );
        eprintln!("preferred VRM1 Stage F pixels distinct from clear: {changed}");
        assert!(
            changed > 500,
            "Stage F VRM1 frame should contain a visible avatar"
        );

        // This preferred avatar authors zero UV-animation speeds on all 15
        // materials, so fixed-time Stage F rendering must be pixel-stable.
        let target = pocket3d::gpu::OffscreenTarget::new(&gpu, 256, 256);
        let (min, max) = candidate.asset.aabb;
        let center = (min + max) * 0.5;
        let radius = (max - min).length().max(1.0);
        let camera = pocket3d::camera::Camera {
            pos: center + Vec3::new(0.0, 0.0, radius * 1.5),
            fov_y: 45.0_f32.to_radians(),
            znear: 0.01,
            zfar: radius * 10.0,
            ..Default::default()
        };
        let mut scene = pocket3d::scene::Scene::default();
        scene
            .models
            .push(pocket3d::model::ModelInstance::new(candidate.asset));
        let frames = [0.0, 1000.0].map(|time| {
            scene.time = time;
            renderer.render(
                &gpu,
                &target.view,
                target.size,
                &scene,
                &camera,
                &pocket3d::hud::Hud::default(),
            );
            target.read_rgba(&gpu).unwrap()
        });
        assert_eq!(frames[0], frames[1]);
    }

    #[test]
    fn local_seed_san_stage_g_texture_expression_round_trips_to_authored_state() {
        let fixture = Path::new(r"C:\Users\Breeze\Downloads\Seed-san.vrm");
        if !fixture.is_file() {
            eprintln!(
                "skipping local Seed-san Stage G fixture: {} is unavailable",
                fixture.display()
            );
            return;
        }

        let gpu = Gpu::new_headless().expect("headless GPU is required for Seed-san Stage G");
        let renderer = Renderer::new(&gpu, pocket3d::gpu::OFFSCREEN_FORMAT).unwrap();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let request = AvatarLoadRequest::new(fixture.to_owned(), None, "Seed-san");
        let candidate =
            AvatarCandidate::prepare(&gpu, &renderer, &root.join("dist/character.js"), &request)
                .expect("Seed-san VRM1 fixture must prepare");

        let AvatarSemanticDocument::Vrm1(document) = &candidate.document else {
            panic!("Seed-san must parse as VRM 1.0");
        };
        assert_eq!(document.expressions.len(), 18);
        assert_eq!(
            document
                .expressions
                .iter()
                .map(|expression| expression.material_color_binds.len())
                .sum::<usize>(),
            0
        );
        let transform_expressions = document
            .expressions
            .iter()
            .filter(|expression| !expression.texture_transform_binds.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(transform_expressions.len(), 5);
        assert_eq!(
            transform_expressions
                .iter()
                .map(|expression| expression.name.as_str())
                .collect::<std::collections::BTreeSet<_>>(),
            ["angry", "happy", "relaxed", "sad", "surprised"]
                .into_iter()
                .collect()
        );
        assert!(transform_expressions.iter().all(|expression| {
            !expression.morph_target_binds.is_empty()
                && expression.texture_transform_binds.len() == 1
                && expression.texture_transform_binds[0].material == 11
        }));

        let ResolvedExpressionRuntime::Vrm1(runtime) = &candidate.expressions else {
            unreachable!()
        };
        let happy = runtime
            .expressions
            .iter()
            .find(|expression| expression.name == "happy")
            .expect("Seed-san happy expression must resolve");
        let happy_index = runtime
            .expressions
            .iter()
            .position(|expression| expression.name == "happy")
            .unwrap();
        assert!(happy.is_binary);
        assert!(!happy.morph_binds.is_empty());
        assert_eq!(happy.texture_transform_binds.len(), 1);
        assert_eq!(happy.texture_transform_binds[0].material, 11);
        assert_eq!(happy.texture_transform_binds[0].scale, [1.0, 1.0]);
        assert_eq!(happy.texture_transform_binds[0].offset, [0.25, 0.0]);
        let morph_target = happy
            .morph_binds
            .first()
            .map(|bind| (bind.mesh_slot, bind.target, bind.weight))
            .unwrap();

        let slot = AvatarSceneSlot::new(0);
        let (instance, mut active) = candidate.into_parts(slot);
        let authored = instance.materials.get(11).unwrap().clone();
        assert!(!authored.texture_transforms.is_empty());
        let mut scene = Scene::default();
        scene.models.push(instance);
        let ResolvedExpressionRuntime::Vrm1(runtime) = &mut active.expressions else {
            unreachable!()
        };
        assert!(runtime.set_manual_input(happy_index, 1.0));
        runtime.compose_if_needed(&mut scene, slot, 0.0);
        let composed = scene.models[0].materials.get(11).unwrap();
        let mut transformed_roles = 0;
        for (&role, authored_transform) in &authored.texture_transforms {
            let composed_transform = composed.texture_transforms.get(&role).unwrap();
            if role == pocket3d::material::TextureRole::Matcap {
                assert_eq!(composed_transform, authored_transform);
                continue;
            }
            transformed_roles += 1;
            assert_eq!(composed_transform.scale, [1.0, 1.0]);
            assert_eq!(composed_transform.offset, [0.25, 0.0]);
            assert_eq!(composed_transform.rotation, authored_transform.rotation);
            assert_eq!(
                composed_transform.tex_coord_override,
                authored_transform.tex_coord_override
            );
        }
        assert!(transformed_roles > 0);
        assert_eq!(
            scene.models[0]
                .morph
                .as_ref()
                .unwrap()
                .weight(morph_target.0, morph_target.1),
            morph_target.2
        );

        assert!(runtime.reset_manual_inputs());
        runtime.compose_if_needed(&mut scene, slot, 0.0);
        assert_eq!(scene.models[0].materials.get(11).unwrap(), &authored);
    }

    #[test]
    fn official_mtoon_stage_f_fixture_routes_and_animates_all_three_quads() {
        let fixture = std::env::var_os("MTOON_STAGE_F_FIXTURE")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::temp_dir().join("VRMC_materials_mtoon_UV_Animation_Test.vrm")
            });
        if !fixture.is_file() {
            eprintln!(
                "skipping official MToon Stage F fixture: {} is unavailable",
                fixture.display()
            );
            return;
        }

        let gpu = Gpu::new_headless().expect("headless GPU is required for MToon Stage F");
        let mut renderer = Renderer::new(&gpu, pocket3d::gpu::OFFSCREEN_FORMAT).unwrap();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let request = AvatarLoadRequest::new(fixture, None, "MToonUvAnimationTest");
        let candidate =
            AvatarCandidate::prepare(&gpu, &renderer, &root.join("dist/character.js"), &request)
                .expect("official MToon UV Animation Test must prepare");

        let authored = candidate.asset.materials();
        assert_eq!(authored.len(), 3);
        let expected = [
            ("ScrollX", [1.0, 0.0, 0.0]),
            ("ScrollY", [0.0, 1.0, 0.0]),
            ("ScrollRot", [0.0, 0.0, 1.0]),
        ];
        for (material, (name, speeds)) in authored.iter().zip(expected) {
            assert_eq!(material.name.as_deref(), Some(name));
            let pocket3d::material::MaterialModel::Mtoon(mtoon) = &material.model else {
                panic!("official Stage F fixture material must be typed MToon")
            };
            assert_eq!(
                [
                    mtoon.uv_animation_scroll_x_speed_factor,
                    mtoon.uv_animation_scroll_y_speed_factor,
                    mtoon.uv_animation_rotation_speed_factor,
                ],
                speeds
            );
        }
        let native = candidate
            .asset
            .primitives
            .iter()
            .filter(|primitive| primitive.mtoon_bind_group.is_some())
            .count();
        assert_eq!((native, candidate.asset.primitives.len() - native), (3, 0));

        let target = pocket3d::gpu::OffscreenTarget::new(&gpu, 384, 768);
        let camera = pocket3d::camera::Camera {
            pos: Vec3::new(0.0, 0.0, 5.0),
            fov_y: 45.0_f32.to_radians(),
            znear: 0.01,
            zfar: 50.0,
            ..Default::default()
        };
        let mut scene = pocket3d::scene::Scene::default();
        scene
            .models
            .push(pocket3d::model::ModelInstance::new(candidate.asset));
        let frames = [0.0, 0.5, 1.0].map(|time| {
            scene.time = time;
            renderer.render(
                &gpu,
                &target.view,
                target.size,
                &scene,
                &camera,
                &pocket3d::hud::Hud::default(),
            );
            target.read_rgba(&gpu).unwrap()
        });
        let changed_in_band = |a: &[u8], b: &[u8], y0: usize, y1: usize| {
            (y0..y1)
                .flat_map(|y| (0..target.size.0 as usize).map(move |x| (x, y)))
                .filter(|&(x, y)| {
                    let offset = (y * target.size.0 as usize + x) * 4;
                    a[offset..offset + 4] != b[offset..offset + 4]
                })
                .count()
        };
        for (label, y0, y1) in [
            ("rotation", 96, 192),
            ("scroll-y", 192, 288),
            ("scroll-x", 288, 384),
        ] {
            let first = changed_in_band(&frames[0], &frames[1], y0, y1);
            let second = changed_in_band(&frames[1], &frames[2], y0, y1);
            eprintln!("official Stage F {label}: changed pixels {first}/{second}");
            assert!(first > 100 && second > 100, "{label} quad must animate");
        }
    }

    #[test]
    fn local_real_vrm1_eligible_mtoon_preserves_native_routes_through_stage_e() {
        let fixture = Path::new(r"C:\Users\Breeze\Downloads\VRM1_Constraint_Twist_Sample.vrm");
        if !fixture.is_file() {
            eprintln!(
                "skipping local native MToon smoke fixture: {}",
                fixture.display()
            );
            return;
        }
        let gpu = Gpu::new_headless().expect("headless GPU is required for MToon smoke");
        let renderer = Renderer::new(&gpu, pocket3d::gpu::OFFSCREEN_FORMAT).unwrap();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let request =
            AvatarLoadRequest::new(fixture.to_owned(), None, "VRM1_Constraint_Twist_Sample");
        let candidate =
            AvatarCandidate::prepare(&gpu, &renderer, &root.join("dist/character.js"), &request)
                .expect("real VRM1 MToon fixture must prepare");
        let native = candidate
            .asset
            .primitives
            .iter()
            .filter(|primitive| primitive.mtoon_bind_group.is_some())
            .count();
        let fallback = candidate.asset.primitives.len() - native;
        eprintln!("eligible real VRM1 Stage E primitives: native {native}, fallback {fallback}");
        assert_eq!((native, fallback), (13, 0));
        assert!(candidate.asset.primitives.iter().all(|primitive| {
            primitive.mtoon_bind_group.is_none()
                || primitive.alpha_mode != pocket3d::material::MaterialAlphaMode::Blend
        }));
        let mut renderer = renderer;
        let changed = render_avatar_smoke(
            &gpu,
            &mut renderer,
            candidate.asset.clone(),
            "mtoon-stage-b-native.ppm",
        );
        eprintln!("native VRM1 rendered pixels distinct from clear: {changed}");
        assert!(
            changed > 500,
            "native MToon frame should contain a visible avatar"
        );
    }

    #[test]
    fn local_real_vrm1_native_mtoon_samples_uv1_and_texture_transform_override() {
        let fixture = Path::new(r"C:\Users\Breeze\Downloads\VRM1_Constraint_Twist_Sample.vrm");
        if !fixture.is_file() {
            eprintln!(
                "skipping local UV1 MToon smoke fixture: {}",
                fixture.display()
            );
            return;
        }
        let gpu = Gpu::new_headless().expect("headless GPU is required for MToon UV smoke");
        let renderer = Renderer::new(&gpu, pocket3d::gpu::OFFSCREEN_FORMAT).unwrap();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let bundle = root.join("dist/character.js");
        let original = AvatarCandidate::prepare(
            &gpu,
            &renderer,
            &bundle,
            &AvatarLoadRequest::new(fixture.to_owned(), None, "VRM1_UV_Source"),
        )
        .unwrap();
        let native_indices: std::collections::HashSet<usize> = original
            .asset
            .primitives
            .iter()
            .filter(|primitive| primitive.mtoon_bind_group.is_some())
            .filter_map(|primitive| primitive.material_index)
            .collect();
        assert!(!native_indices.is_empty());

        let source = std::fs::read(fixture).unwrap();
        let json_length = u32::from_le_bytes(source[12..16].try_into().unwrap()) as usize;
        assert_eq!(&source[16..20], b"JSON");
        let json_end = 20 + json_length;
        let mut json: Value = serde_json::from_slice(&source[20..json_end]).unwrap();
        let bin_length =
            u32::from_le_bytes(source[json_end..json_end + 4].try_into().unwrap()) as usize;
        assert_eq!(&source[json_end + 4..json_end + 8], b"BIN\0");
        let bin = &source[json_end + 8..json_end + 8 + bin_length];
        for mesh in json["meshes"].as_array_mut().unwrap() {
            for primitive in mesh["primitives"].as_array_mut().unwrap() {
                if let Some(uv0) = primitive["attributes"]["TEXCOORD_0"].as_u64() {
                    // Reuse authored UV0 values as UV1 to isolate selection from geometry.
                    primitive["attributes"]["TEXCOORD_1"] = serde_json::json!(uv0);
                }
            }
        }
        for (index, material) in json["materials"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .enumerate()
        {
            if !native_indices.contains(&index) {
                continue;
            }
            let base = &mut material["pbrMetallicRoughness"]["baseColorTexture"];
            if base.is_object() {
                base["texCoord"] = serde_json::json!(1);
            }
            let shade = &mut material["extensions"]["VRMC_materials_mtoon"]["shadeMultiplyTexture"];
            if shade.is_object() {
                shade["texCoord"] = serde_json::json!(0);
                shade["extensions"]["KHR_texture_transform"] = serde_json::json!({
                    "texCoord": 1,
                    "offset": [0.015, 0.025],
                    "rotation": 0.1,
                    "scale": [0.98, 1.02]
                });
            }
        }
        json["extensionsUsed"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!("KHR_texture_transform"));
        let mut adapted = tempfile::Builder::new().suffix(".vrm").tempfile().unwrap();
        adapted
            .as_file_mut()
            .write_all(&glb_with_json_and_bin(&json, bin))
            .unwrap();
        let candidate = AvatarCandidate::prepare(
            &gpu,
            &renderer,
            &bundle,
            &AvatarLoadRequest::new(adapted.path().to_owned(), None, "VRM1_UV1_Transform"),
        )
        .expect("real VRM1 with UV1 and KHR transform must prepare");
        let adapted_native = candidate
            .asset
            .primitives
            .iter()
            .filter(|primitive| primitive.mtoon_bind_group.is_some())
            .count();
        let original_native = original
            .asset
            .primitives
            .iter()
            .filter(|primitive| primitive.mtoon_bind_group.is_some())
            .count();
        assert_eq!(adapted_native, original_native);
        for index in native_indices {
            let material = &candidate.asset.materials()[index];
            let base = material.inputs.base_color_texture.as_ref().unwrap();
            assert_eq!(base.effective_tex_coord(), 1);
            let pocket3d::material::MaterialModel::Mtoon(mtoon) = &material.model else {
                panic!("native material must retain typed MToon descriptor");
            };
            let shade = mtoon.shade_multiply_texture.as_ref().unwrap();
            assert_eq!(shade.tex_coord, 0);
            assert_eq!(shade.effective_tex_coord(), 1);
            assert_eq!(shade.transform.offset, [0.015, 0.025]);
        }
        let mut renderer = renderer;
        let changed = render_avatar_smoke(
            &gpu,
            &mut renderer,
            candidate.asset.clone(),
            "mtoon-stage-b-native-uv1.ppm",
        );
        assert!(
            changed > 500,
            "native UV1 frame should contain a visible avatar"
        );
    }

    #[test]
    fn local_real_vrm1_mask_normal_map_uses_native_stage_b() {
        let fixture = Path::new(r"C:\Users\Breeze\Downloads\5447297406763866907.vrm");
        if !fixture.is_file() {
            eprintln!(
                "skipping local MASK MToon smoke fixture: {}",
                fixture.display()
            );
            return;
        }
        let gpu = Gpu::new_headless().expect("headless GPU is required for MToon smoke");
        let renderer = Renderer::new(&gpu, pocket3d::gpu::OFFSCREEN_FORMAT).unwrap();
        let source = std::fs::read(fixture).unwrap();
        let json_length = u32::from_le_bytes(source[12..16].try_into().unwrap()) as usize;
        assert_eq!(&source[16..20], b"JSON");
        let json_end = 20 + json_length;
        let mut json: Value = serde_json::from_slice(&source[20..json_end]).unwrap();
        let bin_length =
            u32::from_le_bytes(source[json_end..json_end + 4].try_into().unwrap()) as usize;
        assert_eq!(&source[json_end + 4..json_end + 8], b"BIN\0");
        let bin = &source[json_end + 8..json_end + 8 + bin_length];
        let mask_material = json["materials"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|material| {
                material["name"]
                    .as_str()
                    .is_some_and(|name| name.contains("HairBack"))
            })
            .unwrap();
        // Only remove out-of-scope emission; the MASK and normal-map source is
        // otherwise authored exactly as in the real model.
        mask_material["emissiveFactor"] = serde_json::json!([0.0, 0.0, 0.0]);
        mask_material
            .as_object_mut()
            .unwrap()
            .remove("emissiveTexture");
        let mut adapted = tempfile::Builder::new().suffix(".vrm").tempfile().unwrap();
        adapted
            .as_file_mut()
            .write_all(&glb_with_json_and_bin(&json, bin))
            .unwrap();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let request = AvatarLoadRequest::new(adapted.path().to_owned(), None, "VRM1_Mask_Normal");
        let candidate =
            AvatarCandidate::prepare(&gpu, &renderer, &root.join("dist/character.js"), &request)
                .expect("real VRM1 MASK MToon fixture must prepare");
        let native_mask = candidate
            .asset
            .primitives
            .iter()
            .filter(|primitive| {
                primitive.mtoon_bind_group.is_some()
                    && primitive.alpha_mode == pocket3d::material::MaterialAlphaMode::Mask
            })
            .count();
        eprintln!("real VRM1 native MASK primitives: {native_mask}");
        assert!(
            native_mask > 0,
            "real fixture should exercise native MASK with normal map"
        );
        let mut renderer = renderer;
        let changed = render_avatar_smoke(
            &gpu,
            &mut renderer,
            candidate.asset.clone(),
            "mtoon-stage-b-native-mask.ppm",
        );
        eprintln!("native MASK frame pixels distinct from clear: {changed}");
        assert!(changed > 500);
    }

    #[test]
    fn generated_vrm1_candidate_retargets_bundled_idle_end_to_end() {
        let gpu = Gpu::new_headless().expect("headless GPU is required for VRM1 integration");
        let renderer = Renderer::new(&gpu, pocket3d::gpu::OFFSCREEN_FORMAT).unwrap();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let (model_file, root_node, spine_node, expression_node) =
            generated_vrm1_avatar(false, None, ConstraintFixture::None);
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
            AvatarSemanticDocument::Vrm1(document)
                if document.node_meshes[expression_node].is_some()
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
        assert_eq!(candidate.capabilities.expression_names, ["blink"]);
        assert!(matches!(
            &candidate.expressions,
            ResolvedExpressionRuntime::Vrm1(runtime)
                if runtime
                    .expressions
                    .iter()
                    .any(|expression| expression.name == "blink" && !expression.morph_binds.is_empty())
        ));
        let (mesh_slot, target) = match &candidate.expressions {
            ResolvedExpressionRuntime::Vrm1(runtime) => runtime
                .expressions
                .iter()
                .find(|expression| expression.name == "blink")
                .and_then(|expression| expression.morph_binds.first())
                .map(|bind| (bind.mesh_slot, bind.target))
                .unwrap(),
            ResolvedExpressionRuntime::Vrm0Legacy => unreachable!(),
        };
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
        assert!(candidate.node_constraints.is_none());

        let (instance, mut active) = candidate.into_parts(AvatarSceneSlot::new(0));
        let mut scene = Scene::default();
        scene.models.push(instance);
        if let ResolvedExpressionRuntime::Vrm1(runtime) = &mut active.expressions {
            assert!(runtime.set_input("blink", 1.0));
            runtime.compose_if_needed(&mut scene, active.scene_slot, 0.0);
        }
        assert_eq!(
            scene.models[0]
                .morph
                .as_ref()
                .unwrap()
                .weight(mesh_slot, target),
            1.0
        );
    }

    #[test]
    fn replacement_has_final_skin_pose_before_its_first_render() {
        let gpu = Gpu::new_headless().expect("headless GPU is required for VRM1 integration");
        let renderer = Renderer::new(&gpu, pocket3d::gpu::OFFSCREEN_FORMAT).unwrap();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let (model_file, _, spine_node, _) =
            generated_vrm1_avatar(true, Some("bone"), ConstraintFixture::ValidAimOptional);
        let request = AvatarLoadRequest::new(
            model_file.path().to_owned(),
            Some(root.join("assets/idle_loop.vrma")),
            "PreparedReplacement",
        );
        let candidate =
            AvatarCandidate::prepare(&gpu, &renderer, &root.join("dist/character.js"), &request)
                .expect("replacement pose must prepare before it is committed");
        assert!(
            candidate.locals[spine_node]
                .rotation
                .angle_between(candidate.asset.skeleton.rest[spine_node].rotation)
                > 1.0e-4,
            "initial VRMA sample must be applied before the replacement is visible"
        );

        let mut widget = Widget::new(WidgetConfig {
            model_path: model_file.path().to_owned(),
            vrma_path: root.join("assets/idle_loop.vrma"),
            bundle_path: root.join("dist/character.js"),
            menu_bundle_path: PathBuf::new(),
            menu_pak_path: PathBuf::new(),
            size: (450, 600),
            cli_max_fps_override: None,
            frames: None,
        });
        widget.commit_avatar_candidate(candidate);
        assert_eq!(
            widget.tick_count, 0,
            "no normal tick has run before first render"
        );

        let active = widget.active_avatar.as_ref().unwrap();
        let instance = &widget.scene.models[active.scene_slot.index()];
        let pose = instance
            .pose
            .as_ref()
            .expect("committed avatar must be posed");
        assert_eq!(pose, &active.globals);
        let mut palette = Vec::new();
        active.asset.palette_from_globals(pose, &mut palette);
        assert!(!palette.is_empty());
        assert!(palette.iter().all(|transform| transform.is_finite()));

        let mut rest_locals = Vec::new();
        active
            .asset
            .skeleton
            .sample_locals(None, 0.0, false, &mut rest_locals);
        let mut rest_globals = Vec::new();
        active
            .asset
            .skeleton
            .globals_from_locals(&rest_locals, &mut rest_globals);
        let mut rest_palette = Vec::new();
        active
            .asset
            .palette_from_globals(&rest_globals, &mut rest_palette);
        assert!(
            palette
                .iter()
                .zip(rest_palette.iter())
                .any(|(posed, rest)| {
                    posed
                        .to_cols_array()
                        .into_iter()
                        .zip(rest.to_cols_array())
                        .any(|(a, b)| (a - b).abs() > 1.0e-4)
                }),
            "the first rendered skin palette must include the staged animation"
        );

        let old_pose = pose.clone();
        let old_asset = active.asset.clone();
        let mut invalid_replacement =
            AvatarCandidate::prepare(&gpu, &renderer, &root.join("dist/character.js"), &request)
                .unwrap();
        invalid_replacement.instance.pose = Some(vec![Mat4::from_cols_array(&[f32::NAN; 16])]);
        assert!(
            invalid_replacement.validate_initial_pose().is_err(),
            "the preparation validator must reject an invalid initial pose"
        );
        let active = widget.active_avatar.as_ref().unwrap();
        let instance = &widget.scene.models[active.scene_slot.index()];
        assert!(Arc::ptr_eq(&active.asset, &old_asset));
        assert_eq!(instance.pose.as_ref(), Some(&old_pose));
    }

    #[test]
    fn widget_tick_keeps_constraints_in_model_space_through_final_palette() {
        let gpu = Gpu::new_headless().expect("headless GPU is required for VRM1 integration");
        let renderer = Renderer::new(&gpu, pocket3d::gpu::OFFSCREEN_FORMAT).unwrap();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let (model_file, _, spine_node, _) =
            generated_vrm1_avatar(true, Some("bone"), ConstraintFixture::ValidAimOptional);
        let request = AvatarLoadRequest::new(
            model_file.path().to_owned(),
            Some(root.join("assets/idle_loop.vrma")),
            "GeneratedVrm1FullTick",
        );
        let candidate =
            AvatarCandidate::prepare(&gpu, &renderer, &root.join("dist/character.js"), &request)
                .expect("generated full-pipeline VRM1 candidate should prepare");
        let (left_hand, right_hand, left_eye) = match &candidate.document {
            AvatarSemanticDocument::Vrm1(document) => (
                document
                    .humanoid
                    .node_for(pocket_vrm::Vrm1HumanBone::LeftHand)
                    .unwrap(),
                document
                    .humanoid
                    .node_for(pocket_vrm::Vrm1HumanBone::RightHand)
                    .unwrap(),
                document
                    .humanoid
                    .node_for(pocket_vrm::Vrm1HumanBone::LeftEye)
                    .unwrap(),
            ),
            AvatarSemanticDocument::Vrm0(_) => unreachable!(),
        };
        assert!(candidate.clips.iter().any(|(name, _)| name == "idle_loop"));
        assert!(candidate.clips.iter().any(|(name, clip)| {
            name == "idle_loop"
                && clip
                    .channels
                    .iter()
                    .any(|channel| channel.node == right_hand)
        }));
        assert!(candidate.look_at.is_some());
        assert!(candidate.node_constraints.is_some());
        assert_eq!(
            candidate.springs.as_ref().map(SpringSolver::joint_count),
            Some(1)
        );

        let mut widget = Widget::new(WidgetConfig {
            model_path: model_file.path().to_owned(),
            vrma_path: root.join("assets/idle_loop.vrma"),
            bundle_path: root.join("dist/character.js"),
            menu_bundle_path: PathBuf::new(),
            menu_pak_path: PathBuf::new(),
            size: (450, 600),
            cli_max_fps_override: None,
            frames: None,
        });
        widget.commit_avatar_candidate(candidate);
        {
            let active = widget.active_avatar.as_mut().unwrap();
            active.sim.tracking = TrackingMode::Mouse;
            active.sim.mouse_target = Vec3::new(1.25, 1.75, 4.0);
        }

        <Widget as Game>::tick(&mut widget, 0.25, &Input::default());

        let active = widget.active_avatar.as_ref().unwrap();
        let instance = &widget.scene.models[active.scene_slot.index()];
        let presentation = Mat4::from_rotation_y(core::f32::consts::PI);
        assert_eq!(instance.transform, presentation);
        assert_eq!(widget.tick_count, 1);
        assert!(
            active.locals[right_hand]
                .rotation
                .angle_between(active.asset.skeleton.rest[right_hand].rotation)
                > 1.0e-4,
            "VRMA should update its source joint before constraint evaluation"
        );
        assert!(
            active.locals[left_eye]
                .rotation
                .angle_between(active.asset.skeleton.rest[left_eye].rotation)
                > 1.0e-4,
            "bone LookAt should update skeleton locals before constraints"
        );
        assert!(
            active.locals[left_hand]
                .rotation
                .angle_between(active.asset.skeleton.rest[left_hand].rotation)
                > 1.0e-4,
            "the generated Aim constraint should produce a non-rest destination"
        );

        let mut recomputed_globals = vec![Mat4::IDENTITY; active.locals.len()];
        active
            .asset
            .skeleton
            .globals_from_locals(&active.locals, &mut recomputed_globals);
        for (actual, expected) in active.globals.iter().zip(&recomputed_globals) {
            assert!(
                actual
                    .to_cols_array()
                    .into_iter()
                    .zip(expected.to_cols_array())
                    .all(|(a, b)| (a - b).abs() < 1.0e-5),
                "final globals must be reconstructed solely from model-space locals"
            );
        }

        let pose = instance
            .pose
            .as_ref()
            .expect("Widget::tick must publish a final pose");
        assert_eq!(pose.len(), active.globals.len());
        for (actual, expected) in pose.iter().zip(&active.globals) {
            assert!(
                actual
                    .to_cols_array()
                    .into_iter()
                    .zip(expected.to_cols_array())
                    .all(|(a, b)| (a - b).abs() < 1.0e-5)
            );
        }

        let mut global_rotations = vec![glam::Quat::IDENTITY; active.locals.len()];
        for &node in &active.asset.skeleton.order {
            let local = active.locals[node].rotation.normalize();
            let parent = active.asset.skeleton.parents[node];
            global_rotations[node] = if parent == usize::MAX {
                local
            } else {
                (global_rotations[parent] * local).normalize()
            };
        }
        let parent = active.asset.skeleton.parents[left_hand];
        let parent_rotation = if parent == usize::MAX {
            glam::Quat::IDENTITY
        } else {
            global_rotations[parent]
        };
        let aimed = parent_rotation * active.locals[left_hand].rotation * Vec3::X;
        let direction = (active.globals[right_hand].w_axis.truncate()
            - active.globals[left_hand].w_axis.truncate())
        .normalize();
        assert!(
            aimed.normalize().dot(direction) > 0.999,
            "Aim must use the final model-space skeleton, never the instance transform"
        );

        let mut palette = Vec::new();
        active.asset.palette_from_globals(pose, &mut palette);
        let mut palette_index = 0;
        let mut constrained_palette_entry = None;
        for skin in &active.asset.skins {
            for (joint, &node) in skin.joints.iter().enumerate() {
                if node == left_hand {
                    constrained_palette_entry = Some((
                        palette_index,
                        active.globals[left_hand] * skin.inverse_bind[joint],
                    ));
                    break;
                }
                palette_index += 1;
            }
            if constrained_palette_entry.is_some() {
                break;
            }
        }
        let (palette_index, expected_palette) =
            constrained_palette_entry.expect("constrained hand should be a skinned joint");
        assert!(
            palette[palette_index]
                .to_cols_array()
                .into_iter()
                .zip(expected_palette.to_cols_array())
                .all(|(a, b)| (a - b).abs() < 1.0e-5),
            "skin palette must observe the constrained final pose"
        );
        assert!(
            active.springs.is_some(),
            "SpringBone remains live after the tick"
        );
        assert!(active.locals[spine_node].rotation.is_finite());
    }

    #[test]
    fn cursor_look_at_toggle_removes_and_restores_vrm1_bone_contribution() {
        let gpu = Gpu::new_headless().expect("headless GPU is required for LookAt integration");
        let renderer = Renderer::new(&gpu, pocket3d::gpu::OFFSCREEN_FORMAT).unwrap();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let (model_file, _, _, _) =
            generated_vrm1_avatar(true, Some("bone"), ConstraintFixture::ValidAimOptional);
        let candidate = AvatarCandidate::prepare(
            &gpu,
            &renderer,
            &root.join("dist/character.js"),
            &AvatarLoadRequest::new(model_file.path().to_owned(), None, "ToggleLookAt"),
        )
        .unwrap();
        let left_eye = match &candidate.document {
            AvatarSemanticDocument::Vrm1(document) => document
                .humanoid
                .node_for(pocket_vrm::Vrm1HumanBone::LeftEye)
                .unwrap(),
            AvatarSemanticDocument::Vrm0(_) => unreachable!(),
        };
        let mut widget = Widget::new(WidgetConfig {
            model_path: model_file.path().to_owned(),
            vrma_path: root.join("assets/idle_loop.vrma"),
            bundle_path: root.join("dist/character.js"),
            menu_bundle_path: PathBuf::new(),
            menu_pak_path: PathBuf::new(),
            size: (450, 600),
            cli_max_fps_override: None,
            frames: None,
        });
        widget.commit_avatar_candidate(candidate);
        {
            let active = widget.active_avatar.as_mut().unwrap();
            active.sim.tracking = TrackingMode::Mouse;
            active.sim.mouse_target = Vec3::new(1.25, 1.75, 4.0);
        }
        widget.settings.avatar_behavior.look_at_mode = crate::settings::LookAtMode::Off;
        <Widget as Game>::tick(&mut widget, 0.25, &Input::default());
        let active = widget.active_avatar.as_ref().unwrap();
        let rest = active.asset.skeleton.rest[left_eye].rotation;
        assert!(active.locals[left_eye].rotation.angle_between(rest) < 1.0e-4);

        widget.settings.avatar_behavior.look_at_mode = crate::settings::LookAtMode::Window;
        <Widget as Game>::tick(&mut widget, 0.25, &Input::default());
        let active = widget.active_avatar.as_ref().unwrap();
        assert!(active.locals[left_eye].rotation.angle_between(rest) > 1.0e-4);

        widget.settings.avatar_behavior.look_at_mode = crate::settings::LookAtMode::Global;
        <Widget as Game>::tick(&mut widget, 0.25, &Input::default());
        let active = widget.active_avatar.as_ref().unwrap();
        assert!(active.locals[left_eye].rotation.angle_between(rest) > 1.0e-4);
    }

    #[test]
    fn auto_blink_toggle_removes_generated_source_but_preserves_manual_blink() {
        let gpu = Gpu::new_headless().expect("headless GPU is required for blink integration");
        let renderer = Renderer::new(&gpu, pocket3d::gpu::OFFSCREEN_FORMAT).unwrap();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let (model_file, _, _, _) = generated_vrm1_avatar(true, None, ConstraintFixture::None);
        let candidate = AvatarCandidate::prepare(
            &gpu,
            &renderer,
            &root.join("dist/character.js"),
            &AvatarLoadRequest::new(model_file.path().to_owned(), None, "ToggleBlink"),
        )
        .unwrap();
        let mut widget = Widget::new(WidgetConfig {
            model_path: model_file.path().to_owned(),
            vrma_path: root.join("assets/idle_loop.vrma"),
            bundle_path: root.join("dist/character.js"),
            menu_bundle_path: PathBuf::new(),
            menu_pak_path: PathBuf::new(),
            size: (450, 600),
            cli_max_fps_override: None,
            frames: None,
        });
        widget.commit_avatar_candidate(candidate);
        let (blink_index, mesh_slot, target) = {
            let active = widget.active_avatar.as_ref().unwrap();
            let ResolvedExpressionRuntime::Vrm1(runtime) = &active.expressions else {
                unreachable!()
            };
            let (index, expression) = runtime
                .expressions
                .iter()
                .enumerate()
                .find(|(_, expression)| expression.name == "blink")
                .unwrap();
            let bind = expression.morph_binds.first().unwrap();
            (index, bind.mesh_slot, bind.target)
        };
        for _ in 0..600 {
            <Widget as Game>::tick(&mut widget, 1.0 / 60.0, &Input::default());
            if widget.last_blink > 0.1 {
                break;
            }
        }
        assert!(
            widget.last_blink > 0.1,
            "automatic blink should occur within ten seconds"
        );

        widget.settings.avatar_behavior.auto_blink = false;
        <Widget as Game>::tick(&mut widget, 1.0 / 60.0, &Input::default());
        assert_eq!(widget.last_blink, 0.0);
        let active = widget.active_avatar.as_ref().unwrap();
        assert_eq!(
            widget.scene.models[active.scene_slot.index()]
                .morph
                .as_ref()
                .unwrap()
                .weight(mesh_slot, target),
            0.0
        );

        let active = widget.active_avatar.as_mut().unwrap();
        let ResolvedExpressionRuntime::Vrm1(runtime) = &mut active.expressions else {
            unreachable!()
        };
        assert!(runtime.set_manual_input(blink_index, 0.75));
        <Widget as Game>::tick(&mut widget, 1.0 / 60.0, &Input::default());
        let active = widget.active_avatar.as_ref().unwrap();
        assert_eq!(
            widget.scene.models[active.scene_slot.index()]
                .morph
                .as_ref()
                .unwrap()
                .weight(mesh_slot, target),
            0.75
        );
    }

    #[test]
    fn generated_node_constraint_loading_policy_and_lifecycle_are_transactional() {
        let gpu = Gpu::new_headless().expect("headless GPU is required for constraint integration");
        let renderer = Renderer::new(&gpu, pocket3d::gpu::OFFSCREEN_FORMAT).unwrap();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let bundle = root.join("dist/character.js");

        let optional =
            prepare_constraint_fixture(&gpu, &renderer, &bundle, ConstraintFixture::ValidOptional)
                .expect("valid optional constraints should prepare");
        assert!(optional.node_constraints.is_some());

        let required =
            prepare_constraint_fixture(&gpu, &renderer, &bundle, ConstraintFixture::ValidRequired)
                .expect("valid required constraints should prepare");
        assert!(required.node_constraints.is_some());

        for fixture in [
            ConstraintFixture::MalformedOptional,
            ConstraintFixture::CyclicOptional,
        ] {
            let candidate = prepare_constraint_fixture(&gpu, &renderer, &bundle, fixture)
                .expect("invalid optional constraints should downgrade atomically");
            assert!(candidate.node_constraints.is_none(), "{fixture:?}");
        }

        for fixture in [
            ConstraintFixture::MalformedRequired,
            ConstraintFixture::CyclicRequired,
            ConstraintFixture::UnsupportedRequired,
        ] {
            let error = match prepare_constraint_fixture(&gpu, &renderer, &bundle, fixture) {
                Ok(_) => panic!("invalid required constraints must reject {fixture:?}"),
                Err(error) => error,
            };
            assert!(
                error.message().contains("VRMC_node_constraint")
                    || error.message().contains("node constraint"),
                "{fixture:?}: {error:#}"
            );
        }

        let slot = AvatarSceneSlot::new(0);
        let (_instance, old_active) = optional.into_parts(slot);
        assert!(old_active.node_constraints.is_some());
        let failed_replacement =
            prepare_constraint_fixture(&gpu, &renderer, &bundle, ConstraintFixture::CyclicRequired);
        assert!(failed_replacement.is_err());
        assert!(
            old_active.node_constraints.is_some(),
            "failed replacement must not mutate the active constraint runtime"
        );

        let replacement =
            prepare_constraint_fixture(&gpu, &renderer, &bundle, ConstraintFixture::None)
                .expect("constraint-free replacement should prepare");
        let (_instance, replacement_active) = replacement.into_parts(slot);
        assert!(
            replacement_active.node_constraints.is_none(),
            "successful replacement must not retain the old runtime"
        );
    }

    #[test]
    fn official_constraint_twist_sample_prepares_evaluates_and_feeds_springs() {
        let fixture = Path::new(r"C:\Users\Breeze\Downloads\VRM1_Constraint_Twist_Sample.vrm");
        if !fixture.is_file() {
            eprintln!(
                "skipping local official VRMC_node_constraint fixture: {} is unavailable",
                fixture.display()
            );
            return;
        }

        let bytes = std::fs::read(fixture).expect("reading official constraint fixture");
        let document =
            Vrm1Doc::from_glb_bytes(&bytes).expect("parsing official constraint fixture");
        let glb =
            pocket_vrm::glb::parse_glb(&bytes).expect("reading official fixture declarations");
        assert_eq!(document.node_count, 171);
        assert!(
            extension_declaration_contains(&glb.json, "extensionsUsed", "VRMC_node_constraint")
                .unwrap()
        );
        assert!(
            !extension_declaration_contains(
                &glb.json,
                "extensionsRequired",
                "VRMC_node_constraint"
            )
            .unwrap()
        );

        let semantics = document
            .node_constraint_semantics
            .clone()
            .expect("official fixture should contain typed constraints");
        let spring_semantics = document
            .spring_bone_semantics
            .clone()
            .expect("official fixture should contain SpringBone chains");
        assert_eq!(semantics.constraints.len(), 14);
        assert_eq!(spring_semantics.springs.len(), 22);
        let mut metadata_counts = (0usize, 0usize, 0usize);
        for constraint in &semantics.constraints {
            match constraint.kind {
                pocket_vrm::Vrm1NodeConstraintKind::Roll { .. } => metadata_counts.0 += 1,
                pocket_vrm::Vrm1NodeConstraintKind::Aim { .. } => metadata_counts.1 += 1,
                pocket_vrm::Vrm1NodeConstraintKind::Rotation { .. } => metadata_counts.2 += 1,
            }
        }
        assert_eq!(metadata_counts, (8, 6, 0));

        let gpu = Gpu::new_headless().expect("headless GPU is required for official fixture smoke");
        let renderer = Renderer::new(&gpu, pocket3d::gpu::OFFSCREEN_FORMAT).unwrap();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let request = AvatarLoadRequest::new(fixture.to_owned(), None, "ConstraintTwistSample");
        let mut candidate =
            AvatarCandidate::prepare(&gpu, &renderer, &root.join("dist/character.js"), &request)
                .expect("official optional constraint fixture should prepare");
        let runtime = candidate
            .node_constraints
            .as_ref()
            .expect("official fixture should resolve a runtime");
        assert_eq!(runtime.kind_counts(), (8, 6, 0));
        assert_eq!(runtime.ordered_destinations().len(), 14);
        assert!(candidate.springs.is_some());

        let roll = semantics
            .constraints
            .iter()
            .find_map(|constraint| match constraint.kind {
                pocket_vrm::Vrm1NodeConstraintKind::Roll {
                    source,
                    axis,
                    weight,
                } => Some((constraint.destination, source, axis, weight)),
                _ => None,
            })
            .expect("official fixture should contain Roll");
        candidate.locals.clone_from(&candidate.asset.skeleton.rest);
        let source_rest = candidate.asset.skeleton.rest[roll.1].rotation;
        let destination_rest = candidate.asset.skeleton.rest[roll.0].rotation;
        let (source_current, expected_roll) = [Vec3::X, Vec3::Y, Vec3::Z]
            .into_iter()
            .find_map(|axis| {
                let source_current = source_rest * glam::Quat::from_axis_angle(axis, 0.8);
                let expected = pocket_vrm::apply_roll_constraint(
                    source_rest,
                    source_current,
                    destination_rest,
                    roll.2,
                    roll.3,
                );
                (expected.angle_between(destination_rest) > 1.0e-3)
                    .then_some((source_current, expected))
            })
            .expect("generated source motion should exercise official Roll");
        candidate.locals[roll.1].rotation = source_current;
        candidate.node_constraints.as_mut().unwrap().evaluate(
            &candidate.asset.skeleton,
            &mut candidate.locals,
            &mut candidate.globals,
        );
        assert!(
            candidate.locals[roll.0]
                .rotation
                .normalize()
                .dot(expected_roll.normalize())
                .abs()
                > 1.0 - 1.0e-5
        );

        candidate.locals.clone_from(&candidate.asset.skeleton.rest);
        candidate.node_constraints.as_mut().unwrap().evaluate(
            &candidate.asset.skeleton,
            &mut candidate.locals,
            &mut candidate.globals,
        );
        let aim = semantics
            .constraints
            .iter()
            .find_map(|constraint| match constraint.kind {
                pocket_vrm::Vrm1NodeConstraintKind::Aim {
                    source,
                    axis,
                    weight: 1.0,
                } => Some((constraint.destination, source, axis)),
                _ => None,
            })
            .expect("official fixture should contain a full-weight Aim");
        let mut global_rotations = vec![glam::Quat::IDENTITY; candidate.locals.len()];
        for &node in &candidate.asset.skeleton.order {
            let local = candidate.locals[node].rotation.normalize();
            let parent = candidate.asset.skeleton.parents[node];
            global_rotations[node] = if parent == usize::MAX {
                local
            } else {
                (global_rotations[parent] * local).normalize()
            };
        }
        let parent = candidate.asset.skeleton.parents[aim.0];
        let parent_rotation = if parent == usize::MAX {
            glam::Quat::IDENTITY
        } else {
            global_rotations[parent]
        };
        let aimed =
            parent_rotation * candidate.locals[aim.0].rotation * pocket_vrm::aim_axis_vector(aim.2);
        let direction = (candidate.globals[aim.1].w_axis.truncate()
            - candidate.globals[aim.0].w_axis.truncate())
        .normalize();
        assert!(aimed.normalize().dot(direction) > 0.999);

        let is_strict_ancestor = |ancestor: usize, mut node: usize| {
            node = candidate.asset.skeleton.parents[node];
            while node != usize::MAX {
                if node == ancestor {
                    return true;
                }
                node = candidate.asset.skeleton.parents[node];
            }
            false
        };
        let constrained_spring_ancestor = semantics
            .constraints
            .iter()
            .find_map(|constraint| {
                spring_semantics
                    .springs
                    .iter()
                    .filter_map(|spring| spring.joints.first())
                    .any(|joint| is_strict_ancestor(constraint.destination, joint.node))
                    .then_some(constraint.destination)
            })
            .expect("official fixture should constrain an ancestor of a SpringBone chain");
        let constrained_rotation = candidate.locals[constrained_spring_ancestor].rotation;
        candidate.springs.as_mut().unwrap().step(
            1.0 / 60.0,
            &candidate.asset.skeleton,
            &mut candidate.locals,
            Mat4::IDENTITY,
        );
        assert!(
            candidate.locals[constrained_spring_ancestor]
                .rotation
                .normalize()
                .dot(constrained_rotation.normalize())
                .abs()
                > 1.0 - 1.0e-5
        );
        candidate
            .asset
            .skeleton
            .globals_from_locals(&candidate.locals, &mut candidate.globals);
        assert!(candidate.globals.iter().all(|matrix| {
            matrix
                .to_cols_array()
                .into_iter()
                .all(|component| component.is_finite())
        }));
    }

    #[test]
    fn generated_vrm1_candidate_builds_spring_solver() {
        let gpu = Gpu::new_headless().expect("headless GPU is required for VRM1 integration");
        let renderer = Renderer::new(&gpu, pocket3d::gpu::OFFSCREEN_FORMAT).unwrap();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let (model_file, _, _, _) =
            generated_vrm1_avatar(true, Some("bone"), ConstraintFixture::None);
        let request =
            AvatarLoadRequest::new(model_file.path().to_owned(), None, "GeneratedVrm1Spring");
        let candidate =
            AvatarCandidate::prepare(&gpu, &renderer, &root.join("dist/character.js"), &request)
                .expect("generated VRM1 spring avatar should load");
        assert_eq!(
            candidate.springs.as_ref().map(SpringSolver::joint_count),
            Some(1)
        );
        assert!(candidate.look_at.is_some());
        assert_eq!(
            candidate.presentation.transform,
            Mat4::from_rotation_y(core::f32::consts::PI)
        );
    }

    #[test]
    fn generated_bone_look_at_resolves_and_malformed_metadata_stays_nonfatal() {
        let gpu = Gpu::new_headless().expect("headless GPU is required for VRM1 integration");
        let renderer = Renderer::new(&gpu, pocket3d::gpu::OFFSCREEN_FORMAT).unwrap();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");

        let (bone_file, _, _, _) =
            generated_vrm1_avatar(false, Some("bone"), ConstraintFixture::None);
        let bone = AvatarCandidate::prepare(
            &gpu,
            &renderer,
            &root.join("dist/character.js"),
            &AvatarLoadRequest::new(bone_file.path().to_owned(), None, "BoneLookAt"),
        )
        .expect("generated bone LookAt avatar should load");
        assert!(bone.look_at.is_some());

        let (malformed_file, _, _, _) =
            generated_vrm1_avatar(false, Some("malformed"), ConstraintFixture::None);
        let malformed = AvatarCandidate::prepare(
            &gpu,
            &renderer,
            &root.join("dist/character.js"),
            &AvatarLoadRequest::new(malformed_file.path().to_owned(), None, "MalformedLookAt"),
        )
        .expect("malformed LookAt must not poison transactional preparation");
        assert!(malformed.look_at.is_none());
        assert!(matches!(
            malformed.document,
            AvatarSemanticDocument::Vrm1(Vrm1Doc { look_at: None, .. })
        ));
    }

    #[test]
    fn avatar_replacement_does_not_retain_procedural_gaze() {
        let gpu = Gpu::new_headless().expect("headless GPU is required for VRM1 integration");
        let renderer = Renderer::new(&gpu, pocket3d::gpu::OFFSCREEN_FORMAT).unwrap();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let bundle = root.join("dist/character.js");
        let (first_file, _, _, _) =
            generated_vrm1_avatar(false, Some("expression"), ConstraintFixture::None);
        let (second_file, _, _, _) =
            generated_vrm1_avatar(false, Some("expression"), ConstraintFixture::None);
        let first = AvatarCandidate::prepare(
            &gpu,
            &renderer,
            &bundle,
            &AvatarLoadRequest::new(first_file.path().to_owned(), None, "FirstLookAt"),
        )
        .unwrap();
        let second = AvatarCandidate::prepare(
            &gpu,
            &renderer,
            &bundle,
            &AvatarLoadRequest::new(second_file.path().to_owned(), None, "SecondLookAt"),
        )
        .unwrap();
        let slot = AvatarSceneSlot::new(0);
        let (instance, mut active) = first.into_parts(slot);
        let mut scene = Scene::default();
        scene.models.push(instance);
        if let ResolvedExpressionRuntime::Vrm1(runtime) = &mut active.expressions {
            let index = runtime
                .manual_state()
                .iter()
                .find(|expression| expression.name == "lookLeft")
                .unwrap()
                .index;
            assert!(runtime.set_manual_input(index, 0.7));
        }
        let failed = AvatarCandidate::prepare(
            &gpu,
            &renderer,
            &bundle,
            &AvatarLoadRequest::new(
                first_file.path().with_extension("missing.vrm"),
                None,
                "Malformed",
            ),
        );
        assert!(failed.is_err());
        let ResolvedExpressionRuntime::Vrm1(old_runtime) = &active.expressions else {
            unreachable!()
        };
        assert_eq!(
            old_runtime
                .manual_state()
                .iter()
                .find(|expression| expression.name == "lookLeft")
                .unwrap()
                .weight,
            0.7
        );
        let (mesh_slot, target) = match &mut active.expressions {
            ResolvedExpressionRuntime::Vrm1(runtime) => {
                let bind = runtime
                    .expressions
                    .iter()
                    .find(|expression| expression.name == "lookLeft")
                    .and_then(|expression| expression.morph_binds.first())
                    .unwrap();
                let key = (bind.mesh_slot, bind.target);
                runtime.compose_with_procedural_look_at(
                    &mut scene,
                    slot,
                    0.0,
                    pocket_vrm::Vrm1ExpressionLookAt {
                        look_left: 1.0,
                        ..Default::default()
                    },
                );
                key
            }
            ResolvedExpressionRuntime::Vrm0Legacy => unreachable!(),
        };
        assert_eq!(
            scene.models[0]
                .morph
                .as_ref()
                .unwrap()
                .weight(mesh_slot, target),
            1.0
        );

        let replacement_initial_weight = second
            .instance
            .morph
            .as_ref()
            .unwrap()
            .weight(mesh_slot, target);
        assert_ne!(replacement_initial_weight, 1.0);
        let (replacement, mut replacement_active) = second.into_parts(slot);
        let ResolvedExpressionRuntime::Vrm1(new_runtime) = &replacement_active.expressions else {
            unreachable!()
        };
        assert_eq!(
            new_runtime
                .manual_state()
                .iter()
                .find(|expression| expression.name == "lookLeft")
                .unwrap()
                .weight,
            0.0
        );
        slot.replace(&mut scene, replacement);
        if let ResolvedExpressionRuntime::Vrm1(runtime) = &mut replacement_active.expressions {
            runtime.compose_if_needed(&mut scene, slot, 0.0);
        }
        assert_eq!(
            scene.models[0]
                .morph
                .as_ref()
                .unwrap()
                .weight(mesh_slot, target),
            replacement_initial_weight
        );
    }

    #[test]
    fn expression_menu_state_switches_only_after_successful_avatar_commit() {
        use crate::menu_guest::MenuAction;

        let gpu = Gpu::new_headless().expect("headless GPU is required for VRM1 integration");
        let renderer = Renderer::new(&gpu, pocket3d::gpu::OFFSCREEN_FORMAT).unwrap();
        let bundle = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../dist/character.js");
        let (first_file, _, _, _) =
            generated_vrm1_avatar(false, Some("expression"), ConstraintFixture::None);
        let (second_file, _, _, _) = generated_vrm1_avatar(false, None, ConstraintFixture::None);
        let mut widget = Widget::new(WidgetConfig {
            model_path: first_file.path().to_owned(),
            vrma_path: PathBuf::new(),
            bundle_path: bundle.clone(),
            menu_bundle_path: PathBuf::new(),
            menu_pak_path: PathBuf::new(),
            size: (450, 600),
            cli_max_fps_override: None,
            frames: None,
        });
        let first = AvatarCandidate::prepare(
            &gpu,
            &renderer,
            &bundle,
            &AvatarLoadRequest::new(first_file.path().to_owned(), None, "First"),
        )
        .unwrap();
        widget.commit_avatar_candidate(first);
        let first_generation = widget.avatar_generation;
        let (first_names, look_index) = match &widget.active_avatar.as_ref().unwrap().expressions {
            ResolvedExpressionRuntime::Vrm1(runtime) => {
                let state = runtime.manual_state();
                let index = state
                    .iter()
                    .find(|expression| expression.name == "lookLeft")
                    .unwrap()
                    .index;
                (
                    state
                        .iter()
                        .map(|expression| expression.name.clone())
                        .collect::<Vec<_>>(),
                    index,
                )
            }
            ResolvedExpressionRuntime::Vrm0Legacy => unreachable!(),
        };
        widget.apply_menu_action(MenuAction::SetExpression(
            first_generation,
            look_index,
            0.65,
        ));
        let missing = first_file.path().with_extension("missing.vrm");
        widget.request_avatar_replacement(AvatarLoadRequest::new(missing, None, "Malformed"));
        widget.process_pending_avatar_request(&gpu, &renderer);
        assert_eq!(widget.avatar_generation, first_generation);
        let ResolvedExpressionRuntime::Vrm1(runtime) =
            &widget.active_avatar.as_ref().unwrap().expressions
        else {
            unreachable!()
        };
        assert_eq!(
            runtime
                .manual_state()
                .iter()
                .map(|expression| expression.name.clone())
                .collect::<Vec<_>>(),
            first_names
        );
        assert_eq!(
            runtime
                .manual_state()
                .iter()
                .find(|expression| expression.name == "lookLeft")
                .unwrap()
                .weight,
            0.65
        );

        widget.request_avatar_replacement(AvatarLoadRequest::new(
            second_file.path().to_owned(),
            None,
            "Second",
        ));
        widget.process_pending_avatar_request(&gpu, &renderer);
        assert_ne!(widget.avatar_generation, first_generation);
        let ResolvedExpressionRuntime::Vrm1(runtime) =
            &widget.active_avatar.as_ref().unwrap().expressions
        else {
            unreachable!()
        };
        assert_eq!(
            runtime
                .manual_state()
                .iter()
                .map(|expression| expression.name.as_str())
                .collect::<Vec<_>>(),
            ["blink"]
        );
        assert!(
            runtime
                .manual_state()
                .iter()
                .all(|expression| expression.weight == 0.0)
        );
        widget.apply_menu_action(MenuAction::SetExpression(first_generation, 0, 1.0));
        let ResolvedExpressionRuntime::Vrm1(runtime) =
            &widget.active_avatar.as_ref().unwrap().expressions
        else {
            unreachable!()
        };
        assert_eq!(runtime.manual_state()[0].weight, 0.0);
    }
}
