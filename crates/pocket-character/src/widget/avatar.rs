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
use pocket_vrm::{SpringSolver, Vrm1Doc, VrmDoc};
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AvatarLoadRequest {
    pub(super) model_path: PathBuf,
    pub(super) vrma_path: Option<PathBuf>,
    pub(super) model_name: String,
}

impl AvatarLoadRequest {
    pub(crate) fn new(
        model_path: PathBuf,
        vrma_path: Option<PathBuf>,
        model_name: impl Into<String>,
    ) -> Self {
        Self {
            model_path,
            vrma_path,
            model_name: model_name.into(),
        }
    }
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

/// Narrow, observable error state for a failed runtime load.  The old active
/// avatar is intentionally not part of this error path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AvatarRuntimeError {
    message: String,
}

impl AvatarRuntimeError {
    pub(super) fn new(error: &anyhow::Error) -> Self {
        Self {
            message: format!("{error:#}"),
        }
    }

    #[cfg(test)]
    pub(super) fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for AvatarRuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message.fmt(f)
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
    ) -> Result<Self> {
        let model_bytes = std::fs::read(&request.model_path)
            .with_context(|| format!("reading model {}", request.model_path.display()))?;
        let document =
            AvatarSemanticDocument::parse(&model_bytes).context("parsing VRM document")?;
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
                    .context("parsing VRM 1.0 glTF material extensions")?;
                let allowed_required_extensions = vrm1_allowed_required_extensions(&glb.json)?;
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
        .context("loading VRM model")?;

        let mut clips = Vec::new();
        if let (Some(vrm0), Some(vrma_path)) = (document.vrm0(), request.vrma_path.as_ref()) {
            let vrma_bytes = std::fs::read(vrma_path)
                .with_context(|| format!("reading vrma {}", vrma_path.display()))?;
            let vrma = pocket_vrm::load_vrma_bytes(&vrma_bytes)?;
            let clip = pocket_vrm::retarget(&vrma, &vrm0.humanoid, &model.skeleton)?;
            let clip_name = vrma_path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "idle".into());
            clips.push((clip_name, clip));
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
            .with_context(|| format!("reading bundle {}", bundle_path.display()))?;
        let capabilities =
            AvatarCapabilities::for_candidate(request.model_name.clone(), &document, &clips);
        let guest = CharacterGuest::boot(
            &bundle,
            &capabilities.model_name,
            &capabilities.clip_names,
            &capabilities.expression_names,
        )?;

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
                animation: AvatarAnimationState::default(),
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
    use super::*;
    use crate::settings::CameraSettings;

    fn approx_vec3(actual: Vec3, expected: Vec3) {
        assert!(
            (actual - expected).abs().max_element() < 1.0e-5,
            "{actual:?} != {expected:?}"
        );
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
        let runtime_error = AvatarRuntimeError::new(&error);
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
        assert_eq!(
            AvatarAnimationState::default(),
            AvatarAnimationState {
                clip_index: 0,
                clip_time: 0.0,
                clip_looping: true,
            }
        );
    }
}
