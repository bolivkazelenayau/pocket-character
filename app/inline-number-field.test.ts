import { describe, expect, test } from "bun:test";
import {
  InlineNumberFieldModel,
  dispatchInlineNumberPointerDown,
  parseInlineNumberDraft,
  type InlineNumberFieldInput,
} from "./inline-number-field-model";
import type { TextInputCursorArea, TextInputFrame } from "./text-input";

class TestTextInput implements InlineNumberFieldInput {
  private readonly listeners = new Set<(frame: TextInputFrame) => void>();
  capture: { active: boolean; cursorArea: TextInputCursorArea | null } = {
    active: false,
    cursorArea: null,
  };

  onFrame(listener: (frame: TextInputFrame) => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  setCapture(active: boolean, cursorArea: TextInputCursorArea | null): void {
    this.capture = { active, cursorArea };
  }

  clearCapture(): void {
    this.capture = { active: false, cursorArea: null };
  }

  dispatch(frame: TextInputFrame): void {
    for (const listener of this.listeners) listener(frame);
  }
}

function frame(overrides: Partial<TextInputFrame> = {}): TextInputFrame {
  return {
    edits: [],
    ime: [],
    modifiers: { shift: false, control: false, alt: false, super: false },
    cancelled: false,
    ...overrides,
  };
}

function makeField(
  initial = 40,
  id = "FovValue",
  input = new TestTextInput(),
  captureY = 539,
) {
  const commits: number[] = [];
  const field = new InlineNumberFieldModel({
    id,
    value: () => initial,
    format: (value) => (id === "DistanceValue" ? value.toFixed(2) : value.toFixed(1)),
    editFormat: (value) => (id === "DistanceValue" ? value.toFixed(2) : value.toFixed(1)),
    onCommit: (value) => commits.push(value),
    captureArea: (caret) => ({ x: 152 + caret, y: captureY, width: 1, height: 18 }),
    input,
  });
  return { field, input, commits };
}

describe("PocketUI InlineNumberField", () => {
  test("keeps decimal drafts permissive but only parses finite numbers", () => {
    expect(parseInlineNumberDraft("")).toBeNull();
    expect(parseInlineNumberDraft("-")).toBeNull();
    expect(parseInlineNumberDraft(".")).toBeNull();
    expect(parseInlineNumberDraft("55.")).toBe(55);
    expect(parseInlineNumberDraft("55,5")).toBe(55.5);
    expect(parseInlineNumberDraft("1e309")).toBeNull();
  });

  test("starts display-only, then enters editing on a single value click", () => {
    const { field, input } = makeField();
    try {
      expect(field.isEditing()).toBe(false);
      expect(field.selection()).toBeNull();
      expect(field.visualCaret()).toBeNull();
      expect(field.caret()).toBeNull();
      expect(field.anchor()).toBeNull();
      expect(input.capture.active).toBe(false);

      field.pointerDown("FovValue", 170, 539);
      expect(field.isEditing()).toBe(true);
      expect(field.selection()).toEqual({ start: 0, end: 4 });
      expect(field.visualCaret()).toBe(4);
      expect(input.capture.active).toBe(true);
      expect(input.capture.cursorArea).toEqual({ x: 156, y: 539, width: 1, height: 18 });
    } finally {
      field.dispose();
    }
  });

  test("removes caret and selection after cancellation", () => {
    const { field, input } = makeField();
    try {
      field.pointerDown("FovValue", 170, 539);
      expect(field.selection()).not.toBeNull();
      expect(field.visualCaret()).not.toBeNull();

      field.cancel();
      expect(field.isEditing()).toBe(false);
      expect(field.selection()).toBeNull();
      expect(field.visualCaret()).toBeNull();
      expect(field.caret()).toBeNull();
      expect(field.anchor()).toBeNull();
      expect(input.capture.active).toBe(false);
    } finally {
      field.dispose();
    }
  });

  test("buttons do not activate the display-only value field", () => {
    const { field, input } = makeField();
    try {
      dispatchInlineNumberPointerDown("FovDecrement", 148, 539);
      expect(field.isEditing()).toBe(false);
      dispatchInlineNumberPointerDown("FovIncrement", 196, 539);
      dispatchInlineNumberPointerDown("SaveCamera", 200, 500);
      dispatchInlineNumberPointerDown("ResetRuntimeCamera", 200, 500);
      expect(field.isEditing()).toBe(false);
      expect(input.capture.active).toBe(false);
    } finally {
      field.dispose();
    }
  });

  test("reopens authoritative precision instead of the compact idle display", () => {
    const input = new TestTextInput();
    const commits: number[] = [];
    let value = 40.25;
    const idleFormat = (next: number) => next.toFixed(1);
    const field = new InlineNumberFieldModel({
      id: "FovValue",
      value: () => value,
      format: idleFormat,
      editFormat: (next) => next.toString(),
      onCommit: (next) => {
        value = next;
        commits.push(next);
      },
      captureArea: (caret) => ({ x: 152 + caret, y: 539, width: 1, height: 18 }),
      input,
    });
    try {
      expect(idleFormat(value)).toBe("40.3");

      field.beginEditing();
      expect(field.draft()).toBe("40.25");
      expect(field.selection()).toEqual({ start: 0, end: 5 });

      input.dispatch(frame({ edits: [{ kind: "key", key: "enter" }] }));
      expect(commits).toEqual([40.25]);
      expect(value).toBe(40.25);

      field.beginEditing();
      expect(field.draft()).toBe("40.25");
    } finally {
      field.dispose();
    }
  });

  test("normalizes a committed ASCII comma to a decimal point", () => {
    const { field, input } = makeField();
    try {
      field.beginEditing();
      input.dispatch(frame({ edits: [{ kind: "char", text: "55,5" }] }));
      expect(field.draft()).toBe("55.5");
    } finally {
      field.dispose();
    }
  });

  test("normalizes Russian decimal aliases and rejects other printable characters", () => {
    const { field, input } = makeField();
    try {
      field.beginEditing();
      input.dispatch(frame({ edits: [{ kind: "char", text: "55" }] }));
      expect(field.draft()).toBe("55");

      input.dispatch(frame({ edits: [{ kind: "char", text: "ф" }] }));
      input.dispatch(frame({ edits: [{ kind: "char", text: "a" }] }));
      input.dispatch(frame({ edits: [{ kind: "char", text: "🙂" }] }));
      expect(field.draft()).toBe("55");
      expect(field.visualCaret()).toBe(2);
      expect(input.capture.cursorArea).toEqual({ x: 154, y: 539, width: 1, height: 18 });

      input.dispatch(frame({ edits: [{ kind: "char", text: "ю" }] }));
      expect(field.draft()).toBe("55.");
      expect(field.visualCaret()).toBe(3);

      const afterFirstDecimal = field.snapshot();
      const afterFirstDecimalCapture = input.capture;
      for (const text of ["ю", "Ю", ","]) {
        input.dispatch(frame({ edits: [{ kind: "char", text }] }));
        expect(field.snapshot()).toEqual(afterFirstDecimal);
        expect(input.capture).toEqual(afterFirstDecimalCapture);
      }
    } finally {
      field.dispose();
    }
  });

  test("normalizes uppercase Russian decimal alias", () => {
    const { field, input } = makeField();
    try {
      field.beginEditing();
      input.dispatch(frame({ edits: [{ kind: "char", text: "55" }, { kind: "char", text: "Ю" }] }));
      expect(field.draft()).toBe("55.");
      expect(field.visualCaret()).toBe(3);
    } finally {
      field.dispose();
    }
  });

  test("rejects malformed insertion without disturbing a selected draft", () => {
    const { field, input } = makeField();
    try {
      field.beginEditing();
      const before = field.snapshot();
      const beforeCapture = input.capture;

      input.dispatch(frame({ edits: [{ kind: "char", text: "ф" }] }));

      expect(field.snapshot()).toEqual(before);
      expect(input.capture).toEqual(beforeCapture);
    } finally {
      field.dispose();
    }
  });

  test("preserves valid partial numeric drafts", () => {
    const { field, input } = makeField();
    try {
      field.beginEditing();
      input.dispatch(frame({ edits: [{ kind: "char", text: "-" }] }));
      expect(field.draft()).toBe("-");
      input.dispatch(frame({ edits: [{ kind: "char", text: "." }] }));
      expect(field.draft()).toBe("-.");
      input.dispatch(frame({ edits: [{ kind: "char", text: "5" }] }));
      expect(field.draft()).toBe("-.5");

      field.cancel();
      field.beginEditing();
      input.dispatch(frame({ edits: [{ kind: "char", text: "55." }] }));
      expect(field.draft()).toBe("55.");
    } finally {
      field.dispose();
    }
  });

  test("transferring from FOV to Distance leaves Distance as the capture owner", () => {
    const input = new TestTextInput();
    const fov = makeField(40, "FovValue", input, 539);
    const distance = makeField(0.6, "DistanceValue", input, 519);
    try {
      dispatchInlineNumberPointerDown("FovValue", 170, 539);
      dispatchInlineNumberPointerDown("DistanceValue", 170, 519);

      expect(fov.field.isEditing()).toBe(false);
      expect(distance.field.isEditing()).toBe(true);
      expect(input.capture).toEqual({
        active: true,
        cursorArea: { x: 156, y: 519, width: 1, height: 18 },
      });
    } finally {
      distance.field.dispose();
      fov.field.dispose();
    }
  });

  test("transferring from Distance to FOV leaves FOV as the capture owner", () => {
    const input = new TestTextInput();
    const distance = makeField(0.6, "DistanceValue", input, 519);
    const fov = makeField(40, "FovValue", input, 539);
    try {
      dispatchInlineNumberPointerDown("DistanceValue", 170, 519);
      dispatchInlineNumberPointerDown("FovValue", 170, 539);

      expect(distance.field.isEditing()).toBe(false);
      expect(fov.field.isEditing()).toBe(true);
      expect(input.capture).toEqual({
        active: true,
        cursorArea: { x: 156, y: 539, width: 1, height: 18 },
      });
    } finally {
      fov.field.dispose();
      distance.field.dispose();
    }
  });

  test("clicking elsewhere applies existing blur commit/cancel semantics", () => {
    const { field, input, commits } = makeField();
    try {
      field.pointerDown("FovValue", 170, 539);
      input.dispatch(frame({ edits: [{ kind: "char", text: "60" }] }));
      field.pointerDown("SaveCamera", 200, 500);
      expect(commits).toEqual([60]);
      expect(field.isEditing()).toBe(false);

      field.pointerDown("FovValue", 170, 539);
      input.dispatch(frame({ edits: [{ kind: "key", key: "home" }, { kind: "key", key: "delete" }] }));
      input.dispatch(frame({ edits: [{ kind: "key", key: "delete" }, { kind: "key", key: "delete" }, { kind: "key", key: "delete" }] }));
      field.pointerDown("SaveCamera", 200, 500);
      expect(commits).toEqual([60]);
      expect(field.isEditing()).toBe(false);
    } finally {
      field.dispose();
    }
  });

  test("initial selection is replaced by typing and Enter commits exactly once", () => {
    const { field, input, commits } = makeField();
    try {
      field.beginEditing();
      input.dispatch(frame({ edits: [{ kind: "char", text: "55.5" }] }));
      expect(field.draft()).toBe("55.5");
      input.dispatch(frame({ edits: [{ kind: "key", key: "enter" }] }));
      expect(commits).toEqual([55.5]);
      expect(field.isEditing()).toBe(false);
      input.dispatch(frame({ edits: [{ kind: "key", key: "enter" }] }));
      expect(commits).toEqual([55.5]);
      expect(input.capture.active).toBe(false);
    } finally {
      field.dispose();
    }
  });

  test("Escape and cancellation discard the draft and release capture", () => {
    const { field, input, commits } = makeField();
    try {
      field.beginEditing();
      input.dispatch(frame({ edits: [{ kind: "char", text: "55" }, { kind: "key", key: "escape" }] }));
      expect(commits).toEqual([]);
      expect(field.isEditing()).toBe(false);
      expect(input.capture.active).toBe(false);

      field.beginEditing();
      input.dispatch(frame({ edits: [{ kind: "char", text: "55" }], cancelled: true }));
      expect(field.isEditing()).toBe(false);
      expect(commits).toEqual([]);
    } finally {
      field.dispose();
    }
  });

  test("valid blur commits while empty or invalid blur cancels", () => {
    const { field, input, commits } = makeField();
    try {
      field.beginEditing();
      input.dispatch(frame({ edits: [{ kind: "char", text: "60" }] }));
      field.pointerDown("SaveCamera", 200, 500);
      expect(commits).toEqual([60]);

      field.beginEditing();
      input.dispatch(frame({ edits: [{ kind: "key", key: "home" }, { kind: "key", key: "delete" }] }));
      input.dispatch(frame({ edits: [{ kind: "key", key: "delete" }, { kind: "key", key: "delete" }, { kind: "key", key: "delete" }] }));
      field.pointerDown("SaveCamera", 200, 500);
      expect(commits).toEqual([60]);
      expect(field.isEditing()).toBe(false);
    } finally {
      field.dispose();
    }
  });

  test("orientation fields commit raw values and transfer native capture ownership", () => {
    const input = new TestTextInput();
    const committed: Array<{ id: string; value: number }> = [];
    const authoritative = new Map([
      ["YawValue", 22.5],
      ["PitchValue", -7.5],
      ["RollValue", 15],
    ]);
    const fields = [
      { id: "YawValue", captureY: 605 },
      { id: "PitchValue", captureY: 625 },
      { id: "RollValue", captureY: 645 },
    ].map(({ id, captureY }) => {
      const field = new InlineNumberFieldModel({
        id,
        value: () => authoritative.get(id) ?? null,
        format: (value) => `${value.toFixed(1)}°`,
        editFormat: (value) => value.toString(),
        onCommit: (value) => {
          authoritative.set(id, value);
          committed.push({ id, value });
        },
        captureArea: (caret) => ({ x: 152 + caret, y: captureY, width: 1, height: 18 }),
        input,
      });
      return { field, id };
    });

    try {
      dispatchInlineNumberPointerDown("YawValue", 170, 605);
      expect(input.capture.cursorArea?.y).toBe(605);
      input.dispatch(frame({ edits: [{ kind: "char", text: "190.25" }] }));
      input.dispatch(frame({ edits: [{ kind: "key", key: "enter" }] }));

      dispatchInlineNumberPointerDown("PitchValue", 170, 625);
      expect(fields[0].field.isEditing()).toBe(false);
      expect(input.capture.cursorArea?.y).toBe(625);
      input.dispatch(frame({ edits: [{ kind: "char", text: "100" }] }));
      input.dispatch(frame({ edits: [{ kind: "key", key: "escape" }] }));

      dispatchInlineNumberPointerDown("RollValue", 170, 645);
      input.dispatch(frame({ edits: [{ kind: "char", text: "-190" }] }));
      dispatchInlineNumberPointerDown("YawValue", 170, 605);

      expect(committed).toEqual([
        { id: "YawValue", value: 190.25 },
        { id: "RollValue", value: -190 },
      ]);
      expect(authoritative.get("YawValue")).toBe(190.25);
      expect(authoritative.get("PitchValue")).toBe(-7.5);
      expect(authoritative.get("RollValue")).toBe(-190);
      expect(input.capture).toEqual({
        active: true,
        cursorArea: { x: 158, y: 605, width: 1, height: 18 },
      });
    } finally {
      for (const { field } of fields.reverse()) field.dispose();
    }
  });

  test("Backspace, Delete, caret movement, and IME preedit remain local to the draft", () => {
    const { field, input } = makeField();
    try {
      field.beginEditing();
      input.dispatch(frame({ edits: [{ kind: "char", text: "1234" }] }));
      input.dispatch(frame({ edits: [{ kind: "key", key: "home" }, { kind: "key", key: "right" }] }));
      input.dispatch(frame({ edits: [{ kind: "key", key: "delete" }] }));
      expect(field.draft()).toBe("134");
      input.dispatch(frame({ edits: [{ kind: "key", key: "backspace" }] }));
      expect(field.draft()).toBe("34");

      input.dispatch(frame({ ime: [{ kind: "preedit", text: "55", rangeBytes: [0, 2] }] }));
      expect(field.draft()).toBe("34");
      expect(field.displayText()).toBe("5534");
      input.dispatch(frame({ ime: [{ kind: "disabled" }] }));
      expect(field.draft()).toBe("34");
      input.dispatch(frame({ ime: [{ kind: "preedit", text: "60", rangeBytes: [0, 2] }] }));
      input.dispatch(frame({ ime: [{ kind: "commit", text: "60" }] }));
      expect(field.draft()).toBe("6034");
    } finally {
      field.dispose();
    }
  });

  test("uses UTF-8 byte length for a null-range Unicode IME caret", () => {
    const { field, input } = makeField();
    try {
      field.beginEditing();
      input.dispatch(frame({ ime: [{ kind: "preedit", text: "５５", rangeBytes: null }] }));

      expect(field.displayText()).toBe("５５");
      expect(field.visualCaret()).toBe(2);
      expect(input.capture.cursorArea).toEqual({ x: 154, y: 539, width: 1, height: 18 });
    } finally {
      field.dispose();
    }
  });
});
