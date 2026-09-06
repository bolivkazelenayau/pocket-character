import { describe, expect, test } from "bun:test";
import { formatCompactFov } from "./camera-value-format";

describe("compact FOV idle formatting", () => {
  test.each([
    [40.255, "40.26"],
    [40.254, "40.25"],
    [40.2, "40.2"],
    [40, "40"],
  ])("formats %s as %s", (value, expected) => {
    expect(formatCompactFov(value)).toBe(expected);
  });
});
