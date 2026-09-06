// PocketUI controls menu: a panel rendered through the MenuGuest →
// UiSurface → UiRenderer → Pocket3D Game::overlay() path, alpha-blended
// over the 3D character. Rust owns camera and AA policy; this guest renders
// authoritative snapshots, emits semantic button intents, and exposes
// generic text-input ownership for future editable widgets.
import { createSignal, type JSX } from "solid-js";
import { Focusable, Text, View } from "@pocketjs/framework/components";
import { virtualNow } from "@pocketjs/framework/clock";
import { getOps } from "@pocketjs/framework/solid";
import { focusNode, hitFocusable, pressNode, setActiveNode } from "@pocketjs/framework/input";
import { onFrame } from "@pocketjs/framework/lifecycle";
import { mount } from "@pocketjs/framework/solid";
import { formatCompactFov } from "./camera-value-format";
import {
  MSAA_OPTIONS,
  encodeMsaaRequest,
  encodeSmaaRequest,
  effectiveMsaaLabel,
  formatMsaaStatus,
  formatSmaaStatus,
  smaaStateLabel,
  type MsaaPreference,
} from "./graphics-settings";
import {
  INLINE_NUMBER_CELL_HEIGHT,
  INLINE_NUMBER_CELL_WIDTH,
  INLINE_NUMBER_CHAR_WIDTH,
  InlineNumberField,
  cancelInlineNumberFields,
  dispatchInlineNumberPointerDown,
} from "./inline-number-field";
import { PointerRepeat, type RepeatAction } from "./menu-repeat";
import { textInput } from "./text-input";

/// Authoritative facts pushed by the Rust host (MenuState in
/// crates/pocket-character/src/menu_guest.rs). Requested and effective AA are
/// intentionally separate: the guest only renders these facts and never
/// predicts whether a renderer request will apply.
interface ControlsState {
  effective_fov_deg: number;
  effective_distance_scale: number;
  requested_msaa: MsaaPreference;
  effective_msaa: number;
  requested_smaa: boolean;
  effective_smaa: boolean;
  msaa_pending: boolean;
  smaa_pending: boolean;
}

type ActionName =
  | "distance_decrement"
  | "distance_increment"
  | "fov_decrement"
  | "fov_increment"
  | "set_effective_fov"
  | "set_effective_distance"
  | "save_camera"
  | "reset_runtime_camera";

// Latest host facts, or null before the first svc line arrives.
const [controls, setControls] = createSignal<ControlsState | null>(null);
type SettingsPage = "camera" | "graphics";
// Ephemeral presentation state only; it is never serialized or sent to Rust.
const [activePage, setActivePage] = createSignal<SettingsPage>("camera");
const CAMERA_VALUE_X = 152;
const CAMERA_DISTANCE_VALUE_Y = 519;
const CAMERA_FOV_VALUE_Y = 539;

// Pointer ownership follows the framework's focusable hit target. The
// release compares against the latched down target, so one physical press can
// never fire more than once and dragging away cancels activation.
let pointerDown = false;
let pressedTarget: ReturnType<typeof hitFocusable> = null;
type PointerTarget = NonNullable<ReturnType<typeof hitFocusable>>;
const pointerRepeat = new PointerRepeat<PointerTarget>();

function sendAction(action: ActionName, value?: number): boolean {
  const ops = getOps();
  if (!ops.svcOpen || !ops.svcSend || !ops.svcOpen("controls")) return false;
  ops.svcSend(JSON.stringify({ t: "action", action, ...(value === undefined ? {} : { value }) }));
  return true;
}

function sendSmaaAction(enabled: boolean): boolean {
  const ops = getOps();
  if (!ops.svcOpen || !ops.svcSend || !ops.svcOpen("controls")) return false;
  ops.svcSend(JSON.stringify(encodeSmaaRequest(enabled)));
  return true;
}

