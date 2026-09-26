# Manual expression yielding of Auto Blink

VRM1 expression composition previously added generated blink even when a manual
expression already moved the eyelids and its author specified `overrideBlink:
none`. The resolver now derives `conflicts_with_blink` during avatar preparation,
alongside the resolved morph binds. Pocket3D does not own this product policy.

For each authored semantic blink (`blink`, `blinkLeft`, `blinkRight`), the resolver
adds its bind-weighted position deltas per `(mesh slot, primitive, vertex)`.
It compares every other expression's accumulated position deltas at those same
vertices, including expressions with different morph target indices. A vertex
counts as overlapping when the expression displaces it by at least 10% of the
blink's displacement. A conflict requires at least 20% of that semantic blink's
squared displacement energy to overlap. Checking each side separately also
handles unilateral deformation. Summing vectors before comparison respects bind
weights and cancellation; mesh identity alone never establishes a conflict.
Normal-only, material-only and texture-only expressions do not move geometry.

This is a geometry heuristic, not an anatomical eyelid classifier. It depends
on usable authored blink position morphs, and may miss very localized eye motion
below the coverage threshold or classify substantial non-eyelid motion included
in an author's blink region. It does not infer bone-driven eyelid deformation.
The relative motion and energy thresholds filter small exported deltas without
depending on model units or expression names. No morph scanning occurs per frame.
VRM0 retains its legacy expression path.

Only manual slider inputs contribute to suppression:

```text
t = clamp((manual_weight - 0.50) / (0.95 - 0.50), 0, 1)
suppression_i = t * t * (3 - 2 * t)
suppression = max(suppression_i for conflicting expressions)
generated_blink *= 1 - suppression
```

Guest expression inputs are unchanged. Explicit blink and wink retain their
existing ownership of eye state. Authored `overrideBlink` none/block/blend still
resolves afterward, including its effects on explicit expressions and binary
expression outputs. Binary sliders retain their authored binary behavior.

When Auto Blink is on and suppression reaches 0.95, the avatar behavior layer
parks the simulation's generated blink cycle. Release schedules a fresh normal
1–6 second interval; a hidden mid-blink cannot reappear. Saccades continue. Auto
Blink Off retains its existing output gate. New avatars own fresh metadata,
manual inputs and scheduler state; failed replacement retains the active avatar.

Focused unit tests cover the curve, max aggregation, input ownership, explicit
blink/wink, overrides, disjoint geometry and negligible motion. Headless tests
also import a generated custom expression with a different overlapping target,
and use `C:\Users\Breeze\Downloads\AvatarSample_VRM1.0.vrm` when present to check
happy's 1.0 → 0.8 → 0.6 → 0.4 → 0.0 ramp, a non-eye mouth expression at 1.0,
both MToon routes, parked-cycle release and transactional avatar replacement.
