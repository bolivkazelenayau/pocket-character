import { describe, expect, test } from "bun:test";
import {
  PointerRepeat,
  REPEAT_DELAY_SECONDS,
  REPEAT_INTERVAL_SECONDS,
  type RepeatAction,
} from "./menu-repeat";

const repeatActions: RepeatAction[] = [
  "fov_decrement",
  "fov_increment",
  "distance_decrement",
  "distance_increment",
];

describe("PocketUI pointer repeat", () => {
  test("tap emits exactly one immediate action and no release duplicate", () => {
    const repeat = new PointerRepeat<object>();
    const target = {};

    expect(repeat.begin(target, "fov_increment", 0)).toBe("fov_increment");
    expect(repeat.tick(REPEAT_DELAY_SECONDS - 0.001)).toEqual([]);
    expect(repeat.end(target)).toBe(false);
    expect(repeat.tick(1)).toEqual([]);
  });

  test("hold emits the immediate action, delayed repeat, and interval repeat", () => {
    const repeat = new PointerRepeat<object>();
    const target = {};

    expect(repeat.begin(target, "distance_increment", 0)).toBe("distance_increment");
    expect(repeat.tick(REPEAT_DELAY_SECONDS - 0.001)).toEqual([]);
    expect(repeat.tick(REPEAT_DELAY_SECONDS)).toEqual(["distance_increment"]);
    expect(repeat.tick(REPEAT_DELAY_SECONDS + REPEAT_INTERVAL_SECONDS - 0.001)).toEqual([]);
    expect(repeat.tick(REPEAT_DELAY_SECONDS + REPEAT_INTERVAL_SECONDS)).toEqual([
      "distance_increment",
    ]);
  });

  test("release stops repeats immediately", () => {
    const repeat = new PointerRepeat<object>();
    const target = {};

    repeat.begin(target, "fov_decrement", 0);
    expect(repeat.tick(REPEAT_DELAY_SECONDS)).toEqual(["fov_decrement"]);
    expect(repeat.end(target)).toBe(false);
    expect(repeat.tick(2)).toEqual([]);
  });

  test("cancellation and focus loss stop repeats without release activation", () => {
    const repeat = new PointerRepeat<object>();
    const target = {};

    repeat.begin(target, "distance_decrement", 0);
    repeat.cancel();
    expect(repeat.tick(2)).toEqual([]);
    expect(repeat.end(target)).toBe(false);
  });

  test("dragging off stops repeats and re-entry does not resume the press", () => {
    const repeat = new PointerRepeat<object>();
    const target = {};
    const other = {};

    repeat.begin(target, "fov_increment", 0);
    repeat.move(other);
    expect(repeat.tick(2)).toEqual([]);
    repeat.move(target);
    expect(repeat.tick(3)).toEqual([]);
    expect(repeat.end(target)).toBe(false);
  });

  test("FOV and Distance buttons share the same repeat schedule", () => {
    for (const action of repeatActions) {
      const repeat = new PointerRepeat<object>();
      const target = {};

      expect(repeat.begin(target, action, 0)).toBe(action);
      expect(repeat.tick(REPEAT_DELAY_SECONDS)).toEqual([action]);
      expect(repeat.tick(REPEAT_DELAY_SECONDS + REPEAT_INTERVAL_SECONDS)).toEqual([action]);
      expect(repeat.end(target)).toBe(false);
    }
  });
});
