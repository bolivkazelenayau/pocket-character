import { describe, expect, test } from "bun:test";
import { TextInputDispatcher, decodeTextInputFrame } from "./text-input";

describe("PocketUI text input bridge", () => {
  test("preserves edit/IME order, modifiers, and preedit byte ranges", () => {
    expect(
      decodeTextInputFrame({
        t: "input",
        edits: [
          { kind: "char", text: "é" },
          { kind: "key", key: "backspace" },
        ],
        ime: [
          { kind: "enabled" },
          { kind: "preedit", text: "候補", range_bytes: [3, 6] },
          { kind: "commit", text: "候補" },
          { kind: "disabled" },
        ],
        modifiers: { shift: true, control: false, alt: true, super: false },
        cancelled: false,
      }),
    ).toEqual({
      edits: [
        { kind: "char", text: "é" },
        { kind: "key", key: "backspace" },
      ],
      ime: [
        { kind: "enabled" },
        { kind: "preedit", text: "候補", rangeBytes: [3, 6] },
        { kind: "commit", text: "候補" },
        { kind: "disabled" },
      ],
      modifiers: { shift: true, control: false, alt: true, super: false },
      cancelled: false,
    });
  });

  test("rejects malformed edits rather than inventing input", () => {
    expect(
      decodeTextInputFrame({
        t: "input",
        edits: [{ kind: "key", key: "future-key" }],
        ime: [],
        cancelled: false,
      }),
    ).toBeNull();
  });

  test("delivers cancellation to listeners and clears capture", () => {
    const dispatcher = new TextInputDispatcher();
    const frames: unknown[] = [];
    dispatcher.onFrame((frame) => frames.push(frame));

    dispatcher.dispatch({ t: "input", edits: [], ime: [], cancelled: true });

    expect(frames).toHaveLength(1);
    expect(dispatcher.currentCapture()).toEqual({ active: false, cursorArea: null });
  });
});
