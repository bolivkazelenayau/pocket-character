import { getOps } from "@pocketjs/framework/solid";

export interface TextInputCursorArea {
  /** PocketUI logical pixels; the desktop host applies the window scale. */
  x: number;
  y: number;
  width: number;
  height: number;
}

export interface TextInputCapture {
  active: boolean;
  cursorArea: TextInputCursorArea | null;
}

export type TextEditKey =
  | "backspace"
  | "delete"
  | "enter"
  | "tab"
  | "left"
  | "right"
  | "up"
  | "down"
  | "home"
  | "end"
  | "page_up"
  | "page_down"
  | "escape";

export type TextEdit = { kind: "char"; text: string } | { kind: "key"; key: TextEditKey };

export type ImeEdit =
  | { kind: "enabled" }
  // rangeBytes deliberately stays in the byte-index units supplied by winit.
  | { kind: "preedit"; text: string; rangeBytes: [number, number] | null }
  | { kind: "commit"; text: string }
  | { kind: "disabled" };

export interface TextInputModifiers {
  shift: boolean;
  control: boolean;
  alt: boolean;
  super: boolean;
}

export interface TextInputFrame {
  edits: TextEdit[];
  ime: ImeEdit[];
  modifiers: TextInputModifiers;
  cancelled: boolean;
}

export type TextInputListener = (frame: TextInputFrame) => void;

const editKeys = new Set<TextEditKey>([
  "backspace",
  "delete",
  "enter",
  "tab",
  "left",
  "right",
  "up",
  "down",
  "home",
  "end",
  "page_up",
  "page_down",
  "escape",
]);

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function finiteNumber(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value);
}

function decodeEdit(value: unknown): TextEdit | null {
  if (!isRecord(value) || typeof value.kind !== "string") return null;
  if (value.kind === "char" && typeof value.text === "string") {
    return { kind: "char", text: value.text };
  }
  if (value.kind === "key" && typeof value.key === "string" && editKeys.has(value.key as TextEditKey)) {
    return { kind: "key", key: value.key as TextEditKey };
  }
  return null;
}

function decodeIme(value: unknown): ImeEdit | null {
  if (!isRecord(value) || typeof value.kind !== "string") return null;
  if (value.kind === "enabled" || value.kind === "disabled") {
    return { kind: value.kind };
  }
  if ((value.kind === "commit" || value.kind === "preedit") && typeof value.text === "string") {
    if (value.kind === "commit") return { kind: "commit", text: value.text };
    if (value.range_bytes === null) return { kind: "preedit", text: value.text, rangeBytes: null };
    if (
      Array.isArray(value.range_bytes) &&
      value.range_bytes.length === 2 &&
      Number.isSafeInteger(value.range_bytes[0]) &&
      Number.isSafeInteger(value.range_bytes[1]) &&
      value.range_bytes[0] >= 0 &&
      value.range_bytes[1] >= 0
    ) {
      return {
        kind: "preedit",
        text: value.text,
        rangeBytes: [value.range_bytes[0], value.range_bytes[1]],
      };
    }
  }
  return null;
}

function decodeModifiers(value: unknown): TextInputModifiers {
  if (!isRecord(value)) return { shift: false, control: false, alt: false, super: false };
  return {
    shift: value.shift === true,
    control: value.control === true,
    alt: value.alt === true,
    super: value.super === true,
  };
}

/** Decode one host input line while preserving both source stream orders. */
export function decodeTextInputFrame(value: unknown): TextInputFrame | null {
  if (!isRecord(value) || value.t !== "input") return null;
  if (!Array.isArray(value.edits) || !Array.isArray(value.ime)) return null;
  const edits = value.edits.map(decodeEdit);
  const ime = value.ime.map(decodeIme);
  if (edits.some((edit) => edit === null) || ime.some((event) => event === null)) return null;
  return {
    edits: edits as TextEdit[],
    ime: ime as ImeEdit[],
    modifiers: decodeModifiers(value.modifiers),
    cancelled: value.cancelled === true,
  };
}

function sameCursorArea(
  left: TextInputCursorArea | null,
  right: TextInputCursorArea | null,
): boolean {
  if (left === right) return true;
  if (!left || !right) return false;
  return left.x === right.x && left.y === right.y && left.width === right.width && left.height === right.height;
}

function sameCapture(left: TextInputCapture, right: TextInputCapture): boolean {
  return left.active === right.active && sameCursorArea(left.cursorArea, right.cursorArea);
}

function validCursorArea(area: TextInputCursorArea | null): area is TextInputCursorArea {
  return (
    area !== null &&
    finiteNumber(area.x) &&
    finiteNumber(area.y) &&
    finiteNumber(area.width) &&
    finiteNumber(area.height)
  );
}

/**
 * Generic PocketUI text-input owner. Widgets subscribe to host edit/IME
 * frames and publish their logical caret area through `setCapture`.
 */
export class TextInputDispatcher {
  private readonly listeners = new Set<TextInputListener>();
  private capture: TextInputCapture = { active: false, cursorArea: null };
  private reportedCapture: TextInputCapture = { active: false, cursorArea: null };

  onFrame(listener: TextInputListener): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  currentCapture(): TextInputCapture {
    return {
      active: this.capture.active,
      cursorArea: this.capture.cursorArea && { ...this.capture.cursorArea },
    };
  }

  setCapture(active: boolean, cursorArea: TextInputCursorArea | null): void {
    const next: TextInputCapture =
      active && validCursorArea(cursorArea)
        ? { active: true, cursorArea: { ...cursorArea } }
        : { active: false, cursorArea: null };
    if (sameCapture(this.capture, next)) return;
    this.capture = next;
    this.syncCapture();
  }

  /** Retry only an as-yet-unreported ownership transition. */
  syncCapture(): void {
    if (sameCapture(this.capture, this.reportedCapture)) return;
    const ops = getOps();
    if (!ops.svcOpen || !ops.svcSend || !ops.svcOpen("controls")) return;
    ops.svcSend(
      JSON.stringify({
        t: "text-input-state",
        active: this.capture.active,
        cursor_area_logical_px: this.capture.cursorArea,
      }),
    );
    this.reportedCapture = this.capture;
  }

  clearCapture(): void {
    this.setCapture(false, null);
  }

  dispatch(value: unknown): void {
    const frame = decodeTextInputFrame(value);
    if (!frame) return;
    if (frame.cancelled) this.clearCapture();
    for (const listener of this.listeners) listener(frame);
  }
}

export const textInput = new TextInputDispatcher();