function sendMsaaAction(preference: MsaaPreference): boolean {
  const ops = getOps();
  if (!ops.svcOpen || !ops.svcSend || !ops.svcOpen("controls")) return false;
  ops.svcSend(JSON.stringify(encodeMsaaRequest(preference)));
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
  cancelInlineNumberFields();
  pointerDown = false;
  pressedTarget = null;
  setActiveNode(null);
}

function handleMouse(x: number, y: number, down: boolean): void {
  const target = hitFocusable(x, y);
  focusNode(target);

  if (down) {
    if (!pointerDown) {
      dispatchInlineNumberPointerDown(target?.debugName ?? null, x, y);
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
          requested_msaa?: unknown;
          effective_msaa?: unknown;
          requested_smaa?: unknown;
          effective_smaa?: unknown;
          msaa_pending?: unknown;
          smaa_pending?: unknown;
          x?: unknown;
          y?: unknown;
          d?: unknown;
        };
        if (msg.t === "state") {
          if (
            typeof msg.effective_fov_deg !== "number" ||
            typeof msg.effective_distance_scale !== "number" ||
            typeof msg.requested_msaa !== "string" ||
            !MSAA_OPTIONS.some((option) => option.preference === msg.requested_msaa) ||
            typeof msg.effective_msaa !== "number" ||
            !Number.isSafeInteger(msg.effective_msaa) ||
            msg.effective_msaa < 1 ||
            typeof msg.requested_smaa !== "boolean" ||
            typeof msg.effective_smaa !== "boolean" ||
            typeof msg.msaa_pending !== "boolean" ||
            typeof msg.smaa_pending !== "boolean" ||
            !Number.isFinite(msg.effective_fov_deg) ||
            !Number.isFinite(msg.effective_distance_scale) ||
            !Number.isFinite(msg.effective_msaa)
          ) continue;
          setControls({
            effective_fov_deg: msg.effective_fov_deg,
            effective_distance_scale: msg.effective_distance_scale,
            requested_msaa: msg.requested_msaa as MsaaPreference,
            effective_msaa: msg.effective_msaa,
            requested_smaa: msg.requested_smaa,
            effective_smaa: msg.effective_smaa,
            msaa_pending: msg.msaa_pending,
            smaa_pending: msg.smaa_pending,
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
        } else if (msg.t === "input") {
          textInput.dispatch(msg);
        }
      } catch {
        // Skip malformed values.
      }
    }
  }
  textInput.syncCapture();

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
  value: () => number | null;
  format: (value: number) => string;
  commitAction: "set_effective_fov" | "set_effective_distance";
  captureY: number;
  decrement: () => void;
  increment: () => void;
  debugName: string;
}) {
  return (
    <View debugName={`${props.debugName}Row`} class="h-[18] flex-row items-center">
      <Text class="min-w-[48] flex-1 text-xs text-[#9fb3c8]">{props.label}</Text>
      <View class="shrink-0 flex-row items-center gap-[2]">
        <Button label="−" debugName={`${props.debugName}Decrement`} onPress={props.decrement} />
        <InlineNumberField
          id={`${props.debugName}Value`}
          debugName={`${props.debugName}Value`}
          value={props.value}
          format={props.format}
          editFormat={(value) => value.toString()}
          onCommit={(value) => sendAction(props.commitAction, value)}
          captureArea={(caret, draft) => {
            const textWidth = Math.min(INLINE_NUMBER_CELL_WIDTH, draft.length * INLINE_NUMBER_CHAR_WIDTH);
            const textLeft = Math.max(0, (INLINE_NUMBER_CELL_WIDTH - textWidth) / 2);
            return {
              x: CAMERA_VALUE_X + Math.min(INLINE_NUMBER_CELL_WIDTH - 1, textLeft + caret * INLINE_NUMBER_CHAR_WIDTH),
              y: props.captureY,
              width: 1,
              height: INLINE_NUMBER_CELL_HEIGHT,
            };
          }}
          caretFromPointer={(x, draft) => {
            const textWidth = Math.min(INLINE_NUMBER_CELL_WIDTH, draft.length * INLINE_NUMBER_CHAR_WIDTH);
            const textLeft = Math.max(0, (INLINE_NUMBER_CELL_WIDTH - textWidth) / 2);
            return Math.round((x - CAMERA_VALUE_X - textLeft) / INLINE_NUMBER_CHAR_WIDTH);
          }}
        />
        <Button label="+" debugName={`${props.debugName}Increment`} onPress={props.increment} />
      </View>
    </View>
  );
}

