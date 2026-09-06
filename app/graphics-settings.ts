export const MSAA_OPTIONS = [
  { preference: "off", samples: 1, label: "Off" },
  { preference: "2x", samples: 2, label: "2×" },
  { preference: "4x", samples: 4, label: "4×" },
  { preference: "8x", samples: 8, label: "8×" },
] as const;

export type MsaaPreference = (typeof MSAA_OPTIONS)[number]["preference"];

export type AaActionMessage =
  | { t: "action"; action: "request_msaa"; value: number }
  | { t: "action"; action: "request_smaa"; value: boolean };

export function msaaSamples(preference: MsaaPreference): number {
  return MSAA_OPTIONS.find((option) => option.preference === preference)?.samples ?? 1;
}

export function msaaPreferenceLabel(preference: MsaaPreference | null): string {
  if (preference === null) return "—";
  return preference === "off" ? "Off" : preference.replace("x", "×");
}

/**
 * Keep the UI request payload dumb and explicit. Rust remains the validation
 * boundary and turns this wire message into ControlAction.
 */
export function encodeMsaaRequest(preference: MsaaPreference): AaActionMessage {
  return { t: "action", action: "request_msaa", value: msaaSamples(preference) };
}

export function encodeSmaaRequest(enabled: boolean): AaActionMessage {
  return { t: "action", action: "request_smaa", value: enabled };
}

export function effectiveMsaaLabel(samples: number | null): string {
  if (samples === null) return "—";
  return samples <= 1 ? "Off" : `${samples}×`;
}

export function smaaStateLabel(enabled: boolean | null): string {
  if (enabled === null) return "—";
  return enabled ? "On" : "Off";
}

export function formatMsaaStatus(
  requested: MsaaPreference,
  effective: number,
  pending: boolean,
): string {
  if (pending) {
    return `Applying ${msaaPreferenceLabel(requested)}…`;
  }
  if (msaaSamples(requested) !== effective) {
    return "Hardware fallback";
  }
  return "";
}

/**
 * SMAA has a boolean renderer setter with no fallback policy. A non-pending
 * mismatch is therefore an unexpected state, not a renderer fallback.
 */
export function formatSmaaStatus(
  requested: boolean,
  effective: boolean,
  pending: boolean,
): string {
  if (pending) return `Applying ${smaaStateLabel(requested)}…`;
  if (requested !== effective) return "State mismatch";
  return "";
}
