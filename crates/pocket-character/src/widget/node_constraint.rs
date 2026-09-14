//! Prepared VRM 1.0 node-constraint scheduling and evaluation.
//!
//! Parsing and pure quaternion operations live in `pocket-vrm`. This host-side
//! runtime resolves immutable skeleton data once, builds the semantic
//! dependency graph once, and evaluates only structurally valid constraints.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashSet};

use anyhow::{Result, bail, ensure};
use glam::{Mat4, Quat};
use pocket_vrm::{
    Vrm1NodeConstraintKind, Vrm1NodeConstraintSet, aim_axis_vector, aim_constraint,
    checked_normalize_quat, roll_axis_vector, roll_constraint, rotation_constraint,
};
use pocket3d::anim::{NodeTrs, Skeleton};

#[derive(Clone, Copy, Debug)]
enum ResolvedConstraintKind {
    Rotation {
        source_rest: Quat,
        destination_rest: Quat,
        weight: f32,
    },
    Roll {
        source_rest: Quat,
        destination_rest: Quat,
        axis: glam::Vec3,
        weight: f32,
    },
    Aim {
        destination_parent: Option<usize>,
        destination_rest: Quat,
        axis: glam::Vec3,
        weight: f32,
    },
}

#[derive(Clone, Copy, Debug)]
struct ResolvedConstraint {
    source: usize,
    destination: usize,
    kind: ResolvedConstraintKind,
}

impl ResolvedConstraint {
    fn consumed_nodes(self) -> [Option<usize>; 2] {
        let destination_parent = match self.kind {
            ResolvedConstraintKind::Aim {
                destination_parent, ..
            } => destination_parent,
            ResolvedConstraintKind::Rotation { .. } | ResolvedConstraintKind::Roll { .. } => None,
        };
        [Some(self.source), destination_parent]
    }
}

/// Fully resolved, deterministically ordered VRMC_node_constraint runtime.
///
/// Indices, rest rotations, axes, weights, destination parents, and graph
/// ordering are immutable after construction. `global_rotations` is the only
/// per-frame scratch state and deliberately excludes the instance presentation
/// transform.
#[derive(Debug)]
pub(super) struct Vrm1NodeConstraintRuntime {
    ordered: Vec<ResolvedConstraint>,
    global_rotations: Vec<Quat>,
}

impl Vrm1NodeConstraintRuntime {
    pub(super) fn new(semantics: &Vrm1NodeConstraintSet, skeleton: &Skeleton) -> Result<Self> {
        validate_skeleton(skeleton)?;

        let mut destinations = HashSet::with_capacity(semantics.constraints.len());
        let mut resolved = Vec::with_capacity(semantics.constraints.len());
        for (parse_order, constraint) in semantics.constraints.iter().copied().enumerate() {
            let destination = constraint.destination;
            ensure!(
                destination < skeleton.rest.len(),
                "VRMC_node_constraint destination {destination} is out of range for {} nodes",
                skeleton.rest.len()
            );
            ensure!(
                destinations.insert(destination),
                "VRMC_node_constraint destination {destination} is declared more than once"
            );

            let (source, kind) = match constraint.kind {
                Vrm1NodeConstraintKind::Rotation { source, weight } => {
                    let (source_rest, destination_rest) =
                        resolve_common(skeleton, source, destination, weight, parse_order)?;
                    (
                        source,
                        ResolvedConstraintKind::Rotation {
                            source_rest,
                            destination_rest,
                            weight,
                        },
                    )
                }
                Vrm1NodeConstraintKind::Roll {
                    source,
                    axis,
                    weight,
                } => {
                    let (source_rest, destination_rest) =
                        resolve_common(skeleton, source, destination, weight, parse_order)?;
                    (
                        source,
                        ResolvedConstraintKind::Roll {
                            source_rest,
                            destination_rest,
                            axis: roll_axis_vector(axis),
                            weight,
                        },
                    )
                }
                Vrm1NodeConstraintKind::Aim {
                    source,
                    axis,
                    weight,
                } => {
                    let (_, destination_rest) =
                        resolve_common(skeleton, source, destination, weight, parse_order)?;
                    let parent = skeleton.parents[destination];
                    (
                        source,
                        ResolvedConstraintKind::Aim {
                            destination_parent: (parent != usize::MAX).then_some(parent),
                            destination_rest,
                            axis: aim_axis_vector(axis),
                            weight,
                        },
                    )
                }
            };
            resolved.push(ResolvedConstraint {
                source,
                destination,
                kind,
            });
        }

        let order = stable_constraint_order(&resolved, skeleton)?;
        let ordered = order.into_iter().map(|index| resolved[index]).collect();
        Ok(Self {
            ordered,
            global_rotations: vec![Quat::IDENTITY; skeleton.rest.len()],
        })
    }

