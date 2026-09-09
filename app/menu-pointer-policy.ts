/** Pointer targets that must cancel an inline edit before generic blur. */
export function cancelsInlineEditBeforePointerBlur(debugName: string | null): boolean {
  return debugName === "RestoreDefaults";
}
