import { describe, expect, test } from "bun:test";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import type { HostOps } from "@pocketjs/framework";

const BTN_UP = 0x0010;
const BTN_DOWN = 0x0040;
const BTN_CIRCLE = 0x2000;

interface TestCameraState {
  headroom: number;
  yaw: number;
  pitch: number;
  roll: number;
  yawSnap: number;
  pitchSnap: number;
  rollSnap: number;
}

function testHost(
  incoming: string[],
  outgoing: unknown[],
  textUpdates: string[],
  onSend: (message: unknown) => void,
  focusState: { id: number },
): HostOps & { __viewport: { w: number; h: number } } {
  let nextNode = 2;
  return {
    __viewport: { w: 450, h: 600 },
    createNode: () => nextNode++,
    destroyNode: () => {},
    insertBefore: () => {},
    removeChild: () => {},
    setStyle: () => {},
    setProp: () => {},
    setText: (_id, value) => textUpdates.push(value),
    replaceText: (_id, value) => textUpdates.push(value),
    uploadTexture: () => 0,
    setImage: () => {},
    setSprite: () => {},
    animate: () => 0,
    cancelAnim: () => {},
    setFocus: (id) => {
      focusState.id = id;
    },
    setActive: () => {},
    hitTest: () => focusState.id,
    measureText: (value) => value.length * 8,
    svcOpen: (app) => app === "controls",
    svcPoll: () => incoming.shift(),
    svcSend: (line) => {
      const message = JSON.parse(line);
      outgoing.push(message);
      onSend(message);
    },
  };
}

function stateLine(
  state: TestCameraState = {
    headroom: 0.05,
    yaw: 22.5,
    pitch: -7.5,
    roll: 15,
    yawSnap: 15,
    pitchSnap: 15,
    rollSnap: 15,
  },
  observedSize: [number, number] = [720, 700],
): string {
  return JSON.stringify({
    t: "state",
    effective_fov_deg: 40,
    effective_distance_scale: 0.6,
    headroom: state.headroom,
    yaw_deg: state.yaw,
    pitch_deg: state.pitch,
    roll_deg: state.roll,
    yaw_snap_deg: state.yawSnap,
    pitch_snap_deg: state.pitchSnap,
    roll_snap_deg: state.rollSnap,
    requested_msaa: "4x",
    effective_msaa: 4,
    requested_smaa: false,
    effective_smaa: false,
    msaa_pending: false,
    smaa_pending: false,
    window: {
      configured_width: 450,
      configured_height: 600,
      configured_resizable: false,
      configured_always_on_top: true,
      configured_max_fps: 60,
      current_width_logical: observedSize[0],
      current_height_logical: observedSize[1],
      applied_resizable: false,
      applied_always_on_top: true,
      effective_max_fps: 60,
      cli_max_fps_override: null,
    },
  });
}

