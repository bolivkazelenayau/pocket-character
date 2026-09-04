// PocketUI controls menu: a panel rendered through the MenuGuest →
// UiSurface → UiRenderer → Pocket3D Game::overlay() path, alpha-blended
// over the 3D character. This pass is render-only and one-way — Rust
// pushes effective camera facts over the svc channel, the app polls them
// per frame into a Solid signal and applies display formatting (decimals)
// only. No Focusable,
// no press/touch/keyboard input, no svc lines back to Rust.
import { createSignal } from "solid-js";
import { Text, View } from "@pocketjs/framework/components";
import { getOps } from "@pocketjs/framework/solid";
import { onFrame } from "@pocketjs/framework/lifecycle";
import { mount } from "@pocketjs/framework/solid";

/// Authoritative camera facts pushed by the Rust host (MenuState in
/// crates/pocket-character/src/menu_guest.rs). TSX performs display
/// formatting only (toFixed decimals below), with no validation or
/// clamping — camera validation/settings semantics stay canonical in Rust
/// and must never be duplicated here.
interface ControlsState {
  effective_fov_deg: number;
  effective_distance_scale: number;
}

// Latest host facts, or null before the first svc line arrives (the host
// queues one ahead of every guest turn, so placeholders only show
// pre-first-frame). One-way bridge: the menu never sends anything back.
const [controls, setControls] = createSignal<ControlsState | null>(null);

/// Drain this frame's host lines into the signal (the note-app svc
/// dialect: newline-batched JSON, `t`-tagged; unknown or malformed lines
/// are skipped so a host bug can never wedge the menu).
function pollControls() {
  const ops = getOps();
  if (!ops.svcOpen || !ops.svcPoll || !ops.svcOpen("controls")) return;
  const batch = ops.svcPoll();
  if (!batch) return;
  for (const line of batch.split("\n")) {
    if (line === "") continue;
    try {
      const msg = JSON.parse(line) as {
        t?: unknown;
        effective_fov_deg?: unknown;
        effective_distance_scale?: unknown;
      };
      if (msg.t !== "state") continue;
      if (typeof msg.effective_fov_deg !== "number" || typeof msg.effective_distance_scale !== "number") continue;
      if (!Number.isFinite(msg.effective_fov_deg) || !Number.isFinite(msg.effective_distance_scale)) continue;
      setControls({ effective_fov_deg: msg.effective_fov_deg, effective_distance_scale: msg.effective_distance_scale });
    } catch {
      // Skip malformed lines.
    }
  }
}

function Row(props: { label: string; value: string }) {
  return (
    <View debugName="CameraRow" class="h-[18] flex-row items-center justify-between">
      <Text class="text-xs text-[#9fb3c8]">{props.label}</Text>
      <Text class="text-xs text-[#e8f1f8]">{props.value}</Text>
    </View>
  );
}

export default function ControlsMenu() {
  onFrame(pollControls);
  // Display formatting only (decimals match the earlier static proof).
  const distance = () => controls()?.effective_distance_scale.toFixed(2) ?? "—";
  const fov = () => controls()?.effective_fov_deg.toFixed(1) ?? "—";
  return (
    <View
      debugName="ControlsMenu"
      class="absolute left-[14] top-[510] w-[172] flex-col rounded-md bg-[#0b1420b4] p-[10]"
    >
      <Text class="text-xs font-bold tracking-wide text-[#7fd0ff]">CAMERA</Text>
      <View class="mt-[4] h-[1] w-full bg-[#33c6ff4d]" />
      <View class="mt-[6] flex-col gap-[2]">
        <Row label="Distance" value={distance()} />
        <Row label="FOV" value={fov()} />
      </View>
    </View>
  );
}

mount(() => <ControlsMenu />);