function msaaStatusText(state: ControlsState | null): string {
  if (!state) return "Waiting for host";
  return formatMsaaStatus(state.requested_msaa, state.effective_msaa, state.msaa_pending);
}

function smaaStatusText(state: ControlsState | null): string {
  if (!state) return "Waiting for host";
  return formatSmaaStatus(state.requested_smaa, state.effective_smaa, state.smaa_pending);
}

function SelectorOption(props: {
  preference: MsaaPreference;
  label: string;
  selected: boolean;
  onPress: () => void;
}) {
  return (
    <Focusable
      debugName={`Msaa${props.preference}`}
      class={
        props.selected
          ? "h-[18] w-[28] flex-col items-center justify-center rounded-sm bg-[#2b5167]"
          : "h-[18] w-[28] flex-col items-center justify-center rounded-sm bg-[#172b3b] focus:bg-[#2b5167] active:bg-[#3a6f88]"
      }
      onPress={props.onPress}
    >
      <Text class="text-xs text-[#e8f1f8]">{props.label}</Text>
    </Focusable>
  );
}

function PageTabs() {
  const tabClass = (page: SettingsPage): string =>
    activePage() === page
      ? "h-[24] flex-1 flex-col items-center justify-center rounded-sm bg-[#2b5167]"
      : "h-[24] flex-1 flex-col items-center justify-center rounded-sm bg-[#172b3b] focus:bg-[#2b5167] active:bg-[#3a6f88]";

  return (
    <View debugName="SettingsTabs" class="h-[24] w-full flex-row gap-[4]">
      <Focusable debugName="CameraTab" class={tabClass("camera")} onPress={() => setActivePage("camera")}>
        <Text class="text-sm text-[#e8f1f8]">CAMERA</Text>
      </Focusable>
      <Focusable debugName="GraphicsTab" class={tabClass("graphics")} onPress={() => setActivePage("graphics")}>
        <Text class="text-sm text-[#e8f1f8]">GRAPHICS</Text>
      </Focusable>
    </View>
  );
}

function SettingsFrame(props: { panelClass: string; children: JSX.Element }) {
  return (
    <View debugName="SettingsFrame" class={props.panelClass}>
      <PageTabs />
      <View class="relative top-[2] mt-[4] h-[1] w-full bg-[#33c6ff4d]" />
      {props.children}
    </View>
  );
}