describe("PocketUI camera menu snap regression", () => {
  test("displaying ORIENTATION never changes persisted snap increments", async () => {
    const directory = await mkdtemp(join(tmpdir(), "pocket-character-menu-"));
    const settingsPath = join(directory, "settings.json");
    const settings = {
      camera: {
        headroom: 0.05,
        yaw_snap_deg: 15,
        pitch_snap_deg: 15,
        roll_snap_deg: 15,
      },
    };
    await writeFile(settingsPath, `${JSON.stringify(settings, null, 2)}\n`);
    const persistedBefore = await readFile(settingsPath, "utf8");
    const runtimeSnaps = { ...settings.camera };
    const incoming = [stateLine(undefined, [720, 700])];
    const outgoing: unknown[] = [];
    const snapMutations: unknown[] = [];
    const textUpdates: string[] = [];
    const focusState = { id: 0 };
    const cameraState: TestCameraState = {
      headroom: 0.05,
      yaw: 22.5,
      pitch: -7.5,
      roll: 15,
      yawSnap: 15,
      pitchSnap: 15,
      rollSnap: 15,
    };
    const host = testHost(incoming, outgoing, textUpdates, (message) => {
      if (
        typeof message === "object" &&
        message !== null &&
        "action" in message &&
        typeof message.action === "string" &&
        message.action.includes("snap")
      ) {
        snapMutations.push(message);
      }
      if (typeof message !== "object" || message === null || !("action" in message)) return;
      const actionMessage = message as { action: unknown; value?: unknown };
      const action = actionMessage.action;
      if (action === "set_yaw" && typeof actionMessage.value === "number") cameraState.yaw = actionMessage.value;
      if (action === "set_pitch" && typeof actionMessage.value === "number") cameraState.pitch = actionMessage.value;
      if (action === "set_roll" && typeof actionMessage.value === "number") cameraState.roll = actionMessage.value;
      if (action === "set_headroom" && typeof actionMessage.value === "number") cameraState.headroom = actionMessage.value;
      if (action === "set_yaw_snap" && typeof actionMessage.value === "number") cameraState.yawSnap = actionMessage.value;
      if (action === "set_pitch_snap" && typeof actionMessage.value === "number") cameraState.pitchSnap = actionMessage.value;
      if (action === "set_roll_snap" && typeof actionMessage.value === "number") cameraState.rollSnap = actionMessage.value;
      if (action === "reset_runtime_camera") {
        cameraState.yaw = 0;
        cameraState.pitch = 0;
        cameraState.roll = 0;
      }
      if (
        action === "set_yaw" ||
        action === "set_pitch" ||
        action === "set_roll" ||
        action === "set_headroom" ||
        action === "set_yaw_snap" ||
        action === "set_pitch_snap" ||
        action === "set_roll_snap" ||
        action === "reset_runtime_camera"
      ) {
        incoming.push(stateLine(cameraState));
      }
    }, focusState);
    const global = globalThis as typeof globalThis & {
      ui?: HostOps;
      frame?: (buttons: number) => void;
    };
    global.ui = host;

    try {
      // The application module owns the real mount; load it only after the
      // test host is installed so its current generated PocketUI tree is
      // exercised. The normal build step produces this ignored artifact.
      await import(`${pathToFileURL(join(import.meta.dir, "..", "dist", "menu.js")).href}?test=menu`);
      const frame = global.frame;
      expect(frame).toBeDefined();
      expect(textUpdates).toContain("ORIENTATION");

      // Poll one authoritative MenuState, then let ordinary idle ticks run.
      frame!(0);
      expect(incoming).toHaveLength(0);
      expect(textUpdates).toEqual(
        expect.arrayContaining(["ORIENTATION", "22.5°", "-7.5°", "15°"]),
      );
      expect(textUpdates).toEqual(expect.arrayContaining(["FRAMING", "Headroom", "0.05"]));

      // A non-default authoritative snapshot must be rendered as-is; the
      // guest has no local 15° fallback for the persisted step sizes.
      Object.assign(cameraState, { headroom: 0.12, yawSnap: 7.5, pitchSnap: 22.5, rollSnap: 30 });
      incoming.push(stateLine(cameraState));
      frame!(0);
      expect(textUpdates).toEqual(expect.arrayContaining(["Headroom", "0.12", "SNAPPING", "Yaw step", "7.5°", "Pitch step", "22.5°", "Roll step", "30°"]));

      // Traverse the actual focus order: Camera → Graphics → Camera. The
      // tabs are the only interaction; no snap-setting action is sent.
      frame!(BTN_DOWN);
      frame!(0);
      frame!(BTN_CIRCLE);
      frame!(0);
      frame!(BTN_DOWN);
      frame!(0);
      frame!(BTN_CIRCLE);
      frame!(0);
      expect(textUpdates).toContain("MSAA");
      frame!(BTN_UP);
      frame!(0);
      frame!(BTN_CIRCLE);
      frame!(0);

      for (let i = 0; i < 8; i++) frame!(0);

      expect(textUpdates).toContain("ORIENTATION");
      expect(runtimeSnaps).toEqual({ headroom: 0.05, yaw_snap_deg: 15, pitch_snap_deg: 15, roll_snap_deg: 15 });
      expect(snapMutations).toEqual([]);
      expect(await readFile(settingsPath, "utf8")).toBe(persistedBefore);
      expect(outgoing).toEqual([]);

      // From the focused Camera tab, walk the actual focus order to YawValue,
      // click it through the host hit-test bridge, then commit a precise edit.
      for (let i = 0; i < 12; i++) {
        frame!(BTN_DOWN);
        frame!(0);
      }
      incoming.push(JSON.stringify({ t: "mouse", x: 170, y: 625, d: true }));
      frame!(0);
      incoming.push(JSON.stringify({ t: "mouse", x: 170, y: 625, d: false }));
      frame!(0);
      incoming.push(
        JSON.stringify({
          t: "input",
          edits: [{ kind: "char", text: "45.25" }],
          ime: [],
          modifiers: { shift: false, control: false, alt: false, super: false },
          cancelled: false,
        }),
      );
      frame!(0);
      incoming.push(
        JSON.stringify({
          t: "input",
          edits: [{ kind: "key", key: "enter" }],
          ime: [],
          modifiers: { shift: false, control: false, alt: false, super: false },
          cancelled: false,
        }),
      );
      frame!(0);
      frame!(0);

      expect(outgoing).toContainEqual({ t: "action", action: "set_yaw", value: 45.25 });
      expect(
        outgoing.some(
          (message) =>
            typeof message === "object" &&
            message !== null &&
            "t" in message &&
            message.t === "text-input-state" &&
            "active" in message &&
             message.active === true &&
            "cursor_area_logical_px" in message &&
            typeof message.cursor_area_logical_px === "object" &&
            message.cursor_area_logical_px !== null &&
            "y" in message.cursor_area_logical_px &&
             message.cursor_area_logical_px.y === 625,
        ),
      ).toBe(true);
      expect(textUpdates).toContain("45.25°");

      // Reset remains the same session-only action and reconciles the next
      // authoritative snapshot back to zero for every orientation axis.
      frame!(BTN_UP);
      frame!(0);
      frame!(BTN_CIRCLE);
      frame!(0);
      frame!(0);
      expect(outgoing).toContainEqual({ t: "action", action: "reset_runtime_camera" });
      expect(textUpdates).toEqual(expect.arrayContaining(["0°"]));

      // The new SNAPPING fields are after the unchanged orientation rows.
      // Commit one explicit step edit and verify it does not touch the pose.
      for (let i = 0; i < 4; i++) {
        frame!(BTN_DOWN);
        frame!(0);
      }
      incoming.push(JSON.stringify({ t: "mouse", x: 170, y: 707, d: true }));
      frame!(0);
      incoming.push(JSON.stringify({ t: "mouse", x: 170, y: 707, d: false }));
      frame!(0);
      incoming.push(
        JSON.stringify({
          t: "input",
          edits: [{ kind: "char", text: "12.5" }],
          ime: [],
          modifiers: { shift: false, control: false, alt: false, super: false },
          cancelled: false,
        }),
      );
      frame!(0);
      incoming.push(
        JSON.stringify({
          t: "input",
          edits: [{ kind: "key", key: "enter" }],
          ime: [],
          modifiers: { shift: false, control: false, alt: false, super: false },
          cancelled: false,
        }),
      );
      frame!(0);
      frame!(0);
      expect(outgoing).toContainEqual({ t: "action", action: "set_yaw_snap", value: 12.5 });
      expect(textUpdates).toContain("12.5°");
      expect(cameraState).toMatchObject({ headroom: 0.12, yaw: 0, pitch: 0, roll: 0, yawSnap: 12.5 });

      // Headroom is the only editable scalar in FRAMING; it uses the same
      // authoritative action path while leaving the runtime pose untouched.
      for (let i = 0; i < 6; i++) {
        frame!(BTN_UP);
        frame!(0);
      }
      incoming.push(JSON.stringify({ t: "mouse", x: 170, y: 559, d: true }));
      frame!(0);
      incoming.push(JSON.stringify({ t: "mouse", x: 170, y: 559, d: false }));
      frame!(0);
      incoming.push(
        JSON.stringify({
          t: "input",
          edits: [{ kind: "char", text: "0.2" }],
          ime: [],
          modifiers: { shift: false, control: false, alt: false, super: false },
          cancelled: false,
        }),
      );
      frame!(0);
      incoming.push(
        JSON.stringify({
          t: "input",
          edits: [{ kind: "key", key: "enter" }],
          ime: [],
          modifiers: { shift: false, control: false, alt: false, super: false },
          cancelled: false,
        }),
      );
      frame!(0);
      frame!(0);
      expect(outgoing).toContainEqual({ t: "action", action: "set_headroom", value: 0.2 });
      expect(cameraState).toMatchObject({ headroom: 0.2, yaw: 0, pitch: 0, roll: 0, yawSnap: 12.5 });

      // The third tab is present in the retained focus order and uses the
      // same inline editor path for its logical-size fields.
      for (let i = 0; i < 7; i++) {
        frame!(BTN_UP);
        frame!(0);
      }
      frame!(BTN_CIRCLE);
      frame!(0);
      expect(textUpdates).toContain("SIZE");
      expect(textUpdates).toEqual(expect.arrayContaining(["Width", "720", "Height", "700"]));

      for (let i = 0; i < 3; i++) {
        frame!(BTN_DOWN);
        frame!(0);
      }
      incoming.push(JSON.stringify({ t: "mouse", x: 170, y: 519, d: true }));
      frame!(0);
      incoming.push(JSON.stringify({ t: "mouse", x: 170, y: 519, d: false }));
      frame!(0);
      incoming.push(
        JSON.stringify({
          t: "input",
          edits: [{ kind: "char", text: "720" }],
          ime: [],
          modifiers: { shift: false, control: false, alt: false, super: false },
          cancelled: false,
        }),
      );
      frame!(0);
      incoming.push(
        JSON.stringify({
          t: "input",
          edits: [{ kind: "key", key: "enter" }],
          ime: [],
          modifiers: { shift: false, control: false, alt: false, super: false },
          cancelled: false,
        }),
      );
      frame!(0);
      expect(outgoing).toContainEqual({ t: "action", action: "set_window_width", value: 720 });

      frame!(BTN_DOWN);
      frame!(0);
      incoming.push(JSON.stringify({ t: "mouse", x: 170, y: 539, d: true }));
      frame!(0);
      incoming.push(JSON.stringify({ t: "mouse", x: 170, y: 539, d: false }));
      frame!(0);
      expect(
        outgoing.some(
          (message) =>
            typeof message === "object" &&
            message !== null &&
            "t" in message &&
            message.t === "text-input-state" &&
            "active" in message &&
            message.active === true,
        ),
      ).toBe(true);
      for (let i = 0; i < 4; i++) {
        frame!(BTN_UP);
        frame!(0);
      }
      frame!(BTN_CIRCLE);
      frame!(0);
      expect(
        outgoing.some(
          (message) =>
            typeof message === "object" &&
            message !== null &&
            "t" in message &&
            message.t === "text-input-state" &&
            "active" in message &&
            message.active === false,
        ),
      ).toBe(true);

      expect(await readFile(settingsPath, "utf8")).toBe(persistedBefore);
    } finally {
      delete global.frame;
      delete global.ui;
      await rm(directory, { recursive: true, force: true });
    }
  });
});