    pub(super) fn is_empty(&self) -> bool {
        self.ordered.is_empty()
    }

    /// Evaluate the prepared schedule in authored/model skeleton space.
    ///
    /// A complete matrix and quaternion hierarchy refresh is performed before
    /// the first constraint and after every destination write. This makes bone
    /// LookAt and earlier constraints visible to Aim and intentionally leaves
    /// subtree invalidation as a future optimization.
    pub(super) fn evaluate(
        &mut self,
        skeleton: &Skeleton,
        locals: &mut [NodeTrs],
        globals: &mut Vec<Mat4>,
    ) {
        debug_assert_eq!(locals.len(), skeleton.rest.len());
        debug_assert_eq!(self.global_rotations.len(), skeleton.rest.len());
        refresh_pose(skeleton, locals, globals, &mut self.global_rotations);

        for constraint in &self.ordered {
            let output = match constraint.kind {
                ResolvedConstraintKind::Rotation {
                    source_rest,
                    destination_rest,
                    weight,
                } => rotation_constraint(
                    source_rest,
                    locals[constraint.source].rotation,
                    destination_rest,
                    weight,
                ),
                ResolvedConstraintKind::Roll {
                    source_rest,
                    destination_rest,
                    axis,
                    weight,
                } => roll_constraint(
                    source_rest,
                    locals[constraint.source].rotation,
                    destination_rest,
                    axis,
                    weight,
                ),
                ResolvedConstraintKind::Aim {
                    destination_parent,
                    destination_rest,
                    axis,
                    weight,
                } => {
                    let source_position = globals[constraint.source].w_axis.truncate();
                    let destination_position = globals[constraint.destination].w_axis.truncate();
                    let parent_rotation = destination_parent
                        .map(|parent| self.global_rotations[parent])
                        .unwrap_or(Quat::IDENTITY);
                    aim_constraint(
                        source_position,
                        destination_position,
                        parent_rotation,
                        destination_rest,
                        axis,
                        weight,
                    )
                }
            };

            locals[constraint.destination].rotation = output;
            refresh_pose(skeleton, locals, globals, &mut self.global_rotations);
        }
    }

    #[cfg(test)]
    pub(super) fn ordered_destinations(&self) -> Vec<usize> {
        self.ordered
            .iter()
            .map(|constraint| constraint.destination)
            .collect()
    }

    #[cfg(test)]
    pub(super) fn kind_counts(&self) -> (usize, usize, usize) {
        let mut roll = 0;
        let mut aim = 0;
        let mut rotation = 0;
        for constraint in &self.ordered {
            match constraint.kind {
                ResolvedConstraintKind::Roll { .. } => roll += 1,
                ResolvedConstraintKind::Aim { .. } => aim += 1,
                ResolvedConstraintKind::Rotation { .. } => rotation += 1,
            }
        }
        (roll, aim, rotation)
    }
}

fn resolve_common(
    skeleton: &Skeleton,
    source: usize,
    destination: usize,
    weight: f32,
    parse_order: usize,
) -> Result<(Quat, Quat)> {
    ensure!(
        source < skeleton.rest.len(),
        "VRMC_node_constraint {parse_order} source {source} is out of range for {} nodes",
        skeleton.rest.len()
    );
    ensure!(
        source != destination,
        "VRMC_node_constraint {parse_order} source {source} equals its destination"
    );
    ensure!(
        weight.is_finite() && (0.0..=1.0).contains(&weight),
        "VRMC_node_constraint {parse_order} weight must be finite and within 0..=1"
    );
    let source_rest = checked_normalize_quat(skeleton.rest[source].rotation).ok_or_else(|| {
        anyhow::anyhow!(
            "VRMC_node_constraint {parse_order} source {source} has an invalid rest rotation"
        )
    })?;
    let destination_rest = checked_normalize_quat(skeleton.rest[destination].rotation)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "VRMC_node_constraint {parse_order} destination {destination} has an invalid rest rotation"
            )
        })?;
    Ok((source_rest, destination_rest))
}

