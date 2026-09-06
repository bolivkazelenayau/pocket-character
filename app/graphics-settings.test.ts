import { describe, expect, test } from "bun:test";
import {
  MSAA_OPTIONS,
  encodeMsaaRequest,
  encodeSmaaRequest,
  effectiveMsaaLabel,
  formatMsaaStatus,
  formatSmaaStatus,
  msaaSamples,
} from "./graphics-settings";

describe("PocketUI graphics request messages", () => {
  test("encodes every MSAA selector as an explicit sample request", () => {
    expect(MSAA_OPTIONS.map((option) => encodeMsaaRequest(option.preference))).toEqual([
      { t: "action", action: "request_msaa", value: 1 },
      { t: "action", action: "request_msaa", value: 2 },
      { t: "action", action: "request_msaa", value: 4 },
      { t: "action", action: "request_msaa", value: 8 },
    ]);
    expect(msaaSamples("4x")).toBe(4);
  });

  test("encodes SMAA independently from MSAA", () => {
    expect(encodeSmaaRequest(true)).toEqual({
      t: "action",
      action: "request_smaa",
      value: true,
    });
    expect(encodeSmaaRequest(false)).toEqual({
      t: "action",
      action: "request_smaa",
      value: false,
    });
  });

  test("keeps MSAA Off distinct from the renderer's 1× implementation detail", () => {
    expect(effectiveMsaaLabel(0)).toBe("Off");
    expect(effectiveMsaaLabel(1)).toBe("Off");
    expect(effectiveMsaaLabel(2)).toBe("2×");
    expect(effectiveMsaaLabel(4)).toBe("4×");
    expect(effectiveMsaaLabel(8)).toBe("8×");

    expect(formatMsaaStatus("off", 1, false)).toBe("");
    expect(formatMsaaStatus("8x", 4, false)).toBe("Hardware fallback");
    expect(formatMsaaStatus("2x", 1, false)).toBe("Hardware fallback");
    expect(formatMsaaStatus("8x", 4, true)).toBe("Applying 8×…");
  });

  test("does not call a non-pending SMAA mismatch a renderer fallback", () => {
    expect(formatSmaaStatus(true, false, false)).toBe("State mismatch");
    expect(formatSmaaStatus(true, false, true)).toBe("Applying On…");
    expect(formatSmaaStatus(true, true, false)).toBe("");
  });
});
