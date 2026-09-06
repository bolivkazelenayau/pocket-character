/** Compact FOV presentation for the idle camera control. */
export function formatCompactFov(value: number): string {
  return value.toFixed(2).replace(/\.?0+$/, "");
}

export function formatDegrees(value: number): string {
  return `${value.toFixed(2).replace(/\.?0+$/, "")}°`;
}
