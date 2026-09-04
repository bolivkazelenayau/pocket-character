// PocketUI overlay proof: a static controls-menu panel rendered through the
// MenuGuest → UiSurface → UiRenderer → Pocket3D Game::overlay() path, alpha-
// blended over the 3D character. This pass is render-only — no Focusable,
// no press/touch/keyboard input, no svc actions — so the app mounts one
// static tree and registers nothing per-frame.
//
// TODO(controls): the "0.60" / "40.0" strings are TEMPORARY PROOF VALUES for
// static presentation only. The next pass replaces them with the host's
// ControlsSnapshot (Rust→TSX state bridge) and adds interactive controls.
// Camera validation/settings semantics stay canonical in Rust; never
// duplicate them here.
import { Text, View } from "@pocketjs/framework/components";
import { mount } from "@pocketjs/framework/solid";

function Row(props: { label: string; value: string }) {
  return (
    <View debugName="CameraRow" class="h-[18] flex-row items-center justify-between">
      <Text class="text-xs text-[#9fb3c8]">{props.label}</Text>
      <Text class="text-xs text-[#e8f1f8]">{props.value}</Text>
    </View>
  );
}

export default function ControlsMenu() {
  return (
    <View
      debugName="ControlsMenu"
      class="absolute left-[14] top-[540] w-[172] flex-col rounded-md bg-[#0b1420b4] p-[10]"
    >
      <Text class="text-xs font-bold tracking-wide text-[#7fd0ff]">CAMERA</Text>
      <View class="mt-[4] h-[1] w-full bg-[#33c6ff4d]" />
      <View class="mt-[6] flex-col gap-[2]">
        <Row label="Distance" value="0.60" />
        <Row label="FOV" value="40.0" />
      </View>
    </View>
  );
}

mount(() => <ControlsMenu />);
