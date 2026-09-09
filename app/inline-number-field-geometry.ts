import type { TextInputCursorArea } from "./text-input";
import type { NodeMirror } from "@pocketjs/framework/components";

export interface InlineNumberFieldBounds {
  x: number;
  y: number;
  width: number;
  height: number;
}

export type InlineNumberTextMeasure = (text: string) => number;
export type InlineNumberLayoutOf = (
  id: number,
) => readonly [number, number, number, number] | null;

const FALLBACK_CHAR_WIDTH = 7;

function fallbackTextWidth(text: string): number {
  return text.length * FALLBACK_CHAR_WIDTH;
}

function measuredTextWidth(text: string, measureText?: InlineNumberTextMeasure): number {
  const measured = measureText?.(text);
  return measured !== undefined && Number.isFinite(measured) && measured >= 0
    ? measured
    : fallbackTextWidth(text);
}

function clamp(value: number, min: number, max: number): number {
  return Math.max(min, Math.min(max, value));
}

/** Convert PocketUI's parent-relative layout boxes into world bounds. */
export function layoutBoundsFromInlineNumberNode(
  node: NodeMirror,
  layoutOf: InlineNumberLayoutOf,
): InlineNumberFieldBounds | null {
  let current: NodeMirror | null = node;
  let x = 0;
  let y = 0;
  let width = 0;
  let height = 0;
  while (current) {
    const layout = layoutOf(current.id);
    if (!layout || layout.length !== 4 || layout.some((value) => !Number.isFinite(value))) {
      return null;
    }
    x += layout[0];
    y += layout[1];
    if (current === node) {
      width = layout[2];
      height = layout[3];
    }
    current = current.parent;
  }
  if (width <= 0 || height <= 0) return null;
  return { x, y, width, height };
}

export function boundsInsideInlineNumberViewport(
  bounds: InlineNumberFieldBounds,
  viewport: { w: number; h: number },
): boolean {
  return (
    bounds.x >= 0 &&
    bounds.y >= 0 &&
    bounds.x + bounds.width <= viewport.w &&
    bounds.y + bounds.height <= viewport.h
  );
}

/**
 * Return the native cursor rectangle for the text actually shown by the
 * field. `displayedText` intentionally includes an active IME preedit.
 */
export function captureAreaFromInlineNumberBounds(
  bounds: InlineNumberFieldBounds,
  caret: number,
  displayedText: string,
  measureText?: InlineNumberTextMeasure,
): TextInputCursorArea {
  const textWidth = Math.min(bounds.width, measuredTextWidth(displayedText, measureText));
  const textLeft = Math.max(0, (bounds.width - textWidth) / 2);
  const position = clamp(caret, 0, displayedText.length);
  const prefixWidth = Math.min(textWidth, measuredTextWidth(displayedText.slice(0, position), measureText));
  return {
    x: bounds.x + clamp(textLeft + prefixWidth, 0, Math.max(0, bounds.width - 1)),
    y: bounds.y,
    width: 1,
    height: bounds.height,
  };
}

/** Map a pointer x-coordinate to a UTF-16 caret position in the displayed text. */
export function caretFromInlineNumberBounds(
  bounds: InlineNumberFieldBounds,
  x: number,
  displayedText: string,
  measureText?: InlineNumberTextMeasure,
): number {
  const textWidth = Math.min(bounds.width, measuredTextWidth(displayedText, measureText));
  const textLeft = Math.max(0, (bounds.width - textWidth) / 2);
  let bestCaret = 0;
  let bestDistance = Number.POSITIVE_INFINITY;
  for (let caret = 0; caret <= displayedText.length; caret += 1) {
    const candidate = bounds.x + textLeft + Math.min(textWidth, measuredTextWidth(displayedText.slice(0, caret), measureText));
    const distance = Math.abs(x - candidate);
    if (distance < bestDistance) {
      bestDistance = distance;
      bestCaret = caret;
    }
  }
  return bestCaret;
}
