import { type TextInputCursorArea, type TextInputFrame, textInput } from "./text-input";
import { cancelsInlineEditBeforePointerBlur } from "./menu-pointer-policy";

export interface InlineNumberFieldInput {
  onFrame(listener: (frame: TextInputFrame) => void): () => void;
  setCapture(active: boolean, cursorArea: TextInputCursorArea | null): void;
  clearCapture(): void;
}

export interface InlineNumberFieldOptions {
  id: string;
  value: () => number | null;
  /** Compact presentation formatter used only while the field is idle. */
  format: (value: number) => string;
  /** Authoritative formatter used to seed a lossless edit draft. */
  editFormat: (value: number) => string;
  onCommit: (value: number) => void;
  /** Return the logical caret rectangle for the currently displayed text. */
  captureArea: (caret: number, draft: string, displayedText: string) => TextInputCursorArea | null;
  /** Optional mapping for placing the caret from a pointer press. */
  caretFromPointer?: (x: number, draft: string, displayedText: string) => number;
  input?: InlineNumberFieldInput;
}

export interface InlineNumberFieldSnapshot {
  editing: boolean;
  draft: string;
  caret: number | null;
  anchor: number | null;
  preedit: string | null;
}

function normalizeInlineNumberText(text: string): string {
  return text.replace(/[,юЮ]/g, ".");
}

const partialNumericDraftPattern = /^-?(?:\d+(?:\.\d*)?|\.\d*)?$/;

function isValidInlineNumberDraft(draft: string): boolean {
  return partialNumericDraftPattern.test(normalizeInlineNumberText(draft));
}

export function parseInlineNumberDraft(draft: string): number | null {
  const normalized = normalizeInlineNumberText(draft).trim();
  if (normalized === "") return null;
  if (!isValidInlineNumberDraft(normalized)) return null;
  const value = Number(normalized);
  return Number.isFinite(value) ? value : null;
}

function selectionBounds(caret: number, anchor: number): [number, number] {
  return caret <= anchor ? [caret, anchor] : [anchor, caret];
}

function utf8CodePointByteLength(character: string): number {
  const codePoint = character.codePointAt(0) ?? 0;
  if (codePoint <= 0x7f) return 1;
  if (codePoint <= 0x7ff) return 2;
  if (codePoint <= 0xffff) return 3;
  return 4;
}

function utf8ByteLength(text: string): number {
  let bytes = 0;
  for (const character of text) bytes += utf8CodePointByteLength(character);
  return bytes;
}

function utf8ByteOffsetToUtf16Index(text: string, byteOffset: number): number {
  if (!Number.isSafeInteger(byteOffset) || byteOffset <= 0) return 0;
  let bytes = 0;
  let index = 0;
  for (const character of text) {
    const characterBytes = utf8CodePointByteLength(character);
    if (bytes + characterBytes > byteOffset) break;
    bytes += characterBytes;
    index += character.length;
  }
  return index;
}

/**
 * PocketUI-owned state machine for a compact inline numeric editor. The
 * native bridge only delivers frames; this model owns the draft, selection,
 * composition, commit policy, and capture lifetime.
 */
export class InlineNumberFieldModel {
  private readonly options: InlineNumberFieldOptions;
  private readonly input: InlineNumberFieldInput;
  private readonly listeners = new Set<() => void>();
  private readonly removeInputListener: () => void;
  private editingState = false;
  private draftState = "";
  private caretState = 0;
  private anchorState = 0;
  private preeditState: { text: string; cursor: number } | null = null;
  private preeditSelectionState: { start: number; end: number } | null = null;
  constructor(options: InlineNumberFieldOptions) {
    this.options = options;
    this.input = options.input ?? textInput;
    this.removeInputListener = this.input.onFrame((frame) => this.handleFrame(frame));
    inlineNumberFields.add(this);
  }

