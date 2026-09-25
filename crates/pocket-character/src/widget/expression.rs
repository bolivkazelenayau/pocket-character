//! Parent-owned VRM expression resolution and composition.
//!
//! VRM 0.x intentionally keeps its existing direct-write behavior in
//! `widget.rs`.  This module contains only the additive VRM 1.0 path: parser
//! semantics are resolved against the imported ModelAsset during candidate
//! preparation, then persistent inputs are composed into per-instance morph
//! weights at runtime.

use std::collections::{BTreeMap, HashMap};

use anyhow::Result;
use pocket_vrm::{
    Vrm1Doc, Vrm1ExpressionKind, Vrm1ExpressionLookAt, Vrm1ExpressionOverride,
    Vrm1MaterialColorBindType,
};
use pocket3d::material::{MaterialAsset, MaterialKind, MaterialStateSet, TextureRole};
use pocket3d::model::{ModelAsset, ModelInstance};
use pocket3d::scene::Scene;

use super::avatar::AvatarSceneSlot;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ResolvedExpressionClass {
    Blink,
    LookAt,
    Mouth,
    Other,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct ResolvedMorphBind {
    pub(super) mesh_slot: usize,
    pub(super) target: usize,
    pub(super) weight: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct ResolvedMaterialColorBind {
    pub(super) material: usize,
    pub(super) kind: Vrm1MaterialColorBindType,
    pub(super) target_value: [f32; 4],
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct ResolvedTextureTransformBind {
    pub(super) material: usize,
    pub(super) scale: [f32; 2],
    pub(super) offset: [f32; 2],
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct ResolvedExpression {
    pub(super) name: String,
    pub(super) is_custom: bool,
    pub(super) morph_binds: Vec<ResolvedMorphBind>,
    pub(super) material_color_binds: Vec<ResolvedMaterialColorBind>,
    pub(super) texture_transform_binds: Vec<ResolvedTextureTransformBind>,
    pub(super) is_binary: bool,
    pub(super) class: ResolvedExpressionClass,
    pub(super) override_blink: Vrm1ExpressionOverride,
    pub(super) override_look_at: Vrm1ExpressionOverride,
    pub(super) override_mouth: Vrm1ExpressionOverride,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProceduralBlinkSource {
    Preset(usize),
    Pair(usize, usize),
    Disabled,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum ResolvedExpressionRuntime {
    Vrm0Legacy,
    Vrm1(Vrm1ExpressionRuntime),
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Vrm1ExpressionRuntime {
    pub(super) expressions: Vec<ResolvedExpression>,
    // Character guest (click/animation) and settings are independent sources.
    inputs: Vec<f32>,
    manual_inputs: Vec<f32>,
    managed_targets: Vec<(usize, usize)>,
    procedural_blink: ProceduralBlinkSource,
    dirty: bool,
    last_procedural_blink: f32,
    last_procedural_look_at: Option<Vrm1ExpressionLookAt>,
    warned_nonfinite_input: bool,
}

impl Vrm1ExpressionRuntime {
    fn new(expressions: Vec<ResolvedExpression>) -> Self {
        let mut managed_targets = expressions
            .iter()
            .flat_map(|expression| {
                expression
                    .morph_binds
                    .iter()
                    .map(|bind| (bind.mesh_slot, bind.target))
            })
            .collect::<Vec<_>>();
        managed_targets.sort_unstable();
        managed_targets.dedup();

        let preset = expressions.iter().position(|expression| {
            expression.class == ResolvedExpressionClass::Blink && expression.name == "blink"
        });
        let left = expressions.iter().position(|expression| {
            expression.class == ResolvedExpressionClass::Blink && expression.name == "blinkLeft"
        });
        let right = expressions.iter().position(|expression| {
            expression.class == ResolvedExpressionClass::Blink && expression.name == "blinkRight"
        });
        let procedural_blink = if let Some(index) = preset {
            ProceduralBlinkSource::Preset(index)
        } else if let (Some(left), Some(right)) = (left, right) {
            ProceduralBlinkSource::Pair(left, right)
        } else {
            if left.is_some() || right.is_some() {
                log::warn!(
                    "VRM 1.0 has only one usable blink side; procedural blinking is disabled"
                );
            }
            ProceduralBlinkSource::Disabled
        };

        Self {
            inputs: vec![0.0; expressions.len()],
            manual_inputs: vec![0.0; expressions.len()],
            expressions,
            managed_targets,
            procedural_blink,
            dirty: false,
            // NaN makes the first procedural sample observable without
            // assigning an artificial blink value to the rest pose.
            last_procedural_blink: f32::NAN,
            last_procedural_look_at: None,
            warned_nonfinite_input: false,
        }
    }

    pub(super) fn capability_names(&self) -> Vec<String> {
        self.expressions
            .iter()
            .map(|expression| expression.name.clone())
            .collect()
    }

    pub(super) fn manual_state(&self) -> Vec<super::super::menu_guest::MenuExpressionState> {
        const PRESETS: &[&str] = &[
            "neutral",
            "happy",
            "angry",
            "sad",
            "relaxed",
            "surprised",
            "aa",
            "ih",
            "ou",
            "ee",
            "oh",
            "blink",
            "blinkLeft",
            "blinkRight",
            "lookUp",
            "lookDown",
            "lookLeft",
            "lookRight",
        ];
        let mut indices: Vec<_> = (0..self.expressions.len()).collect();
        indices.sort_by_key(|&index| {
            let expression = &self.expressions[index];
            if expression.is_custom {
                (1, index)
            } else {
                (
                    0,
                    PRESETS
                        .iter()
                        .position(|name| *name == expression.name)
                        .unwrap_or(PRESETS.len()),
                )
            }
        });
        indices
            .into_iter()
            .map(|index| {
                let expression = &self.expressions[index];
                super::super::menu_guest::MenuExpressionState {
                    index,
                    name: expression.name.clone(),
                    weight: self.manual_inputs[index],
                    binary: expression.is_binary,
                    custom: expression.is_custom,
                }
            })
            .collect()
    }

    pub(super) fn set_manual_input(&mut self, index: usize, value: f32) -> bool {
        let (Some(expression), Some(input)) = (
            self.expressions.get(index),
            self.manual_inputs.get_mut(index),
        ) else {
            return false;
        };
        if !value.is_finite() {
            return false;
        }
        let value = if expression.is_binary {
            if value > 0.5 { 1.0 } else { 0.0 }
        } else {
            value.clamp(0.0, 1.0)
        };
        if *input == value {
            return false;
        }
        *input = value;
        self.dirty = true;
        true
    }

    pub(super) fn reset_manual_inputs(&mut self) -> bool {
        let changed = self.manual_inputs.iter().any(|&weight| weight != 0.0);
        self.manual_inputs.fill(0.0);
        self.dirty |= changed;
        changed
    }

    fn combined_input(&self, index: usize) -> f32 {
        self.inputs[index].max(self.manual_inputs[index])
    }

    pub(super) fn set_input(&mut self, name: &str, value: f32) -> bool {
        if !value.is_finite() {
            if !self.warned_nonfinite_input {
                log::warn!("character.setExpression: ignoring non-finite VRM 1.0 expression input");
                self.warned_nonfinite_input = true;
            }
            return false;
        }
        let value = value.clamp(0.0, 1.0);
        let mut changed = false;
        for (index, expression) in self.expressions.iter().enumerate() {
            if expression.name == name && self.inputs[index] != value {
                self.inputs[index] = value;
                changed = true;
            }
        }
        if changed {
            self.dirty = true;
        }
        changed
    }

    pub(super) fn compose_if_needed(
        &mut self,
        scene: &mut Scene,
        scene_slot: AvatarSceneSlot,
        procedural_blink: f32,
    ) {
        let procedural_look_at = self.last_procedural_look_at.unwrap_or_default();
        self.compose_with_procedural_look_at(
            scene,
            scene_slot,
            procedural_blink,
            procedural_look_at,
        );
    }

    pub(super) fn compose_with_procedural_look_at(
        &mut self,
        scene: &mut Scene,
        scene_slot: AvatarSceneSlot,
        procedural_blink: f32,
        procedural_look_at: Vrm1ExpressionLookAt,
    ) {
        self.compose_on_instance(
            scene_slot.get_mut(scene),
            procedural_blink,
            procedural_look_at,
        );
    }

    pub(super) fn compose_on_instance(
        &mut self,
        instance: &mut ModelInstance,
        procedural_blink: f32,
        procedural_look_at: Vrm1ExpressionLookAt,
    ) {
        if !self.dirty
            && self.last_procedural_blink == procedural_blink
            && self.last_procedural_look_at == Some(procedural_look_at)
        {
            return;
        }
        let effective_weights = self.effective_weights(procedural_blink, procedural_look_at);
        let assignments = self.composed_morph_weights(&effective_weights);
        if let Some(morph) = instance.morph.as_mut() {
            // Rewriting the complete managed set prevents a dropped expression
            // or a new command batch from leaving stale weights behind.
            for &(mesh_slot, target) in &self.managed_targets {
                morph.set_weight(mesh_slot, target, 0.0);
            }
            for ((mesh_slot, target), weight) in assignments {
                morph.set_weight(mesh_slot, target, weight);
            }
        }

        let asset = instance.asset.clone();
        instance.materials.reset_from_assets(asset.materials());
        for (expression, weight) in self.expressions.iter().zip(effective_weights) {
            apply_material_expression(
                &mut instance.materials,
                asset.materials(),
                expression,
                weight,
            );
        }
        self.dirty = false;
        self.last_procedural_blink = procedural_blink;
        self.last_procedural_look_at = Some(procedural_look_at);
    }

    #[cfg(test)]
    pub(super) fn composed_weights_for_test(
        &self,
        procedural_blink: f32,
    ) -> Vec<((usize, usize), f32)> {
        self.composed_weights(procedural_blink, Vrm1ExpressionLookAt::default())
    }

    #[cfg(test)]
    pub(super) fn composed_weights_with_look_at_for_test(
        &self,
        procedural_blink: f32,
        procedural_look_at: Vrm1ExpressionLookAt,
    ) -> Vec<((usize, usize), f32)> {
        self.composed_weights(procedural_blink, procedural_look_at)
    }

    #[cfg(test)]
    fn composed_weights(
        &self,
        procedural_blink: f32,
        procedural_look_at: Vrm1ExpressionLookAt,
    ) -> Vec<((usize, usize), f32)> {
        let effective = self.effective_weights(procedural_blink, procedural_look_at);
        self.composed_morph_weights(&effective)
    }

    #[cfg(test)]
    fn effective_weights_for_test(
        &self,
        procedural_blink: f32,
        procedural_look_at: Vrm1ExpressionLookAt,
    ) -> Vec<f32> {
        self.effective_weights(procedural_blink, procedural_look_at)
    }

    fn effective_weights(
        &self,
        procedural_blink: f32,
        procedural_look_at: Vrm1ExpressionLookAt,
    ) -> Vec<f32> {
        let procedural_blink = if procedural_blink.is_finite() {
            procedural_blink.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let blink_override = self.override_state(ResolvedExpressionClass::Blink, |expression| {
            expression.override_blink
        });
        let look_at_override = self.override_state(ResolvedExpressionClass::LookAt, |expression| {
            expression.override_look_at
        });
        let mouth_override = self.override_state(ResolvedExpressionClass::Mouth, |expression| {
            expression.override_mouth
        });

        let mut effective = Vec::with_capacity(self.expressions.len());
        for (index, expression) in self.expressions.iter().enumerate() {
            let output = match expression.class {
                ResolvedExpressionClass::Other => {
                    expression_output(self.combined_input(index), expression.is_binary)
                }
                ResolvedExpressionClass::Blink => {
                    let procedural = match self.procedural_blink {
                        ProceduralBlinkSource::Preset(source) if source == index => {
                            procedural_blink
                        }
                        ProceduralBlinkSource::Pair(left, right)
                            if left == index || right == index =>
                        {
                            procedural_blink
                        }
                        _ => 0.0,
                    };
                    procedural_output(
                        self.combined_input(index).max(procedural),
                        expression.is_binary,
                        blink_override.multiplier,
                        blink_override.active,
                    )
                }
                ResolvedExpressionClass::LookAt => {
                    let procedural = match expression.name.as_str() {
                        "lookUp" => procedural_look_at.look_up,
                        "lookDown" => procedural_look_at.look_down,
                        "lookLeft" => procedural_look_at.look_left,
                        "lookRight" => procedural_look_at.look_right,
                        _ => 0.0,
                    };
                    let procedural = if procedural.is_finite() {
                        procedural.clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    procedural_output(
                        self.combined_input(index).max(procedural),
                        expression.is_binary,
                        look_at_override.multiplier,
                        look_at_override.active,
                    )
                }
                ResolvedExpressionClass::Mouth => procedural_output(
                    self.combined_input(index),
                    expression.is_binary,
                    mouth_override.multiplier,
                    mouth_override.active,
                ),
            };
            effective.push(output);
        }
        effective
    }

    fn override_state(
        &self,
        target: ResolvedExpressionClass,
        select: impl Fn(&ResolvedExpression) -> Vrm1ExpressionOverride,
    ) -> OverrideState {
        let mut blocked = false;
        let mut blend = 0.0;
        for (index, expression) in self.expressions.iter().enumerate() {
            if expression.class == target {
                continue;
            }
            let output = expression_output(self.combined_input(index), expression.is_binary);
            if output <= 0.0 {
                continue;
            }
            match select(expression) {
                Vrm1ExpressionOverride::None => {}
                Vrm1ExpressionOverride::Block => blocked = true,
                Vrm1ExpressionOverride::Blend => blend += output,
            }
        }
        OverrideState {
            multiplier: if blocked { 0.0 } else { (1.0 - blend).max(0.0) },
            active: blocked || blend > 0.0,
        }
    }

    fn composed_morph_weights(&self, effective: &[f32]) -> Vec<((usize, usize), f32)> {
        let mut totals = BTreeMap::<(usize, usize), f32>::new();
        for (expression, &output) in self.expressions.iter().zip(effective) {
            if output == 0.0 {
                continue;
            }
            for bind in &expression.morph_binds {
                *totals.entry((bind.mesh_slot, bind.target)).or_default() += output * bind.weight;
            }
        }
        totals.into_iter().collect()
    }
}

#[derive(Clone, Copy)]
struct OverrideState {
    multiplier: f32,
    active: bool,
}

fn apply_material_expression(
    states: &mut MaterialStateSet,
    materials: &[MaterialAsset],
    expression: &ResolvedExpression,
    weight: f32,
) {
    if weight == 0.0 {
        return;
    }
    for bind in &expression.material_color_binds {
        let Some(authored_material) = materials.get(bind.material) else {
            continue;
        };
        let authored = authored_material.authored_state();
        let Some(state) = states.get_mut(bind.material) else {
            continue;
        };
        match bind.kind {
            Vrm1MaterialColorBindType::Color => {
                add_base_relative4(
                    &mut state.base_color_factor,
                    authored.base_color_factor,
                    bind.target_value,
                    weight,
                );
            }
            Vrm1MaterialColorBindType::EmissionColor
                if authored_material.kind() != MaterialKind::Unlit =>
            {
                add_base_relative3(
                    &mut state.emissive_factor,
                    authored.emissive_factor,
                    bind.target_value,
                    weight,
                );
            }
            Vrm1MaterialColorBindType::ShadeColor => {
                if let (Some(state), Some(authored)) =
                    (state.mtoon.as_mut(), authored.mtoon.as_ref())
                {
                    add_base_relative3(
                        &mut state.shade_color_factor,
                        authored.shade_color_factor,
                        bind.target_value,
                        weight,
                    );
                }
            }
            Vrm1MaterialColorBindType::MatcapColor => {
                if let (Some(state), Some(authored)) =
                    (state.mtoon.as_mut(), authored.mtoon.as_ref())
                {
                    add_base_relative3(
                        &mut state.matcap_factor,
                        authored.matcap_factor,
                        bind.target_value,
                        weight,
                    );
                }
            }
            Vrm1MaterialColorBindType::RimColor => {
                if let (Some(state), Some(authored)) =
                    (state.mtoon.as_mut(), authored.mtoon.as_ref())
                {
                    add_base_relative3(
                        &mut state.parametric_rim_color_factor,
                        authored.parametric_rim_color_factor,
                        bind.target_value,
                        weight,
                    );
                }
            }
            Vrm1MaterialColorBindType::OutlineColor => {
                if let (Some(state), Some(authored)) =
                    (state.mtoon.as_mut(), authored.mtoon.as_ref())
                {
                    add_base_relative3(
                        &mut state.outline_color_factor,
                        authored.outline_color_factor,
                        bind.target_value,
                        weight,
                    );
                }
            }
            Vrm1MaterialColorBindType::EmissionColor => {}
        }
    }

    for bind in &expression.texture_transform_binds {
        let Some(authored_material) = materials.get(bind.material) else {
            continue;
        };
        let authored = authored_material.authored_state();
        let Some(state) = states.get_mut(bind.material) else {
            continue;
        };
        for (&role, authored_transform) in &authored.texture_transforms {
            if role == TextureRole::Matcap {
                continue;
            }
            let Some(transform) = state.texture_transforms.get_mut(&role) else {
                continue;
            };
            for axis in 0..2 {
                transform.scale[axis] +=
                    (bind.scale[axis] - authored_transform.scale[axis]) * weight;
                transform.offset[axis] +=
                    (bind.offset[axis] - authored_transform.offset[axis]) * weight;
            }
        }
    }
}

fn add_base_relative4(value: &mut [f32; 4], base: [f32; 4], target: [f32; 4], weight: f32) {
    for channel in 0..4 {
        value[channel] += (target[channel] - base[channel]) * weight;
    }
}

fn add_base_relative3(value: &mut [f32; 3], base: [f32; 3], target: [f32; 4], weight: f32) {
    for channel in 0..3 {
        value[channel] += (target[channel] - base[channel]) * weight;
    }
}

fn procedural_output(
    raw_output: f32,
    is_binary: bool,
    multiplier: f32,
    override_active: bool,
) -> f32 {
    if is_binary {
        if override_active {
            0.0
        } else if raw_output > 0.5 {
            1.0
        } else {
            0.0
        }
    } else {
        raw_output * multiplier
    }
}

fn expression_output(input: f32, is_binary: bool) -> f32 {
    if is_binary {
        if input > 0.5 { 1.0 } else { 0.0 }
    } else {
        input
    }
}

fn morph_target_is_usable<I>(target_count: usize, target_index: usize, primitive_targets: I) -> bool
where
    I: IntoIterator<Item = Option<bool>>,
{
    if target_index >= target_count {
        return false;
    }
    let mut any_supported_delta = false;
    for supported_delta in primitive_targets {
        let Some(supported_delta) = supported_delta else {
            return false;
        };
        any_supported_delta |= supported_delta;
    }
    any_supported_delta
}

pub(super) fn resolve_vrm1(
    document: &Vrm1Doc,
    model: &ModelAsset,
) -> Result<ResolvedExpressionRuntime> {
    let mut mesh_node_counts = HashMap::<usize, usize>::new();
    for mesh in document.node_meshes.iter().flatten() {
        *mesh_node_counts.entry(*mesh).or_default() += 1;
    }

    let mut resolved = Vec::new();
    let mut warned_shared_mesh = false;
    let mut warned_invalid_bind = false;
    for expression in &document.expressions {
        let mut morph_binds = Vec::new();
        for bind in &expression.morph_target_binds {
            let Some(gltf_mesh) = document.node_meshes.get(bind.node).copied().flatten() else {
                if !warned_invalid_bind {
                    log::warn!("dropping VRM 1.0 expression bind with a missing or meshless node");
                    warned_invalid_bind = true;
                }
                continue;
            };
            if mesh_node_counts.get(&gltf_mesh).copied().unwrap_or(0) > 1 {
                if !warned_shared_mesh {
                    log::warn!(
                        "dropping VRM 1.0 expression binds for a mesh instanced by multiple nodes"
                    );
                    warned_shared_mesh = true;
                }
                continue;
            }
            if !bind.weight.is_finite() || !(0.0..=1.0).contains(&bind.weight) {
                if !warned_invalid_bind {
                    log::warn!("dropping VRM 1.0 expression bind with an invalid weight");
                    warned_invalid_bind = true;
                }
                continue;
            }
            let Some(mesh_slot) = model.morph_mesh_slot(gltf_mesh) else {
                if !warned_invalid_bind {
                    log::warn!(
                        "dropping VRM 1.0 expression bind for a mesh without imported morph targets"
                    );
                    warned_invalid_bind = true;
                }
                continue;
            };
            let Some(morph_mesh) = model.morph_meshes.get(mesh_slot) else {
                if !warned_invalid_bind {
                    log::warn!("dropping VRM 1.0 expression bind with an invalid morph mesh slot");
                    warned_invalid_bind = true;
                }
                continue;
            };
            let valid = !morph_mesh.prims.is_empty()
                && document.mesh_primitive_counts.get(gltf_mesh).copied()
                    == Some(morph_mesh.prims.len())
                && bind.index < morph_mesh.target_count
                // The target index must exist on every primitive, but a
                // sparse target may legitimately have no supported deltas on
                // an individual primitive. At least one primitive must carry
                // a supported nonzero delta for the bind to be useful.
                && morph_target_is_usable(
                    morph_mesh.target_count,
                    bind.index,
                    morph_mesh.prims.iter().map(|primitive| {
                        primitive.targets.get(bind.index).map(|target| {
                            !target.pos.is_empty() || !target.normal.is_empty()
                        })
                    }),
                );
            if !valid {
                if !warned_invalid_bind {
                    log::warn!(
                        "dropping VRM 1.0 expression bind with an invalid or unsupported morph target"
                    );
                    warned_invalid_bind = true;
                }
                continue;
            }
            morph_binds.push(ResolvedMorphBind {
                mesh_slot,
                target: bind.index,
                weight: bind.weight,
            });
        }

        let class = if expression.kind == Vrm1ExpressionKind::Preset {
            match expression.name.as_str() {
                "blink" | "blinkLeft" | "blinkRight" => ResolvedExpressionClass::Blink,
                "lookUp" | "lookDown" | "lookLeft" | "lookRight" => ResolvedExpressionClass::LookAt,
                "aa" | "ih" | "ou" | "ee" | "oh" => ResolvedExpressionClass::Mouth,
                _ => ResolvedExpressionClass::Other,
            }
        } else {
            ResolvedExpressionClass::Other
        };
        let material_color_binds = expression
            .material_color_binds
            .iter()
            .map(|bind| ResolvedMaterialColorBind {
                material: bind.material,
                kind: bind.kind,
                target_value: bind.target_value,
            })
            .collect::<Vec<_>>();
        let texture_transform_binds = expression
            .texture_transform_binds
            .iter()
            .map(|bind| ResolvedTextureTransformBind {
                material: bind.material,
                scale: bind.scale,
                offset: bind.offset,
            })
            .collect::<Vec<_>>();
        let has_override = expression.override_blink != Vrm1ExpressionOverride::None
            || expression.override_look_at != Vrm1ExpressionOverride::None
            || expression.override_mouth != Vrm1ExpressionOverride::None;
        if morph_binds.is_empty()
            && material_color_binds.is_empty()
            && texture_transform_binds.is_empty()
            && !has_override
        {
            continue;
        }
        resolved.push(ResolvedExpression {
            name: expression.name.clone(),
            is_custom: expression.kind == Vrm1ExpressionKind::Custom,
            morph_binds,
            material_color_binds,
            texture_transform_binds,
            is_binary: expression.is_binary,
            class,
            override_blink: expression.override_blink,
            override_look_at: expression.override_look_at,
            override_mouth: expression.override_mouth,
        });
    }
    Ok(ResolvedExpressionRuntime::Vrm1(Vrm1ExpressionRuntime::new(
        resolved,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pocket3d::material::{
        GltfSampler, MaterialAlphaMode, MaterialInputs, MaterialModel, MtoonMaterial,
        MtoonOutlineWidthMode, ScaledTextureInfo, TextureInfo, TextureTransform,
    };

    fn bind(mesh_slot: usize, target: usize, weight: f32) -> ResolvedMorphBind {
        ResolvedMorphBind {
            mesh_slot,
            target,
            weight,
        }
    }

    fn expression(
        name: &str,
        class: ResolvedExpressionClass,
        is_binary: bool,
        override_blink: Vrm1ExpressionOverride,
        morph_binds: Vec<ResolvedMorphBind>,
    ) -> ResolvedExpression {
        ResolvedExpression {
            name: name.into(),
            is_custom: false,
            morph_binds,
            material_color_binds: Vec::new(),
            texture_transform_binds: Vec::new(),
            is_binary,
            class,
            override_blink,
            override_look_at: Vrm1ExpressionOverride::None,
            override_mouth: Vrm1ExpressionOverride::None,
        }
    }

    fn runtime(expressions: Vec<ResolvedExpression>) -> Vrm1ExpressionRuntime {
        Vrm1ExpressionRuntime::new(expressions)
    }

    fn texture(role: TextureRole, scale: [f32; 2], offset: [f32; 2]) -> TextureInfo {
        TextureInfo {
            texture_index: role as usize,
            image_index: role as usize,
            sampler: GltfSampler::default(),
            tex_coord: 0,
            transform: TextureTransform {
                scale,
                offset,
                rotation: 0.375,
                tex_coord_override: Some(1),
            },
            role,
            color_space: role.color_space(),
        }
    }

    fn mtoon_material() -> MaterialAsset {
        MaterialAsset {
            gltf_material_index: 0,
            name: Some("expression-test".into()),
            inputs: MaterialInputs {
                base_color_factor: [0.2, 0.4, 0.6, 0.8],
                base_color_texture: Some(texture(TextureRole::BaseColor, [2.0, 3.0], [0.1, 0.2])),
                normal_texture: Some(ScaledTextureInfo {
                    texture: texture(TextureRole::Normal, [4.0, 5.0], [-0.2, 0.3]),
                    scale: 1.0,
                }),
                emissive_factor: [0.1, 0.2, 0.3],
                emissive_texture: Some(texture(TextureRole::Emissive, [1.5, 1.25], [0.4, 0.5])),
                alpha_mode: MaterialAlphaMode::Blend,
                alpha_cutoff: 0.5,
                double_sided: false,
            },
            model: MaterialModel::Mtoon(Box::new(MtoonMaterial {
                spec_version: "1.0".into(),
                transparent_with_z_write: false,
                render_queue_offset_number: 0,
                shade_color_factor: [0.3, 0.4, 0.5],
                shade_multiply_texture: Some(texture(
                    TextureRole::ShadeMultiply,
                    [1.1, 1.2],
                    [0.01, 0.02],
                )),
                shading_shift_factor: 0.0,
                shading_shift_texture: Some(ScaledTextureInfo {
                    texture: texture(TextureRole::ShadingShift, [0.8, 0.9], [0.03, 0.04]),
                    scale: 1.0,
                }),
                shading_toony_factor: 0.9,
                gi_equalization_factor: 0.9,
                matcap_factor: [0.4, 0.5, 0.6],
                matcap_texture: Some(texture(TextureRole::Matcap, [9.0, 10.0], [0.9, 1.0])),
                parametric_rim_color_factor: [0.5, 0.6, 0.7],
                parametric_rim_fresnel_power_factor: 5.0,
                parametric_rim_lift_factor: 0.0,
                rim_multiply_texture: Some(texture(
                    TextureRole::RimMultiply,
                    [1.3, 1.4],
                    [0.05, 0.06],
                )),
                rim_lighting_mix_factor: 1.0,
                outline_width_mode: MtoonOutlineWidthMode::WorldCoordinates,
                outline_width_factor: 0.01,
                outline_width_multiply_texture: Some(texture(
                    TextureRole::OutlineWidth,
                    [1.6, 1.7],
                    [0.07, 0.08],
                )),
                outline_color_factor: [0.6, 0.7, 0.8],
                outline_lighting_mix_factor: 1.0,
                uv_animation_mask_texture: Some(texture(
                    TextureRole::UvAnimationMask,
                    [1.8, 1.9],
                    [0.09, 0.1],
                )),
                uv_animation_scroll_x_speed_factor: 0.5,
                uv_animation_scroll_y_speed_factor: -0.25,
                uv_animation_rotation_speed_factor: 0.75,
            })),
        }
    }

    fn with_look_at_override(
        mut expression: ResolvedExpression,
        override_look_at: Vrm1ExpressionOverride,
    ) -> ResolvedExpression {
        expression.override_look_at = override_look_at;
        expression
    }

    fn gaze(name: &str, target: usize, is_binary: bool) -> ResolvedExpression {
        expression(
            name,
            ResolvedExpressionClass::LookAt,
            is_binary,
            Vrm1ExpressionOverride::None,
            vec![bind(0, target, 1.0)],
        )
    }

    #[test]
    fn manual_state_orders_authored_presets_then_document_order_customs() {
        let mut custom_a = expression(
            "customB",
            ResolvedExpressionClass::Other,
            false,
            Vrm1ExpressionOverride::None,
            vec![bind(0, 0, 1.0)],
        );
        custom_a.is_custom = true;
        let mut custom_b = custom_a.clone();
        custom_b.name = "customA".into();
        let runtime = runtime(vec![
            custom_a,
            expression(
                "blink",
                ResolvedExpressionClass::Blink,
                false,
                Vrm1ExpressionOverride::None,
                vec![bind(0, 1, 1.0)],
            ),
            custom_b,
            expression(
                "happy",
                ResolvedExpressionClass::Other,
                false,
                Vrm1ExpressionOverride::None,
                vec![bind(0, 2, 1.0)],
            ),
        ]);
        let state = runtime.manual_state();
        assert_eq!(
            state
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["happy", "blink", "customB", "customA"]
        );
        assert_eq!(
            state.iter().map(|item| item.index).collect::<Vec<_>>(),
            [3, 1, 0, 2]
        );
        assert_eq!(
            state.iter().map(|item| item.custom).collect::<Vec<_>>(),
            [false, false, true, true]
        );
    }

    #[test]
    fn manual_inputs_are_owned_by_each_expression_runtime() {
        let definitions = vec![expression(
            "happy",
            ResolvedExpressionClass::Other,
            false,
            Vrm1ExpressionOverride::None,
            vec![bind(0, 0, 1.0)],
        )];
        let mut first = runtime(definitions.clone());
        let second = runtime(definitions);
        first.set_manual_input(0, 0.8);
        assert_eq!(first.manual_state()[0].weight, 0.8);
        assert_eq!(second.manual_state()[0].weight, 0.0);
    }

    #[test]
    fn manual_source_uses_existing_morph_binary_procedural_and_override_resolution() {
        let mut happy = expression(
            "happy",
            ResolvedExpressionClass::Other,
            false,
            Vrm1ExpressionOverride::None,
            vec![bind(0, 0, 1.0)],
        );
        happy.override_blink = Vrm1ExpressionOverride::Blend;
        happy.override_look_at = Vrm1ExpressionOverride::Blend;
        let mut runtime = runtime(vec![
            happy,
            expression(
                "blink",
                ResolvedExpressionClass::Blink,
                false,
                Vrm1ExpressionOverride::None,
                vec![bind(0, 1, 1.0)],
            ),
            gaze("lookLeft", 2, false),
            expression(
                "binary",
                ResolvedExpressionClass::Other,
                true,
                Vrm1ExpressionOverride::None,
                vec![bind(0, 3, 1.0)],
            ),
        ]);
        assert!(runtime.set_manual_input(0, 0.5));
        assert!(runtime.set_manual_input(3, 0.8));
        assert_eq!(runtime.manual_state()[3].weight, 1.0);
        runtime.set_input("happy", 0.25);
        assert_eq!(
            runtime.composed_weights_with_look_at_for_test(
                0.8,
                Vrm1ExpressionLookAt {
                    look_left: 0.6,
                    ..Default::default()
                }
            ),
            vec![((0, 0), 0.5), ((0, 1), 0.4), ((0, 2), 0.3), ((0, 3), 1.0)]
        );
        assert!(runtime.reset_manual_inputs());
        assert_eq!(
            runtime
                .manual_state()
                .iter()
                .map(|item| item.weight)
                .collect::<Vec<_>>(),
            [0.0; 4]
        );
        assert_eq!(
            runtime.composed_weights_with_look_at_for_test(
                0.8,
                Vrm1ExpressionLookAt {
                    look_left: 0.6,
                    ..Default::default()
                }
            ),
            vec![((0, 0), 0.25), ((0, 1), 0.6), ((0, 2), 0.45000002)]
        );
        assert!(!runtime.reset_manual_inputs());
    }

    #[test]
    fn manual_material_and_texture_bindings_use_stage_g_application() {
        let materials = vec![mtoon_material()];
        let mut states = MaterialStateSet::from_assets(&materials);
        let authored = states.get(0).unwrap().clone();
        let mut expression = expression(
            "custom",
            ResolvedExpressionClass::Other,
            false,
            Vrm1ExpressionOverride::None,
            Vec::new(),
        );
        expression
            .material_color_binds
            .push(ResolvedMaterialColorBind {
                material: 0,
                kind: Vrm1MaterialColorBindType::Color,
                target_value: [1.0, 0.0, 0.0, 0.4],
            });
        expression
            .texture_transform_binds
            .push(ResolvedTextureTransformBind {
                material: 0,
                scale: [2.0, 2.0],
                offset: [0.5, 0.0],
            });
        let mut runtime = runtime(vec![expression]);
        runtime.set_manual_input(0, 0.5);
        let weight = runtime.effective_weights_for_test(0.0, Vrm1ExpressionLookAt::default())[0];
        apply_material_expression(&mut states, &materials, &runtime.expressions[0], weight);
        assert_ne!(
            states.get(0).unwrap().base_color_factor,
            authored.base_color_factor
        );
        assert_ne!(
            states.get(0).unwrap().texture_transforms,
            authored.texture_transforms
        );
    }

    #[test]
    fn binary_input_uses_strict_threshold_and_duplicate_binds_accumulate() {
        let mut runtime = runtime(vec![expression(
            "smile",
            ResolvedExpressionClass::Other,
            true,
            Vrm1ExpressionOverride::None,
            vec![bind(0, 0, 0.75), bind(0, 0, 0.5)],
        )]);
        runtime.set_input("smile", 0.5);
        assert!(runtime.composed_weights_for_test(0.0).is_empty());
        runtime.set_input("smile", 0.5001);
        assert_eq!(runtime.composed_weights_for_test(0.0), vec![((0, 0), 1.25)]);
    }

    #[test]
    fn morph_target_with_zero_delta_primitive_remains_usable() {
        assert!(morph_target_is_usable(
            2,
            1,
            [Some(false), Some(true), Some(false)]
        ));
        assert!(!morph_target_is_usable(2, 1, [Some(false), None]));
        assert!(!morph_target_is_usable(2, 2, [Some(true)]));
    }

    #[test]
    fn binary_blink_thresholds_combined_guest_and_procedural_input() {
        let mut runtime = runtime(vec![expression(
            "blink",
            ResolvedExpressionClass::Blink,
            true,
            Vrm1ExpressionOverride::None,
            vec![bind(0, 0, 1.0)],
        )]);

        runtime.set_input("blink", 0.5);
        assert!(runtime.composed_weights_for_test(0.0).is_empty());

        runtime.set_input("blink", 0.0);
        assert!(runtime.composed_weights_for_test(0.5).is_empty());
        assert_eq!(
            runtime.composed_weights_for_test(0.5001),
            vec![((0, 0), 1.0)]
        );
    }

    #[test]
    fn blink_uses_max_and_override_blend_saturates_without_partial_binary_output() {
        let mut runtime = runtime(vec![
            expression(
                "blink",
                ResolvedExpressionClass::Blink,
                true,
                Vrm1ExpressionOverride::None,
                vec![bind(0, 0, 1.0)],
            ),
            expression(
                "surprised",
                ResolvedExpressionClass::Other,
                false,
                Vrm1ExpressionOverride::Blend,
                vec![bind(0, 1, 1.0)],
            ),
        ]);
        runtime.set_input("blink", 0.6);
        runtime.set_input("surprised", 0.25);
        assert_eq!(runtime.composed_weights_for_test(0.8), vec![((0, 1), 0.25)]);
        runtime.set_input("surprised", 1.0);
        assert_eq!(runtime.composed_weights_for_test(0.8), vec![((0, 1), 1.0)]);
    }

    #[test]
    fn left_and_right_blink_pair_share_procedural_input() {
        let runtime = runtime(vec![
            expression(
                "blinkLeft",
                ResolvedExpressionClass::Blink,
                false,
                Vrm1ExpressionOverride::None,
                vec![bind(0, 0, 1.0)],
            ),
            expression(
                "blinkRight",
                ResolvedExpressionClass::Blink,
                false,
                Vrm1ExpressionOverride::None,
                vec![bind(0, 1, 1.0)],
            ),
        ]);
        assert_eq!(
            runtime.composed_weights_for_test(0.4),
            vec![((0, 0), 0.4), ((0, 1), 0.4)]
        );
    }

    #[test]
    fn inputs_are_clamped_unknown_inputs_are_ignored_and_nonfinite_inputs_are_safe() {
        let mut runtime = runtime(vec![expression(
            "smile",
            ResolvedExpressionClass::Other,
            false,
            Vrm1ExpressionOverride::None,
            vec![bind(0, 0, 1.0)],
        )]);

        assert!(runtime.set_input("smile", 2.0));
        assert_eq!(runtime.composed_weights_for_test(0.0), vec![((0, 0), 1.0)]);
        assert!(!runtime.set_input("unknown", 1.0));
        assert!(!runtime.set_input("smile", f32::NAN));
        assert_eq!(runtime.composed_weights_for_test(0.0), vec![((0, 0), 1.0)]);
    }

    #[test]
    fn gaze_directions_are_procedural_and_opposites_do_not_stay_stale() {
        let runtime = runtime(vec![
            gaze("lookUp", 0, false),
            gaze("lookDown", 1, false),
            gaze("lookLeft", 2, false),
            gaze("lookRight", 3, false),
        ]);
        assert_eq!(
            runtime.composed_weights_with_look_at_for_test(
                0.0,
                Vrm1ExpressionLookAt {
                    look_up: 0.25,
                    look_left: 0.75,
                    ..Default::default()
                },
            ),
            vec![((0, 0), 0.25), ((0, 2), 0.75)]
        );
        assert_eq!(
            runtime.composed_weights_with_look_at_for_test(
                0.0,
                Vrm1ExpressionLookAt {
                    look_down: 0.5,
                    look_right: 1.0,
                    ..Default::default()
                },
            ),
            vec![((0, 1), 0.5), ((0, 3), 1.0)]
        );
        assert!(
            runtime
                .composed_weights_with_look_at_for_test(0.0, Vrm1ExpressionLookAt::default())
                .is_empty()
        );
    }

    #[test]
    fn look_at_override_none_block_and_blend_follow_expression_output() {
        let make_runtime = |override_look_at| {
            let mut runtime = runtime(vec![
                gaze("lookLeft", 0, false),
                with_look_at_override(
                    expression(
                        "happy",
                        ResolvedExpressionClass::Other,
                        false,
                        Vrm1ExpressionOverride::None,
                        vec![bind(0, 1, 1.0)],
                    ),
                    override_look_at,
                ),
            ]);
            runtime.set_input("happy", 0.25);
            runtime
        };
        let look_at = Vrm1ExpressionLookAt {
            look_left: 0.8,
            ..Default::default()
        };
        assert_eq!(
            make_runtime(Vrm1ExpressionOverride::None)
                .composed_weights_with_look_at_for_test(0.0, look_at),
            vec![((0, 0), 0.8), ((0, 1), 0.25)]
        );
        assert_eq!(
            make_runtime(Vrm1ExpressionOverride::Block)
                .composed_weights_with_look_at_for_test(0.0, look_at),
            vec![((0, 1), 0.25)]
        );
        assert_eq!(
            make_runtime(Vrm1ExpressionOverride::Blend)
                .composed_weights_with_look_at_for_test(0.0, look_at),
            vec![((0, 0), 0.6), ((0, 1), 0.25)]
        );
    }

    #[test]
    fn binary_overrider_and_binary_gaze_use_visual_outputs() {
        let mut runtime = runtime(vec![
            gaze("lookLeft", 0, true),
            with_look_at_override(
                expression(
                    "angry",
                    ResolvedExpressionClass::Other,
                    true,
                    Vrm1ExpressionOverride::None,
                    vec![bind(0, 1, 1.0)],
                ),
                Vrm1ExpressionOverride::Blend,
            ),
        ]);
        let look_at = Vrm1ExpressionLookAt {
            look_left: 0.75,
            ..Default::default()
        };
        runtime.set_input("angry", 0.5);
        assert_eq!(
            runtime.composed_weights_with_look_at_for_test(0.0, look_at),
            vec![((0, 0), 1.0)]
        );
        runtime.set_input("angry", 0.5001);
        assert_eq!(
            runtime.composed_weights_with_look_at_for_test(0.0, look_at),
            vec![((0, 1), 1.0)]
        );
    }

    #[test]
    fn blink_and_gaze_accumulate_on_a_shared_morph_target() {
        let runtime = runtime(vec![
            expression(
                "blink",
                ResolvedExpressionClass::Blink,
                false,
                Vrm1ExpressionOverride::None,
                vec![bind(0, 0, 1.0)],
            ),
            gaze("lookDown", 0, false),
        ]);
        assert_eq!(
            runtime.composed_weights_with_look_at_for_test(
                0.4,
                Vrm1ExpressionLookAt {
                    look_down: 0.3,
                    ..Default::default()
                },
            ),
            vec![((0, 0), 0.70000005)]
        );
    }

    fn assert_close<const N: usize>(actual: [f32; N], expected: [f32; N]) {
        for index in 0..N {
            assert!(
                (actual[index] - expected[index]).abs() < 1e-6,
                "channel {index}: {} != {}",
                actual[index],
                expected[index]
            );
        }
    }

    #[test]
    fn material_colors_accumulate_base_relative_deltas_without_clamping() {
        let materials = vec![mtoon_material()];
        let mut states = MaterialStateSet::from_assets(&materials);
        let mut a = expression(
            "a",
            ResolvedExpressionClass::Other,
            false,
            Vrm1ExpressionOverride::None,
            Vec::new(),
        );
        a.material_color_binds = vec![
            ResolvedMaterialColorBind {
                material: 0,
                kind: Vrm1MaterialColorBindType::Color,
                target_value: [1.0, 0.0, 0.0, 0.4],
            },
            ResolvedMaterialColorBind {
                material: 0,
                kind: Vrm1MaterialColorBindType::EmissionColor,
                target_value: [1.0, 1.0, 1.0, 99.0],
            },
            ResolvedMaterialColorBind {
                material: 0,
                kind: Vrm1MaterialColorBindType::ShadeColor,
                target_value: [1.0, 1.0, 1.0, 99.0],
            },
            ResolvedMaterialColorBind {
                material: 0,
                kind: Vrm1MaterialColorBindType::MatcapColor,
                target_value: [1.0, 1.0, 1.0, 99.0],
            },
            ResolvedMaterialColorBind {
                material: 0,
                kind: Vrm1MaterialColorBindType::RimColor,
                target_value: [1.0, 1.0, 1.0, 99.0],
            },
            ResolvedMaterialColorBind {
                material: 0,
                kind: Vrm1MaterialColorBindType::OutlineColor,
                target_value: [1.0, 1.0, 1.0, 99.0],
            },
        ];
        let mut b = a.clone();
        b.name = "b".into();
        b.material_color_binds[0].target_value = [0.0, 1.0, 0.0, 0.2];

        apply_material_expression(&mut states, &materials, &a, 0.25);
        apply_material_expression(&mut states, &materials, &b, 0.5);
        let state = states.get(0).unwrap();
        assert_close(state.base_color_factor, [0.3, 0.6, 0.15, 0.4]);
        assert_close(state.emissive_factor, [0.775, 0.8, 0.825]);
        let mtoon = state.mtoon.as_ref().unwrap();
        assert_close(mtoon.shade_color_factor, [0.825, 0.85, 0.875]);
        assert_close(mtoon.matcap_factor, [0.85, 0.875, 0.9]);
        assert_close(mtoon.parametric_rim_color_factor, [0.875, 0.9, 0.925]);
        assert_close(mtoon.outline_color_factor, [0.9, 0.925, 0.95]);

        let forward = state.clone();
        states.reset_from_assets(&materials);
        apply_material_expression(&mut states, &materials, &b, 0.5);
        apply_material_expression(&mut states, &materials, &a, 0.25);
        let reverse = states.get(0).unwrap();
        assert_close(reverse.base_color_factor, forward.base_color_factor);
        assert_close(reverse.emissive_factor, forward.emissive_factor);
        assert_close(
            reverse.mtoon.as_ref().unwrap().shade_color_factor,
            forward.mtoon.as_ref().unwrap().shade_color_factor,
        );

        states.reset_from_assets(&materials);
        apply_material_expression(&mut states, &materials, &a, 1.0);
        apply_material_expression(&mut states, &materials, &a, 1.0);
        assert!(states.get(0).unwrap().base_color_factor[0] > 1.0);
    }

    #[test]
    fn texture_transforms_use_each_texture_base_and_exclude_matcap() {
        let materials = vec![mtoon_material()];
        let mut states = MaterialStateSet::from_assets(&materials);
        let authored = states.get(0).unwrap().clone();
        let mut a = expression(
            "a",
            ResolvedExpressionClass::Other,
            false,
            Vrm1ExpressionOverride::None,
            Vec::new(),
        );
        a.texture_transform_binds = vec![ResolvedTextureTransformBind {
            material: 0,
            scale: [1.0, 1.0],
            offset: [0.0, 0.0],
        }];
        let mut b = a.clone();
        b.name = "b".into();
        b.texture_transform_binds[0].scale = [3.0, 2.0];
        b.texture_transform_binds[0].offset = [0.5, -0.5];

        apply_material_expression(&mut states, &materials, &a, 0.25);
        apply_material_expression(&mut states, &materials, &b, 0.5);
        let state = states.get(0).unwrap();
        let base = state
            .texture_transforms
            .get(&TextureRole::BaseColor)
            .unwrap();
        assert_close(base.scale, [2.25, 2.0]);
        assert_close(base.offset, [0.275, -0.2]);
        assert_eq!(base.rotation, 0.375);
        assert_eq!(base.tex_coord_override, Some(1));
        let normal = state.texture_transforms.get(&TextureRole::Normal).unwrap();
        assert_close(normal.scale, [2.75, 2.5]);
        assert_close(normal.offset, [0.2, -0.175]);
        assert_eq!(
            state.texture_transforms.get(&TextureRole::Matcap),
            authored.texture_transforms.get(&TextureRole::Matcap)
        );
        for role in [
            TextureRole::BaseColor,
            TextureRole::Normal,
            TextureRole::Emissive,
            TextureRole::ShadeMultiply,
            TextureRole::ShadingShift,
            TextureRole::RimMultiply,
            TextureRole::OutlineWidth,
            TextureRole::UvAnimationMask,
        ] {
            assert_ne!(
                state.texture_transforms.get(&role),
                authored.texture_transforms.get(&role),
                "{role:?}"
            );
        }

        let forward = state.clone();
        states.reset_from_assets(&materials);
        apply_material_expression(&mut states, &materials, &b, 0.5);
        apply_material_expression(&mut states, &materials, &a, 0.25);
        let reverse = states.get(0).unwrap();
        for (&role, forward_transform) in &forward.texture_transforms {
            let reverse_transform = reverse.texture_transforms.get(&role).unwrap();
            assert_close(reverse_transform.scale, forward_transform.scale);
            assert_close(reverse_transform.offset, forward_transform.offset);
            assert_eq!(reverse_transform.rotation, forward_transform.rotation);
            assert_eq!(
                reverse_transform.tex_coord_override,
                forward_transform.tex_coord_override
            );
        }

        states.reset_from_assets(&materials);
        assert_eq!(states.get(0).unwrap(), &authored);
        apply_material_expression(&mut states, &materials, &b, 1.0);
        for role in [
            TextureRole::BaseColor,
            TextureRole::Normal,
            TextureRole::Emissive,
            TextureRole::ShadeMultiply,
            TextureRole::ShadingShift,
            TextureRole::RimMultiply,
            TextureRole::OutlineWidth,
            TextureRole::UvAnimationMask,
        ] {
            let transform = states
                .get(0)
                .unwrap()
                .texture_transforms
                .get(&role)
                .unwrap();
            assert_close(transform.scale, [3.0, 2.0]);
            assert_close(transform.offset, [0.5, -0.5]);
            assert_eq!(transform.rotation, 0.375, "{role:?}");
            assert_eq!(transform.tex_coord_override, Some(1), "{role:?}");
        }
    }

    #[test]
    fn material_only_effects_use_binary_procedural_and_override_outputs() {
        let color_bind = ResolvedMaterialColorBind {
            material: 0,
            kind: Vrm1MaterialColorBindType::Color,
            target_value: [1.0; 4],
        };
        let mut binary = expression(
            "materialOnly",
            ResolvedExpressionClass::Other,
            true,
            Vrm1ExpressionOverride::None,
            Vec::new(),
        );
        binary.material_color_binds.push(color_bind.clone());
        let mut gaze = expression(
            "lookLeft",
            ResolvedExpressionClass::LookAt,
            false,
            Vrm1ExpressionOverride::None,
            Vec::new(),
        );
        gaze.material_color_binds.push(color_bind.clone());
        let mut mouth = expression(
            "aa",
            ResolvedExpressionClass::Mouth,
            false,
            Vrm1ExpressionOverride::None,
            Vec::new(),
        );
        mouth.material_color_binds.push(color_bind);
        let mut overrider = expression(
            "happy",
            ResolvedExpressionClass::Other,
            false,
            Vrm1ExpressionOverride::None,
            Vec::new(),
        );
        overrider.override_mouth = Vrm1ExpressionOverride::Blend;
        let mut runtime = runtime(vec![binary, gaze, mouth, overrider]);
        assert_eq!(runtime.capability_names().len(), 4);
        runtime.set_input("materialOnly", 0.5001);
        runtime.set_input("aa", 0.8);
        runtime.set_input("happy", 0.25);
        assert_eq!(
            runtime.effective_weights_for_test(
                0.0,
                Vrm1ExpressionLookAt {
                    look_left: 0.35,
                    ..Default::default()
                }
            ),
            vec![1.0, 0.35, 0.6, 0.25]
        );
    }
}
