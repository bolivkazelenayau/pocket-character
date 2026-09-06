import { createSignal, onCleanup } from "solid-js";
import { Focusable, Text, View } from "@pocketjs/framework/components";
import {
  InlineNumberFieldModel,
  type InlineNumberFieldOptions,
} from "./inline-number-field-model";
import type { TextInputCursorArea } from "./text-input";

export const INLINE_NUMBER_CELL_WIDTH = 42;
export const INLINE_NUMBER_CHAR_WIDTH = 7;
export const INLINE_NUMBER_CELL_HEIGHT = 18;

export { cancelInlineNumberFields, dispatchInlineNumberPointerDown } from "./inline-number-field-model";
export { parseInlineNumberDraft } from "./inline-number-field-model";
export { InlineNumberFieldModel } from "./inline-number-field-model";
export type {
  InlineNumberFieldInput,
  InlineNumberFieldOptions,
} from "./inline-number-field-model";

export interface InlineNumberFieldProps {
  id: string;
  debugName: string;
  value: () => number | null;
  format: (value: number) => string;
  editFormat: (value: number) => string;
  onCommit: (value: number) => void;
  captureArea: (caret: number, draft: string) => TextInputCursorArea;
  caretFromPointer?: (x: number, draft: string) => number;
}

function textWidth(text: string): number {
  return Math.min(INLINE_NUMBER_CELL_WIDTH, text.length * INLINE_NUMBER_CHAR_WIDTH);
}

export function InlineNumberField(props: InlineNumberFieldProps) {
  const model = new InlineNumberFieldModel(props satisfies InlineNumberFieldOptions);
  const [revision, setRevision] = createSignal(0);
  const unsubscribe = model.subscribe(() => setRevision((value) => value + 1));
  onCleanup(() => {
    unsubscribe();
    model.dispose();
  });

  const editing = () => {
    revision();
    return model.isEditing();
  };
  const display = () => {
    revision();
    const value = props.value();
    return editing() ? model.displayText() : value === null || !Number.isFinite(value) ? "—" : props.format(value);
  };
  const selectionStyle = () => {
    revision();
    const selection = model.selection();
    if (!selection) return { insetL: 0, width: 0 };
    const text = display();
    const left = Math.max(0, (INLINE_NUMBER_CELL_WIDTH - textWidth(text)) / 2);
    return {
      insetL: left + selection.start * INLINE_NUMBER_CHAR_WIDTH,
      width: Math.max(1, (selection.end - selection.start) * INLINE_NUMBER_CHAR_WIDTH),
    };
  };
  const caretStyle = () => {
    revision();
    const text = display();
    const left = Math.max(0, (INLINE_NUMBER_CELL_WIDTH - textWidth(text)) / 2);
    const caret = model.visualCaret();
    const position = Math.min(caret ?? 0, text.length);
    return {
      insetL: Math.min(INLINE_NUMBER_CELL_WIDTH - 1, left + position * INLINE_NUMBER_CHAR_WIDTH),
    };
  };

  return (
    <Focusable
      debugName={props.debugName}
      class={
        editing()
          ? "relative h-[18] w-[42] flex-col items-center justify-center rounded-sm bg-[#16384b]"
          : "relative h-[18] w-[42] flex-col items-center justify-center rounded-sm"
      }
      onPress={() => undefined}
    >
      {editing() && model.selection() !== null ? (
        <View
          class="absolute rounded-sm bg-[#7fd0ff66]"
          style={{ ...selectionStyle(), insetT: 2, height: 14 }}
        />
      ) : null}
      <Text class="relative text-center text-xs font-mono text-[#e8f1f8]">
        {display()}
      </Text>
      {editing() ? (
        <View
          class="absolute bg-[#e8f1f8]"
          style={{ ...caretStyle(), insetT: 2, width: 1, height: 14 }}
        />
      ) : null}
    </Focusable>
  );
}