fn validate_skeleton(skeleton: &Skeleton) -> Result<()> {
    let node_count = skeleton.rest.len();
    ensure!(
        skeleton.parents.len() == node_count,
        "constraint skeleton parent/rest length mismatch"
    );
    ensure!(
        skeleton.order.len() == node_count,
        "constraint skeleton order/rest length mismatch"
    );
    let mut seen = vec![false; node_count];
    for &node in &skeleton.order {
        ensure!(
            node < node_count,
            "constraint skeleton order contains node {node} out of range"
        );
        ensure!(!seen[node], "constraint skeleton order repeats node {node}");
        let parent = skeleton.parents[node];
        ensure!(
            parent == usize::MAX || parent < node_count,
            "constraint skeleton node {node} has parent {parent} out of range"
        );
        ensure!(
            parent == usize::MAX || seen[parent],
            "constraint skeleton parent {parent} does not precede child {node}"
        );
        seen[node] = true;
    }
    Ok(())
}

fn ancestor_or_self(skeleton: &Skeleton, ancestor: usize, mut node: usize) -> bool {
    for _ in 0..=skeleton.parents.len() {
        if node == ancestor {
            return true;
        }
        let parent = skeleton.parents[node];
        if parent == usize::MAX {
            return false;
        }
        node = parent;
    }
    false
}

fn stable_constraint_order(
    constraints: &[ResolvedConstraint],
    skeleton: &Skeleton,
) -> Result<Vec<usize>> {
    let count = constraints.len();
    let mut edges = vec![vec![false; count]; count];
    let mut indegree = vec![0usize; count];

    for (index, constraint) in constraints.iter().copied().enumerate() {
        for consumed in constraint.consumed_nodes().into_iter().flatten() {
            if ancestor_or_self(skeleton, constraint.destination, consumed) {
                bail!(
                    "VRMC_node_constraint destination {} is ancestor-or-self of its own consumed node {consumed}",
                    constraint.destination
                );
            }
        }

        for (dependency, producer) in constraints.iter().copied().enumerate() {
            if dependency == index {
                continue;
            }
            if constraint
                .consumed_nodes()
                .into_iter()
                .flatten()
                .any(|consumed| ancestor_or_self(skeleton, producer.destination, consumed))
                && !edges[dependency][index]
            {
                edges[dependency][index] = true;
                indegree[index] += 1;
            }
        }
    }

    let mut ready = BinaryHeap::new();
    for (index, constraint) in constraints.iter().enumerate() {
        if indegree[index] == 0 {
            ready.push(Reverse((constraint.destination, index)));
        }
    }

    let mut order = Vec::with_capacity(count);
    while let Some(Reverse((_, index))) = ready.pop() {
        order.push(index);
        for dependent in 0..count {
            if !edges[index][dependent] {
                continue;
            }
            indegree[dependent] -= 1;
            if indegree[dependent] == 0 {
                ready.push(Reverse((constraints[dependent].destination, dependent)));
            }
        }
    }

    if order.len() != count {
        let mut cycle_destinations = constraints
            .iter()
            .enumerate()
            .filter_map(|(index, constraint)| {
                (indegree[index] != 0).then_some(constraint.destination)
            })
            .collect::<Vec<_>>();
        cycle_destinations.sort_unstable();
        bail!(
            "VRMC_node_constraint dependency cycle involving destinations {cycle_destinations:?}"
        );
    }
    Ok(order)
}

fn refresh_pose(
    skeleton: &Skeleton,
    locals: &[NodeTrs],
    globals: &mut Vec<Mat4>,
    global_rotations: &mut [Quat],
) {
    skeleton.globals_from_locals(locals, globals);
    for &node in &skeleton.order {
        let local = checked_normalize_quat(locals[node].rotation).unwrap_or(Quat::IDENTITY);
        let parent = skeleton.parents[node];
        global_rotations[node] = if parent == usize::MAX {
            local
        } else {
            checked_normalize_quat(global_rotations[parent] * local).unwrap_or(Quat::IDENTITY)
        };
    }
}