  subscribe(listener: () => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  dispose(): void {
    this.removeInputListener();
    if (this.editingState) this.input.clearCapture();
    this.editingState = false;
    inlineNumberFields.delete(this);
  }

  id(): string {
    return this.options.id;
  }

  isEditing(): boolean {
    return this.editingState;
  }

  draft(): string {
    return this.draftState;
  }

  caret(): number | null {
    return this.editingState ? this.caretState : null;
  }

  anchor(): number | null {
    return this.editingState ? this.anchorState : null;
  }

  preedit(): { text: string; cursor: number } | null {
    return this.preeditState && { ...this.preeditState };
  }

  displayText(): string {
    const preedit = this.preeditState;
    if (!preedit) return this.draftState;
    const [start, end] = this.preeditSelectionState
      ? [this.preeditSelectionState.start, this.preeditSelectionState.end]
      : [this.caretState, this.caretState];
    return this.draftState.slice(0, start) + preedit.text + this.draftState.slice(end);
  }

  visualCaret(): number | null {
    if (!this.editingState) return null;
    return this.preeditState
      ? (this.preeditSelectionState?.start ?? this.caretState) + this.preeditState.cursor
      : this.caretState;
  }

  selection(): { start: number; end: number } | null {
    if (!this.editingState || this.preeditState) return null;
    const [start, end] = selectionBounds(this.caretState, this.anchorState);
    return start === end ? null : { start, end };
  }

  snapshot(): InlineNumberFieldSnapshot {
    return {
      editing: this.editingState,
      draft: this.draftState,
      caret: this.editingState ? this.caretState : null,
      anchor: this.editingState ? this.anchorState : null,
      preedit: this.preeditState?.text ?? null,
    };
  }

  /** Called by the menu’s pointer bridge on a new pointer-down edge. */
  pointerDown(targetId: string | null, x: number, y: number): void {
    if (targetId !== this.options.id) {
      if (this.editingState) this.commitOrCancelOnBlur();
      return;
    }

    if (this.editingState) {
      if (this.options.caretFromPointer) {
        // Pointer coordinates are applied to the restored draft after an
        // active composition is discarded. Mapping against displayText()
        // would count the IME replacement text and then apply that visual
        // index to a different UTF-16 string.
        const draft = this.draftState;
        this.preeditState = null;
        this.preeditSelectionState = null;
        this.setCaret(this.options.caretFromPointer(x, draft, draft));
        this.refreshCapture();
        this.notify();
      }
      return;
    }

    // A numeric value is an explicit inline editor: the first click both
    // focuses it through the menu's Focusable target and captures text input.
    this.beginEditing();
  }

  beginEditing(): void {
    if (this.editingState) return;
    const value = this.options.value();
    if (value === null || !Number.isFinite(value)) return;
    this.draftState = this.options.editFormat(value);
    this.caretState = this.draftState.length;
    this.anchorState = 0;
    this.preeditState = null;
    this.preeditSelectionState = null;
    this.editingState = true;
    this.refreshCapture();
    this.notify();
  }

  /** Commit a finite number, or cancel the invalid draft. */
  commit(): boolean {
    if (!this.editingState) return false;
    const value = parseInlineNumberDraft(this.draftState);
    if (value === null) {
      this.cancel();
      return false;
    }
    this.options.onCommit(value);
    this.finishEditing();
    return true;
  }

  cancel(): void {
    if (!this.editingState) return;
    this.finishEditing();
  }

  commitOrCancelOnBlur(): void {
    if (!this.editingState) return;
    if (!this.commit()) this.cancel();
  }

  handleFrame(frame: TextInputFrame): void {
    if (frame.cancelled) {
      this.cancel();
      return;
    }
    if (!this.editingState) return;

    let changed = false;
    // IME events are applied before key edits from the same host frame. A
    // native IME commit therefore lands before any following Enter/arrow
    // event, while preedit remains display-only until committed.
    for (const event of frame.ime) {
      if (!this.editingState) break;
      switch (event.kind) {
        case "enabled":
          break;
        case "disabled":
          if (this.preeditState) {
            this.preeditState = null;
            this.preeditSelectionState = null;
            changed = true;
          }
          break;
        case "preedit":
          if (!this.preeditState) {
            this.preeditSelectionState = this.selection() ?? {
              start: this.caretState,
              end: this.caretState,
            };
          }
          this.preeditState =
            event.text === ""
              ? null
              : {
                  text: event.text,
                  cursor: utf8ByteOffsetToUtf16Index(
                    event.text,
                    event.rangeBytes?.[1] ?? utf8ByteLength(event.text),
                  ),
                };
          if (!this.preeditState) this.preeditSelectionState = null;
          changed = true;
          break;
        case "commit": {
          const preeditSelection = this.preeditSelectionState;
          const hadPreedit = this.preeditState !== null || preeditSelection !== null;
          this.preeditState = null;
          this.preeditSelectionState = null;
          if (event.text !== "") {
            if (preeditSelection) {
              changed =
                this.insertTextAt(event.text, preeditSelection.start, preeditSelection.end) ||
                changed;
            } else {
              changed = this.insertText(event.text) || changed;
            }
          }
          changed = hadPreedit || changed;
          break;
        }
      }
    }

    for (const edit of frame.edits) {
      if (!this.editingState) break;
      if (edit.kind === "char") {
        if (!this.preeditState && edit.text !== "") {
          changed = this.insertText(edit.text) || changed;
        }
        continue;
      }
      if (edit.key === "escape") {
        this.cancel();
        break;
      }
      // While composing, the IME owns navigation and editing keys. Escape
      // remains available as the host’s explicit interaction cancellation.
      if (this.preeditState) continue;
      switch (edit.key) {
        case "backspace":
          changed = this.deleteBackward() || changed;
          break;
        case "delete":
          changed = this.deleteForward() || changed;
          break;
        case "left":
          changed = this.moveCaret(-1, frame.modifiers.shift) || changed;
          break;
        case "right":
          changed = this.moveCaret(1, frame.modifiers.shift) || changed;
          break;
        case "home":
          changed = this.moveTo(0, frame.modifiers.shift) || changed;
          break;
        case "end":
          changed = this.moveTo(this.draftState.length, frame.modifiers.shift) || changed;
          break;
        case "enter":
        case "tab":
          this.commit();
          break;
        default:
          break;
      }
    }

    if (changed && this.editingState) {
      this.refreshCapture();
      this.notify();
    }
  }

  private insertText(text: string): boolean {
    const [start, end] = selectionBounds(this.caretState, this.anchorState);
    return this.insertTextAt(text, start, end);
  }

  private insertTextAt(text: string, start: number, end: number): boolean {
    const normalizedText = normalizeInlineNumberText(text);
    const candidate = this.draftState.slice(0, start) + normalizedText + this.draftState.slice(end);
    if (!isValidInlineNumberDraft(candidate)) return false;

    this.draftState = candidate;
    this.caretState = start + normalizedText.length;
    this.anchorState = this.caretState;
    this.preeditState = null;
    this.preeditSelectionState = null;
    return true;
  }

  private replaceSelection(text: string): boolean {
    return this.insertText(text);
  }

  private deleteBackward(): boolean {
    if (this.caretState !== this.anchorState) {
      return this.replaceSelection("");
    }
    if (this.caretState === 0) return false;
    this.draftState =
      this.draftState.slice(0, this.caretState - 1) + this.draftState.slice(this.caretState);
    this.caretState -= 1;
    this.anchorState = this.caretState;
    return true;
  }

  private deleteForward(): boolean {
    if (this.caretState !== this.anchorState) {
      return this.replaceSelection("");
    }
    if (this.caretState >= this.draftState.length) return false;
    this.draftState =
      this.draftState.slice(0, this.caretState) + this.draftState.slice(this.caretState + 1);
    return true;
  }

  private moveCaret(direction: -1 | 1, extend: boolean): boolean {
    const next = Math.max(0, Math.min(this.draftState.length, this.caretState + direction));
    return this.moveTo(next, extend);
  }

  private moveTo(next: number, extend: boolean): boolean {
    const bounded = Math.max(0, Math.min(this.draftState.length, next));
    const changed = bounded !== this.caretState || (!extend && this.anchorState !== bounded);
    this.caretState = bounded;
    if (!extend) this.anchorState = bounded;
    return changed;
  }

  private setCaret(next: number): void {
    const bounded = Math.max(0, Math.min(this.draftState.length, next));
    this.caretState = bounded;
    this.anchorState = bounded;
  }

  private refreshCapture(): void {
    if (!this.editingState) return;
    const caret = this.visualCaret();
    if (caret === null) return;
    this.input.setCapture(true, this.options.captureArea(caret, this.draftState, this.displayText()));
  }

  private finishEditing(): void {
    this.editingState = false;
    this.preeditState = null;
    this.preeditSelectionState = null;
    this.input.clearCapture();
    this.notify();
  }

  private notify(): void {
    for (const listener of this.listeners) listener();
  }
}

const inlineNumberFields = new Set<InlineNumberFieldModel>();

/** Route existing menu pointer facts to every mounted inline field. */
export function dispatchInlineNumberPointerDown(
  targetId: string | null,
  x: number,
  y: number,
): void {
  if (cancelsInlineEditBeforePointerBlur(targetId)) {
    cancelInlineNumberFields();
    return;
  }
  const fields = [...inlineNumberFields];
  // Blur first so a non-target editor cannot clear the capture acquired by
  // the target editor later in this same pointer transition. The activation
  // result must not depend on component insertion order.
  for (const field of fields) {
    if (field.id() !== targetId) field.pointerDown(targetId, x, y);
  }
  for (const field of fields) {
    if (field.id() === targetId) field.pointerDown(targetId, x, y);
  }
}

export function cancelInlineNumberFields(): void {
  for (const field of inlineNumberFields) {
    field.cancel();
  }
}