function GraphicsPanel() {
  const state = () => controls();
  return (
    <SettingsFrame panelClass="w-[216] flex-col rounded-md bg-[#0b1420e8] p-[16]">
      <View class="mt-[6] flex-col">
        <View class="h-[18] flex-row items-center">
          <Text class="text-xs font-bold text-[#7fd0ff]">MSAA</Text>
        </View>
        <View class="mt-[4] h-[18] flex-row items-center">
          <View class="w-[60] shrink-0">
            <Text class="text-xs text-[#9fb3c8]">Requested</Text>
          </View>
          <View class="w-[6] shrink-0" />
          <View class="flex-row gap-[2]">
            {MSAA_OPTIONS.map((option) => (
              <SelectorOption
                preference={option.preference}
                label={option.label}
                selected={state()?.requested_msaa === option.preference}
                onPress={() => sendMsaaAction(option.preference)}
              />
            ))}
          </View>
        </View>
        <View class="mt-[2] h-[18] flex-row items-center">
          <View class="w-[60] shrink-0">
            <Text class="text-xs text-[#9fb3c8]">Effective</Text>
          </View>
          <View class="w-[6] shrink-0" />
          <Text class="text-xs text-[#e8f1f8]">{effectiveMsaaLabel(state()?.effective_msaa ?? null)}</Text>
        </View>
        {msaaStatusText(state()) ? (
          <View class="flex-row items-center">
            <View class="w-[60] shrink-0" />
            <View class="w-[6] shrink-0" />
            <Text class="h-[11] text-[9] text-[#8fa8bc]">{msaaStatusText(state())}</Text>
          </View>
        ) : null}
        <View class="mt-[8] h-[18] flex-row items-center">
          <Text class="text-xs font-bold text-[#7fd0ff]">SMAA</Text>
        </View>
        <View class="mt-[4] h-[18] flex-row items-center">
          <View class="w-[60] shrink-0">
            <Text class="text-xs text-[#9fb3c8]">Requested</Text>
          </View>
          <View class="w-[6] shrink-0" />
          <Focusable
            debugName="SmaaToggle"
            class={
              state()?.requested_smaa
                ? "h-[18] w-[38] flex-col items-center justify-center rounded-sm bg-[#2b5167]"
                : "h-[18] w-[38] flex-col items-center justify-center rounded-sm bg-[#172b3b] focus:bg-[#2b5167] active:bg-[#3a6f88]"
            }
            onPress={() => {
              const current = state();
              if (current) sendSmaaAction(!current.requested_smaa);
            }}
          >
            <Text class="text-xs text-[#e8f1f8]">{smaaStateLabel(state()?.requested_smaa ?? null)}</Text>
          </Focusable>
        </View>
        <View class="mt-[2] h-[18] flex-row items-center">
          <View class="w-[60] shrink-0">
            <Text class="text-xs text-[#9fb3c8]">Effective</Text>
          </View>
          <View class="w-[6] shrink-0" />
          <Text class="text-xs text-[#e8f1f8]">{smaaStateLabel(state()?.effective_smaa ?? null)}</Text>
        </View>
        {smaaStatusText(state()) ? (
          <View class="flex-row items-center">
            <View class="w-[60] shrink-0" />
            <View class="w-[6] shrink-0" />
            <Text class="h-[11] text-[9] text-[#8fa8bc]">{smaaStatusText(state())}</Text>
          </View>
        ) : null}
      </View>
    </SettingsFrame>
  );
}

function CameraPanel() {
  // The shared divider is directly below the tabs. The heading's compacted
  // slot offsets the taller tabs, preserving camera field, button, and native
  // capture geometry.
  return (
    <SettingsFrame panelClass="w-[216] flex-col rounded-md bg-[#0b1420b4] p-[16]">
      <View class="mt-[6] h-[16] flex-row items-center">
        <Text class="text-xs font-bold text-[#7fd0ff]">FRAMING</Text>
      </View>
      <View class="mt-[0] flex-col gap-[2]">
        <Row
          label="Distance"
          value={() => controls()?.effective_distance_scale ?? null}
          format={(value) => value.toFixed(2)}
          commitAction="set_effective_distance"
          captureY={CAMERA_DISTANCE_VALUE_Y}
          debugName="Distance"
          decrement={() => sendAction("distance_decrement")}
          increment={() => sendAction("distance_increment")}
        />
        <Row
          label="FOV"
          value={() => controls()?.effective_fov_deg ?? null}
          format={formatCompactFov}
          commitAction="set_effective_fov"
          captureY={CAMERA_FOV_VALUE_Y}
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
    </SettingsFrame>
  );
}

export default function ControlsMenu() {
  onFrame(pollControls);
  // Display formatting only. Rust computes the next value and applies all
  // safety/persistence semantics after receiving the semantic action.
  return (
    <View debugName="ControlsMenu" class="absolute left-[14] top-[456]">
      {activePage() === "camera" ? <CameraPanel /> : <GraphicsPanel />}
    </View>
  );
}

mount(() => <ControlsMenu />);