#[cfg(test)]
mod tests {
    use glam::{Quat, Vec3};
    use pocket_vrm::{Vrm1AimAxis, Vrm1NodeConstraint, Vrm1RollAxis};

    use super::*;

    fn assert_quat_equivalent(actual: Quat, expected: Quat) {
        let dot = actual.normalize().dot(expected.normalize()).abs();
        assert!(dot > 1.0 - 1.0e-5, "{actual:?} != {expected:?}");
    }

    fn skeleton(parents: &[usize]) -> Skeleton {
        let mut remaining = (0..parents.len()).collect::<Vec<_>>();
        let mut order = Vec::with_capacity(parents.len());
        while !remaining.is_empty() {
            let old_len = remaining.len();
            remaining.retain(|&node| {
                let parent = parents[node];
                if parent == usize::MAX || order.contains(&parent) {
                    order.push(node);
                    false
                } else {
                    true
                }
            });
            assert_ne!(remaining.len(), old_len, "test hierarchy contains a cycle");
        }
        Skeleton {
            parents: parents.to_vec(),
            rest: vec![NodeTrs::IDENTITY; parents.len()],
            order,
        }
    }

    fn rotation(destination: usize, source: usize) -> Vrm1NodeConstraint {
        Vrm1NodeConstraint {
            destination,
            kind: Vrm1NodeConstraintKind::Rotation {
                source,
                weight: 1.0,
            },
        }
    }

    fn roll(destination: usize, source: usize) -> Vrm1NodeConstraint {
        roll_on(destination, source, Vrm1RollAxis::X, 1.0)
    }

    fn roll_on(
        destination: usize,
        source: usize,
        axis: Vrm1RollAxis,
        weight: f32,
    ) -> Vrm1NodeConstraint {
        Vrm1NodeConstraint {
            destination,
            kind: Vrm1NodeConstraintKind::Roll {
                source,
                axis,
                weight,
            },
        }
    }

    fn aim(destination: usize, source: usize) -> Vrm1NodeConstraint {
        aim_on(destination, source, Vrm1AimAxis::PositiveX, 1.0)
    }

    fn aim_on(
        destination: usize,
        source: usize,
        axis: Vrm1AimAxis,
        weight: f32,
    ) -> Vrm1NodeConstraint {
        Vrm1NodeConstraint {
            destination,
            kind: Vrm1NodeConstraintKind::Aim {
                source,
                axis,
                weight,
            },
        }
    }

    fn runtime(
        parents: &[usize],
        constraints: Vec<Vrm1NodeConstraint>,
    ) -> Result<Vrm1NodeConstraintRuntime> {
        Vrm1NodeConstraintRuntime::new(&Vrm1NodeConstraintSet { constraints }, &skeleton(parents))
    }

    #[test]
    fn independent_constraints_use_destination_index_as_stable_tie_break() {
        let parents = [usize::MAX; 6];
        let runtime = runtime(
            &parents,
            vec![rotation(5, 0), rotation(2, 1), rotation(4, 3)],
        )
        .unwrap();
        assert_eq!(runtime.ordered_destinations(), [2, 4, 5]);
    }

    #[test]
    fn direct_chain_orders_producer_before_consumer() {
        let runtime = runtime(&[usize::MAX; 4], vec![rotation(3, 2), rotation(2, 0)]).unwrap();
        assert_eq!(runtime.ordered_destinations(), [2, 3]);
    }

    #[test]
    fn source_descendant_through_helpers_is_a_dependency() {
        let parents = [usize::MAX, usize::MAX, 1, 2, usize::MAX, usize::MAX];
        let runtime = runtime(&parents, vec![rotation(5, 3), rotation(1, 0)]).unwrap();
        assert_eq!(runtime.ordered_destinations(), [1, 5]);
    }

