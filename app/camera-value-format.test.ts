import { describe, expect, test } from "bun:test";
import { formatCompactDecimal, formatCompactFov, formatDegrees } from "./camera-value-format";

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

describe("compact camera degree idle formatting", () => {
  test.each([
    [15, "15°"],
    [7.5, "7.5°"],
    [0.1, "0.1°"],
    [90, "90°"],
  ])("formats %s as %s", (value, expected) => {
    expect(formatDegrees(value)).toBe(expected);
  });
});

describe("compact scalar camera formatting", () => {
  test.each([
    [0.05, "0.05"],
    [0.125, "0.13"],
    [0.4, "0.4"],
  ])("formats %s as %s", (value, expected) => {
    expect(formatCompactDecimal(value)).toBe(expected);
  });
});
