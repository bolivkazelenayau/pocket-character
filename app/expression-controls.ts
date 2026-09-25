export interface ExpressionState {
  index: number;
  name: string;
  weight: number;
  binary: boolean;
  custom: boolean;
}

export function decodeExpressions(value: unknown): ExpressionState[] | null {
  if (!Array.isArray(value)) return null;
  const expressions: ExpressionState[] = [];
  for (const item of value) {
    if (typeof item !== "object" || item === null) return null;
    const expression = item as Record<string, unknown>;
    if (!Number.isSafeInteger(expression.index) || typeof expression.name !== "string" ||
        typeof expression.weight !== "number" || !Number.isFinite(expression.weight) ||
        typeof expression.binary !== "boolean" || typeof expression.custom !== "boolean") return null;
    expressions.push(expression as unknown as ExpressionState);
  }
  return expressions;
}

export function sliderWeight(x: number, drag: { x: number; width: number }): number {
  return Math.round(Math.max(0, Math.min(1, (x - drag.x) / drag.width)) * 100) / 100;
}

export function binaryWeight(enabled: boolean): number {
  return enabled ? 1 : 0;
}