    #[test]
    fn constrained_ancestor_of_an_aim_source_is_a_dependency() {
        let parents = [usize::MAX, usize::MAX, 1, 2, usize::MAX, usize::MAX];
        let runtime = runtime(&parents, vec![aim(5, 3), rotation(1, 0)]).unwrap();
        assert_eq!(runtime.ordered_destinations(), [1, 5]);
    }

    #[test]
    fn aim_destination_parent_dependency_is_ordered() {
        let parents = [usize::MAX, usize::MAX, 1, 2, usize::MAX];
        let runtime = runtime(&parents, vec![aim(3, 4), rotation(1, 0)]).unwrap();
        assert_eq!(runtime.ordered_destinations(), [1, 3]);
    }

    #[test]
    fn destination_ancestry_is_not_a_local_only_dependency() {
        let parents = [
            usize::MAX,
            4,
            usize::MAX,
            usize::MAX,
            usize::MAX,
            usize::MAX,
        ];
        let runtime = runtime(&parents, vec![rotation(4, 0), roll(1, 2)]).unwrap();
        assert_eq!(runtime.ordered_destinations(), [1, 4]);
    }

    #[test]
    fn direct_and_indirect_cycles_are_rejected() {
        let direct = runtime(&[usize::MAX; 4], vec![rotation(1, 2), rotation(2, 1)])
            .unwrap_err()
            .to_string();
        assert!(direct.contains("cycle"), "{direct}");

        let indirect = runtime(
            &[usize::MAX; 5],
            vec![rotation(1, 2), rotation(2, 3), rotation(3, 1)],
        )
        .unwrap_err()
        .to_string();
        assert!(indirect.contains("cycle"), "{indirect}");
    }

    #[test]
    fn hierarchy_induced_aim_cycle_is_rejected() {
        let parents = [usize::MAX, usize::MAX, usize::MAX, 2, 1];
        let error = runtime(&parents, vec![aim(1, 3), aim(2, 4)])
            .unwrap_err()
            .to_string();
        assert!(error.contains("cycle"), "{error}");
    }

    #[test]
    fn self_reference_and_own_consumed_descendant_are_rejected() {
        let self_reference = runtime(&[usize::MAX; 3], vec![rotation(1, 1)])
            .unwrap_err()
            .to_string();
        assert!(
            self_reference.contains("equals its destination"),
            "{self_reference}"
        );

        let descendant = runtime(&[usize::MAX, usize::MAX, 1], vec![rotation(1, 2)])
            .unwrap_err()
            .to_string();
        assert!(descendant.contains("ancestor-or-self"), "{descendant}");
    }

    #[test]
    fn ordering_is_stable_across_repeated_preparation() {
        let parents = [usize::MAX; 7];
        let constraints = vec![rotation(6, 0), rotation(4, 1), rotation(5, 2)];
        let expected = runtime(&parents, constraints.clone())
            .unwrap()
            .ordered_destinations();
        for _ in 0..16 {
            assert_eq!(
                runtime(&parents, constraints.clone())
                    .unwrap()
                    .ordered_destinations(),
                expected
            );
        }
    }

    #[test]
    fn evaluation_propagates_a_direct_rotation_chain() {
        let skeleton = skeleton(&[usize::MAX; 3]);
        let semantics = Vrm1NodeConstraintSet {
            constraints: vec![rotation(2, 1), rotation(1, 0)],
        };
        let mut runtime = Vrm1NodeConstraintRuntime::new(&semantics, &skeleton).unwrap();
        let mut locals = skeleton.rest.clone();
        locals[0].rotation = Quat::from_rotation_z(0.7);
        let mut globals = Vec::new();
        runtime.evaluate(&skeleton, &mut locals, &mut globals);
        assert!(locals[1].rotation.angle_between(locals[0].rotation) < 1.0e-5);
        assert!(locals[2].rotation.angle_between(locals[0].rotation) < 1.0e-5);
    }

