import { describe, expect, test } from "bun:test";
import { binaryWeight, decodeExpressions, sliderWeight } from "./expression-controls";

describe("avatar expression controls", () => {
  test("uses exactly the ordered authored names and custom metadata supplied by Rust", () => {
    const authored = [
      { index: 2, name: "happy", weight: 0, binary: false, custom: false },
      { index: 0, name: "customSmile", weight: 0.3, binary: false, custom: true },
    ];
    expect(decodeExpressions(authored)).toEqual(authored);
    expect(decodeExpressions(authored)?.map((item) => item.name)).toEqual(["happy", "customSmile"]);
    expect(decodeExpressions(authored)?.some((item) => item.name === "blink")).toBe(false);
    expect(decodeExpressions([{ ...authored[0], weight: Number.NaN }])).toBeNull();
  });

  test("slider produces bounded manual weights while binary controls send only zero or one", () => {
    const bounds = { x: 20, width: 100 };
    expect(sliderWeight(60, bounds)).toBe(0.4);
    expect(sliderWeight(-10, bounds)).toBe(0);
    expect(sliderWeight(200, bounds)).toBe(1);
    expect([binaryWeight(false), binaryWeight(true)]).toEqual([0, 1]);
  });
});
