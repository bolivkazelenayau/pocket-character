// PocketUI controls menu: a panel rendered through the MenuGuest →
// UiSurface → UiRenderer → Pocket3D Game::overlay() path, alpha-blended
// over the 3D character. Rust owns camera policy; this guest renders the
// authoritative live camera values and emits semantic button intents only.
import { createSignal } from "solid-js";
import { Focusable, Text, View } from "@pocketjs/framework/components";
import { virtualNow } from "@pocketjs/framework/clock";
import { getOps } from "@pocketjs/framework/solid";
import { focusNode, hitFocusable, pressNode, setActiveNode } from "@pocketjs/framework/input";
import { onFrame } from "@pocketjs/framework/lifecycle";
import { mount } from "@pocketjs/framework/solid";
import { PointerRepeat, type RepeatAction } from "./menu-repeat";

/// Authoritative camera facts pushed by the Rust host (MenuState in
/// crates/pocket-character/src/menu_guest.rs). This compact panel displays the
/// live/effective optics; persistence is an explicit Save action.
interface ControlsState {
  effective_fov_deg: number;
  effective_distance_scale: number;
}

type ActionName =
  | "distance_decrement"
  | "distance_increment"
  | "fov_decrement"
  | "fov_increment"
  | "save_camera"
  | "reset_runtime_camera";

// Latest host facts, or null before the first svc line arrives.
const [controls, setControls] = createSignal<ControlsState | null>(null);

// Pointer ownership follows the framework's focusable hit target. The
// release compares against the latched down target, so one physical press can
// never fire more than once and dragging away cancels activation.
let pointerDown = false;
let pressedTarget: ReturnType<typeof hitFocusable> = null;
type PointerTarget = NonNullable<ReturnType<typeof hitFocusable>>;
const pointerRepeat = new PointerRepeat<PointerTarget>();

function sendAction(action: ActionName): boolean {
  const ops = getOps();
  if (!ops.svcOpen || !ops.svcSend || !ops.svcOpen("controls")) return false;
  ops.svcSend(JSON.stringify({ t: "action", action }));
  return true;
}

function repeatActionFor(target: ReturnType<typeof hitFocusable>): RepeatAction | null {
  switch (target?.debugName) {
    case "DistanceDecrement":
      return "distance_decrement";
    case "DistanceIncrement":
      return "distance_increment";
    case "FovDecrement":
      return "fov_decrement";
    case "FovIncrement":
      return "fov_increment";
    default:
      return null;
  }
}

function cancelPointerInteraction(): void {
  pointerRepeat.cancel();
  pointerDown = false;
  pressedTarget = null;
  setActiveNode(null);
}

function handleMouse(x: number, y: number, down: boolean): void {
  const target = hitFocusable(x, y);
  focusNode(target);

  if (down) {
    if (!pointerDown) {
      pressedTarget = target;
      const immediate = pointerRepeat.begin(target, repeatActionFor(target), virtualNow());
      if (immediate && !sendAction(immediate)) cancelPointerInteraction();
    } else {
      pointerRepeat.move(target);
    }
    pointerDown = true;
    setActiveNode(target === pressedTarget ? pressedTarget : null);
    return;
  }

  const shouldPress = pointerDown && target !== null && target === pressedTarget;
  const activateOnRelease = pointerRepeat.end(target);
  pointerDown = false;
  pressedTarget = null;
  setActiveNode(null);
  if (shouldPress && activateOnRelease) pressNode(target);
}

/// Drain this frame's host lines into the signal and pointer state. The
/// note-app svc dialect is newline-batched JSON; malformed or unrelated lines
/// are skipped so a host bug cannot wedge the menu.
function pollControls(): void {
  const ops = getOps();
  if (!ops.svcOpen || !ops.svcPoll || !ops.svcOpen("controls")) {
    cancelPointerInteraction();
    return;
  }
  const batch = ops.svcPoll();
  if (batch) {
    for (const line of batch.split("\n")) {
      if (line === "") continue;
      try {
        const msg = JSON.parse(line) as {
          t?: unknown;
          effective_fov_deg?: unknown;
          effective_distance_scale?: unknown;
          x?: unknown;
          y?: unknown;
          d?: unknown;
        };
        if (msg.t === "state") {
          if (
            typeof msg.effective_fov_deg !== "number" ||
            typeof msg.effective_distance_scale !== "number" ||
            !Number.isFinite(msg.effective_fov_deg) ||
            !Number.isFinite(msg.effective_distance_scale)
          ) continue;
          setControls({
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
        // Skip malformed values.
      }
    }
  }

  for (const action of pointerRepeat.tick(virtualNow())) {
    if (!sendAction(action)) {
      cancelPointerInteraction();
      break;
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
    <View debugName={`${props.debugName}Row`} class="h-[18] flex-row items-center">
      <Text class="min-w-[48] flex-1 text-xs text-[#9fb3c8]">{props.label}</Text>
      <View class="shrink-0 flex-row items-center gap-[2]">
        <Button label="−" debugName={`${props.debugName}Decrement`} onPress={props.decrement} />
        <Text class="w-[42] text-center text-xs font-mono text-[#e8f1f8]">{props.value}</Text>
        <Button label="+" debugName={`${props.debugName}Increment`} onPress={props.increment} />
      </View>
    </View>
  );
}

export default function ControlsMenu() {
  onFrame(pollControls);
  // Display formatting only. Rust computes the next value and applies all
  // safety/persistence semantics after receiving the semantic action.
  const distance = () => controls()?.effective_distance_scale.toFixed(2) ?? "—";
  const fov = () => controls()?.effective_fov_deg.toFixed(1) ?? "—";
  return (
    <View
      debugName="ControlsMenu"
      class="absolute left-[14] top-[474] w-[216] flex-col rounded-md bg-[#0b1420b4] p-[16]"
    >
      <View class="h-[18] flex-row items-center">
        <Text class="text-xs font-bold tracking-wide text-[#7fd0ff]">CAMERA</Text>
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
      <View class="mt-[6] flex-row items-center justify-end gap-[4]">
        <Focusable
          debugName="SaveCamera"
          class="h-[18] w-[44] flex-col items-center justify-center rounded-sm bg-[#172b3b] focus:bg-[#2b5167] active:bg-[#3a6f88]"
          onPress={() => sendAction("save_camera")}
        >
          <Text class="text-xs text-[#e8f1f8]">Save</Text>
        </Focusable>
        <Focusable
          debugName="ResetRuntimeCamera"
          class="h-[18] w-[96] flex-col items-center justify-center rounded-sm bg-[#172b3b] focus:bg-[#2b5167] active:bg-[#3a6f88]"
          onPress={() => sendAction("reset_runtime_camera")}
        >
          <Text class="text-xs text-[#e8f1f8]">Reset Camera</Text>
        </Focusable>
      </View>
    </View>
  );
}

mount(() => <ControlsMenu />);