    #[test]
    fn evaluation_dispatches_rotation_roll_and_aim_into_final_globals() {
        let skeleton = skeleton(&[usize::MAX; 6]);
        let semantics = Vrm1NodeConstraintSet {
            constraints: vec![rotation(1, 0), roll(3, 2), aim(5, 4)],
        };
        let mut runtime = Vrm1NodeConstraintRuntime::new(&semantics, &skeleton).unwrap();
        let mut locals = skeleton.rest.clone();
        locals[0].rotation = Quat::from_rotation_z(0.4);
        locals[2].rotation = Quat::from_rotation_x(-0.6);
        locals[4].translation = Vec3::Y;
        let mut globals = Vec::new();

        runtime.evaluate(&skeleton, &mut locals, &mut globals);

        assert!(locals[1].rotation.angle_between(locals[0].rotation) < 1.0e-5);
        assert!(locals[3].rotation.angle_between(locals[2].rotation) < 1.0e-5);
        assert!((locals[5].rotation * Vec3::X).distance(Vec3::Y) < 1.0e-5);
        for destination in [1, 3, 5] {
            let expected = locals[destination].matrix();
            assert!(
                globals[destination]
                    .to_cols_array()
                    .iter()
                    .zip(expected.to_cols_array())
                    .all(|(actual, expected)| (actual - expected).abs() < 1.0e-5)
            );
        }
    }

    #[test]
    fn generated_isolated_rotation_caches_arbitrary_rests_and_partial_weight() {
        let mut skeleton = skeleton(&[usize::MAX; 2]);
        skeleton.rest[0].rotation = Quat::from_rotation_x(0.3) * Quat::from_rotation_y(-0.2);
        skeleton.rest[1].rotation = Quat::from_rotation_z(-0.4) * Quat::from_rotation_x(0.1);
        let source_current = skeleton.rest[0].rotation * Quat::from_rotation_y(0.75);
        let expected = rotation_constraint(
            skeleton.rest[0].rotation,
            source_current,
            skeleton.rest[1].rotation,
            0.35,
        );
        let semantics = Vrm1NodeConstraintSet {
            constraints: vec![Vrm1NodeConstraint {
                destination: 1,
                kind: Vrm1NodeConstraintKind::Rotation {
                    source: 0,
                    weight: 0.35,
                },
            }],
        };
        let mut runtime = Vrm1NodeConstraintRuntime::new(&semantics, &skeleton).unwrap();
        let mut locals = skeleton.rest.clone();
        locals[0].rotation = source_current;
        locals[1].rotation = Quat::from_rotation_y(-1.2);
        let mut globals = Vec::new();
        runtime.evaluate(&skeleton, &mut locals, &mut globals);
        assert_quat_equivalent(locals[1].rotation, expected);
    }

    #[test]
    fn generated_isolated_roll_covers_every_axis_and_partial_weight() {
        for axis in [Vrm1RollAxis::X, Vrm1RollAxis::Y, Vrm1RollAxis::Z] {
            let mut skeleton = skeleton(&[usize::MAX; 2]);
            skeleton.rest[0].rotation = Quat::from_rotation_y(0.25) * Quat::from_rotation_z(-0.15);
            skeleton.rest[1].rotation = Quat::from_rotation_x(-0.35) * Quat::from_rotation_y(0.2);
            let source_current = skeleton.rest[0].rotation
                * Quat::from_axis_angle(Vec3::new(1.0, 2.0, -0.5).normalize(), 0.8);
            let expected = roll_constraint(
                skeleton.rest[0].rotation,
                source_current,
                skeleton.rest[1].rotation,
                roll_axis_vector(axis),
                0.45,
            );
            let semantics = Vrm1NodeConstraintSet {
                constraints: vec![roll_on(1, 0, axis, 0.45)],
            };
            let mut runtime = Vrm1NodeConstraintRuntime::new(&semantics, &skeleton).unwrap();
            let mut locals = skeleton.rest.clone();
            locals[0].rotation = source_current;
            let mut globals = Vec::new();
            runtime.evaluate(&skeleton, &mut locals, &mut globals);
            assert_quat_equivalent(locals[1].rotation, expected);
        }
    }

