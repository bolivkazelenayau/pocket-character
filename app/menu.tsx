// PocketUI controls menu: a panel rendered through the MenuGuest →
// UiSurface → UiRenderer → Pocket3D Game::overlay() path, alpha-blended
// over the 3D character. Rust owns camera policy; this guest renders the
// authoritative base values and emits semantic button intents only.
import { createSignal } from "solid-js";
import { Focusable, Text, View } from "@pocketjs/framework/components";
import { getOps } from "@pocketjs/framework/solid";
import { focusNode, hitFocusable, pressNode, setActiveNode } from "@pocketjs/framework/input";
import { onFrame } from "@pocketjs/framework/lifecycle";
import { mount } from "@pocketjs/framework/solid";

/// Authoritative camera facts pushed by the Rust host (MenuState in
/// crates/pocket-character/src/menu_guest.rs). The editable values are the
/// persisted/base values; effective values remain part of the wire state for
/// future read-only diagnostics but are not presented as editable controls.
interface ControlsState {
  base_fov_deg: number;
  base_distance_scale: number;
  effective_fov_deg: number;
  effective_distance_scale: number;
}

type ActionName =
  | "distance_decrement"
  | "distance_increment"
  | "fov_decrement"
  | "fov_increment"
  | "reset_runtime_camera";

// Latest host facts, or null before the first svc line arrives.
const [controls, setControls] = createSignal<ControlsState | null>(null);

// Pointer ownership follows the framework's focusable hit target. The
// release compares against the latched down target, so one physical press can
// never fire more than once and dragging away cancels activation.
let pointerDown = false;
let pressedTarget: ReturnType<typeof hitFocusable> = null;

function sendAction(action: ActionName): void {
  const ops = getOps();
  if (!ops.svcOpen || !ops.svcSend || !ops.svcOpen("controls")) return;
  ops.svcSend(JSON.stringify({ t: "action", action }));
}

function handleMouse(x: number, y: number, down: boolean): void {
  const target = hitFocusable(x, y);
  focusNode(target);

  if (down) {
    if (!pointerDown) pressedTarget = target;
    pointerDown = true;
    setActiveNode(target === pressedTarget ? pressedTarget : null);
    return;
  }

  const shouldPress = pointerDown && target !== null && target === pressedTarget;
  pointerDown = false;
  pressedTarget = null;
  setActiveNode(null);
  if (shouldPress) pressNode(target);
}

/// Drain this frame's host lines into the signal and pointer state. The
/// note-app svc dialect is newline-batched JSON; malformed or unrelated lines
/// are skipped so a host bug cannot wedge the menu.
function pollControls(): void {
  const ops = getOps();
  if (!ops.svcOpen || !ops.svcPoll || !ops.svcOpen("controls")) return;
  const batch = ops.svcPoll();
  if (!batch) return;
  for (const line of batch.split("\n")) {
    if (line === "") continue;
    try {
      const msg = JSON.parse(line) as {
        t?: unknown;
        base_fov_deg?: unknown;
        base_distance_scale?: unknown;
        effective_fov_deg?: unknown;
        effective_distance_scale?: unknown;
        x?: unknown;
        y?: unknown;
        d?: unknown;
      };
      if (msg.t === "state") {
        if (
          typeof msg.base_fov_deg !== "number" ||
          typeof msg.base_distance_scale !== "number" ||
          typeof msg.effective_fov_deg !== "number" ||
          typeof msg.effective_distance_scale !== "number" ||
          !Number.isFinite(msg.base_fov_deg) ||
          !Number.isFinite(msg.base_distance_scale) ||
          !Number.isFinite(msg.effective_fov_deg) ||
          !Number.isFinite(msg.effective_distance_scale)
        ) continue;
        setControls({
          base_fov_deg: msg.base_fov_deg,
          base_distance_scale: msg.base_distance_scale,
          effective_fov_deg: msg.effective_fov_deg,
          effective_distance_scale: msg.effective_distance_scale,
        });
      } else if (
        msg.t === "mouse" &&
        typeof msg.x === "number" &&
        typeof msg.y === "number" &&
        typeof msg.d === "boolean" &&
        Number.isFinite(msg.x) &&
        Number.isFinite(msg.y)
      ) {
        handleMouse(msg.x, msg.y, msg.d);
      }
    } catch {
      // Skip malformed lines.
    }
  }
}

function Button(props: { label: string; onPress: () => void; debugName: string }) {
  return (
    <Focusable
      debugName={props.debugName}
      class="h-[18] w-[18] flex-col items-center justify-center rounded-sm bg-[#172b3b] focus:bg-[#2b5167] active:bg-[#3a6f88]"
      onPress={props.onPress}
    >
      <Text class="text-xs text-[#e8f1f8]">{props.label}</Text>
    </Focusable>
  );
}

function Row(props: {
  label: string;
  value: string;
  decrement: () => void;
  increment: () => void;
  debugName: string;
}) {
  return (
    <View debugName={`${props.debugName}Row`} class="h-[18] flex-row items-center justify-between">
      <Text class="text-xs text-[#9fb3c8]">{props.label}</Text>
      <View class="flex-row items-center gap-[2]">
        <Button label="−" debugName={`${props.debugName}Decrement`} onPress={props.decrement} />
        <Text class="w-[42] text-center text-xs text-[#e8f1f8]">{props.value}</Text>
        <Button label="+" debugName={`${props.debugName}Increment`} onPress={props.increment} />
      </View>
    </View>
  );
}

export default function ControlsMenu() {
  onFrame(pollControls);
  // Display formatting only. Rust computes the next value and applies all
  // safety/persistence semantics after receiving the semantic action.
  const distance = () => controls()?.base_distance_scale.toFixed(2) ?? "—";
  const fov = () => controls()?.base_fov_deg.toFixed(1) ?? "—";
  return (
    <View
      debugName="ControlsMenu"
      class="absolute left-[14] top-[510] w-[172] flex-col rounded-md bg-[#0b1420b4] p-[10]"
    >
      <View class="h-[18] flex-row items-center justify-between">
        <Text class="text-xs font-bold tracking-wide text-[#7fd0ff]">CAMERA</Text>
        <Focusable
          debugName="ResetRuntimeCamera"
          class="h-[18] w-[44] flex-col items-center justify-center rounded-sm bg-[#172b3b] focus:bg-[#2b5167] active:bg-[#3a6f88]"
          onPress={() => sendAction("reset_runtime_camera")}
        >
          <Text class="text-xs text-[#e8f1f8]">Reset</Text>
        </Focusable>
      </View>
      <View class="mt-[4] h-[1] w-full bg-[#33c6ff4d]" />
      <View class="mt-[6] flex-col gap-[2]">
        <Row
          label="Distance"
          value={distance()}
          debugName="Distance"
          decrement={() => sendAction("distance_decrement")}
          increment={() => sendAction("distance_increment")}
        />
        <Row
          label="FOV"
          value={fov()}
          debugName="Fov"
          decrement={() => sendAction("fov_decrement")}
          increment={() => sendAction("fov_increment")}
        />
      </View>
    </View>
  );
}

mount(() => <ControlsMenu />);
