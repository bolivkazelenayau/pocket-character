//! Parent-owned VRM expression resolution and composition.
//!
//! VRM 0.x intentionally keeps its existing direct-write behavior in
//! `widget.rs`.  This module contains only the additive VRM 1.0 path: parser
//! semantics are resolved against the imported ModelAsset during candidate
//! preparation, then persistent inputs are composed into per-instance morph
//! weights at runtime.

use std::collections::{BTreeMap, HashMap};

use anyhow::Result;
use pocket_vrm::{Vrm1Doc, Vrm1ExpressionKind, Vrm1ExpressionOverride};
use pocket3d::model::ModelAsset;
use pocket3d::scene::Scene;

use super::avatar::AvatarSceneSlot;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ResolvedExpressionClass {
    Blink,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum UnsupportedExpressionPart {
    MaterialColorBinds,
    TextureTransformBinds,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct ResolvedMorphBind {
    pub(super) mesh_slot: usize,
    pub(super) target: usize,
    pub(super) weight: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct ResolvedExpression {
    pub(super) name: String,
    pub(super) morph_binds: Vec<ResolvedMorphBind>,
    pub(super) is_binary: bool,
    pub(super) class: ResolvedExpressionClass,
    pub(super) override_blink: Vrm1ExpressionOverride,
    pub(super) override_look_at: Vrm1ExpressionOverride,
    pub(super) override_mouth: Vrm1ExpressionOverride,
    pub(super) unsupported_parts: Vec<UnsupportedExpressionPart>,
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
    inputs: Vec<f32>,
    managed_targets: Vec<(usize, usize)>,
    procedural_blink: ProceduralBlinkSource,
    dirty: bool,
    last_procedural_blink: f32,
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
            expressions,
            managed_targets,
            procedural_blink,
            dirty: false,
            // NaN makes the first procedural sample observable without
            // assigning an artificial blink value to the rest pose.
            last_procedural_blink: f32::NAN,
            warned_nonfinite_input: false,
        }
    }

    pub(super) fn capability_names(&self) -> Vec<String> {
        self.expressions
            .iter()
            .map(|expression| expression.name.clone())
            .collect()
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
        if !self.dirty && self.last_procedural_blink == procedural_blink {
            return;
        }
        let assignments = self.composed_weights(procedural_blink);
        let instance = scene_slot.get_mut(scene);
        let Some(morph) = instance.morph.as_mut() else {
            self.dirty = false;
            self.last_procedural_blink = procedural_blink;
            return;
        };

        // Rewriting the complete managed set prevents a dropped expression or
        // a new command batch from leaving stale weights behind. Unrelated
        // morph targets are never touched.
        for &(mesh_slot, target) in &self.managed_targets {
            morph.set_weight(mesh_slot, target, 0.0);
        }
        for ((mesh_slot, target), weight) in assignments {
            morph.set_weight(mesh_slot, target, weight);
        }
        self.dirty = false;
        self.last_procedural_blink = procedural_blink;
    }

    #[cfg(test)]
    pub(super) fn composed_weights_for_test(
        &self,
        procedural_blink: f32,
    ) -> Vec<((usize, usize), f32)> {
        self.composed_weights(procedural_blink)
    }

    fn composed_weights(&self, procedural_blink: f32) -> Vec<((usize, usize), f32)> {
        let procedural_blink = if procedural_blink.is_finite() {
            procedural_blink.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let mut override_blocks = false;
        let mut override_blend = 0.0;
        for (index, expression) in self.expressions.iter().enumerate() {
            if expression.class == ResolvedExpressionClass::Blink {
                continue;
            }
            let output = expression_output(self.inputs[index], expression.is_binary);
            if output <= 0.0 {
                continue;
            }
            match expression.override_blink {
                Vrm1ExpressionOverride::None => {}
                Vrm1ExpressionOverride::Block => override_blocks = true,
                Vrm1ExpressionOverride::Blend => override_blend += output,
            }
        }
        let blink_multiplier = if override_blocks {
            0.0
        } else {
            (1.0 - override_blend).max(0.0)
        };
        let blink_override_active = override_blocks || override_blend > 0.0;

        let mut totals = BTreeMap::<(usize, usize), f32>::new();
        for (index, expression) in self.expressions.iter().enumerate() {
            let mut output = if expression.class == ResolvedExpressionClass::Blink {
                0.0
            } else {
                expression_output(self.inputs[index], expression.is_binary)
            };
            if expression.class == ResolvedExpressionClass::Blink {
                let procedural = match self.procedural_blink {
                    ProceduralBlinkSource::Preset(source) if source == index => procedural_blink,
                    ProceduralBlinkSource::Pair(left, right) if left == index || right == index => {
                        procedural_blink
                    }
                    _ => 0.0,
                };
                // Guest and procedural blink inputs are combined before VRM1's
                // binary threshold is applied. This keeps binary blink from
                // treating any positive procedural input as a full close.
                let raw_output = self.inputs[index].max(procedural);
                if expression.is_binary {
                    output = if raw_output > 0.5 { 1.0 } else { 0.0 };
                    if blink_override_active {
                        output = 0.0;
                    }
                } else {
                    output = raw_output * blink_multiplier;
                }
            }
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
    let mut warned_unsupported = false;
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

        let mut unsupported_parts = Vec::new();
        if expression.has_material_color_binds {
            unsupported_parts.push(UnsupportedExpressionPart::MaterialColorBinds);
        }
        if expression.has_texture_transform_binds {
            unsupported_parts.push(UnsupportedExpressionPart::TextureTransformBinds);
        }
        if !unsupported_parts.is_empty() && !warned_unsupported {
            log::warn!(
                "VRM 1.0 material/texture expression binds are unsupported; retaining resolved morph binds"
            );
            warned_unsupported = true;
        }
        if morph_binds.is_empty() {
            continue;
        }

        let class = if expression.kind == Vrm1ExpressionKind::Preset
            && matches!(
                expression.name.as_str(),
                "blink" | "blinkLeft" | "blinkRight"
            ) {
            ResolvedExpressionClass::Blink
        } else {
            ResolvedExpressionClass::Other
        };
        resolved.push(ResolvedExpression {
            name: expression.name.clone(),
            morph_binds,
            is_binary: expression.is_binary,
            class,
            override_blink: expression.override_blink,
            override_look_at: expression.override_look_at,
            override_mouth: expression.override_mouth,
            unsupported_parts,
        });
    }
    Ok(ResolvedExpressionRuntime::Vrm1(Vrm1ExpressionRuntime::new(
        resolved,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

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
            morph_binds,
            is_binary,
            class,
            override_blink,
            override_look_at: Vrm1ExpressionOverride::None,
            override_mouth: Vrm1ExpressionOverride::None,
            unsupported_parts: Vec::new(),
        }
    }

    fn runtime(expressions: Vec<ResolvedExpression>) -> Vrm1ExpressionRuntime {
        Vrm1ExpressionRuntime::new(expressions)
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
}