    #[test]
    fn generated_isolated_aim_covers_every_signed_axis_parent_and_partial_weight() {
        for axis in [
            Vrm1AimAxis::PositiveX,
            Vrm1AimAxis::NegativeX,
            Vrm1AimAxis::PositiveY,
            Vrm1AimAxis::NegativeY,
            Vrm1AimAxis::PositiveZ,
            Vrm1AimAxis::NegativeZ,
        ] {
            let mut skeleton = skeleton(&[usize::MAX, 0, usize::MAX]);
            skeleton.rest[0].rotation = Quat::from_rotation_z(0.35);
            skeleton.rest[1].translation = Vec3::new(0.2, -0.1, 0.3);
            skeleton.rest[1].rotation = Quat::from_rotation_y(-0.25) * Quat::from_rotation_x(0.1);
            skeleton.rest[2].translation = Vec3::new(-0.7, 1.3, 0.8);
            let mut initial_globals = Vec::new();
            skeleton.globals_from_locals(&skeleton.rest, &mut initial_globals);
            let expected = aim_constraint(
                initial_globals[2].w_axis.truncate(),
                initial_globals[1].w_axis.truncate(),
                skeleton.rest[0].rotation,
                skeleton.rest[1].rotation,
                aim_axis_vector(axis),
                0.6,
            );
            let semantics = Vrm1NodeConstraintSet {
                constraints: vec![aim_on(1, 2, axis, 0.6)],
            };
            let mut runtime = Vrm1NodeConstraintRuntime::new(&semantics, &skeleton).unwrap();
            let mut locals = skeleton.rest.clone();
            locals[1].rotation = Quat::from_rotation_z(1.1);
            let mut globals = Vec::new();
            runtime.evaluate(&skeleton, &mut locals, &mut globals);
            assert_quat_equivalent(locals[1].rotation, expected);
        }
    }

    #[test]
    fn generated_degenerate_aim_target_is_finite_and_neutral() {
        let mut skeleton = skeleton(&[usize::MAX; 2]);
        skeleton.rest[0].rotation = Quat::from_rotation_x(0.2) * Quat::from_rotation_z(-0.3);
        let semantics = Vrm1NodeConstraintSet {
            constraints: vec![aim_on(0, 1, Vrm1AimAxis::NegativeZ, 0.75)],
        };
        let mut runtime = Vrm1NodeConstraintRuntime::new(&semantics, &skeleton).unwrap();
        let mut locals = skeleton.rest.clone();
        locals[0].rotation = Quat::from_rotation_y(1.0);
        let mut globals = Vec::new();
        runtime.evaluate(&skeleton, &mut locals, &mut globals);
        assert!(locals[0].rotation.is_finite());
        assert_quat_equivalent(locals[0].rotation, skeleton.rest[0].rotation);
    }

    #[test]
    fn later_aim_observes_constraint_modified_ancestor() {
        let mut skeleton = skeleton(&[usize::MAX, usize::MAX, 1, usize::MAX]);
        skeleton.rest[2].translation = Vec3::X;
        let semantics = Vrm1NodeConstraintSet {
            constraints: vec![aim(3, 2), rotation(1, 0)],
        };
        let mut runtime = Vrm1NodeConstraintRuntime::new(&semantics, &skeleton).unwrap();
        let mut locals = skeleton.rest.clone();
        locals[0].rotation = Quat::from_rotation_z(core::f32::consts::FRAC_PI_2);
        let mut globals = Vec::new();
        runtime.evaluate(&skeleton, &mut locals, &mut globals);
        assert!((locals[3].rotation * Vec3::X).distance(Vec3::Y) < 1.0e-5);
        assert!(globals[2].w_axis.truncate().distance(Vec3::Y) < 1.0e-5);
    }

    #[test]
    fn evaluation_refreshes_stale_globals_before_first_aim() {
        let mut skeleton = skeleton(&[usize::MAX, 0, usize::MAX]);
        skeleton.rest[1].translation = Vec3::X;
        let semantics = Vrm1NodeConstraintSet {
            constraints: vec![aim(2, 1)],
        };
        let mut runtime = Vrm1NodeConstraintRuntime::new(&semantics, &skeleton).unwrap();
        let mut locals = skeleton.rest.clone();
        let mut globals = vec![Mat4::IDENTITY; 3];
        locals[0].rotation = Quat::from_rotation_z(core::f32::consts::FRAC_PI_2);
        runtime.evaluate(&skeleton, &mut locals, &mut globals);
        assert!((locals[2].rotation * Vec3::X).distance(Vec3::Y) < 1.0e-5);
    }
}
