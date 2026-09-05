export type RepeatAction =
  | "distance_decrement"
  | "distance_increment"
  | "fov_decrement"
  | "fov_increment";

export const REPEAT_DELAY_SECONDS = 0.3;
export const REPEAT_INTERVAL_SECONDS = 0.1;

/**
 * Pointer press state for a repeatable button. The first press is returned
 * immediately; due repeats are returned by tick(). Release activation is
 * suppressed for repeatable buttons so a held press cannot fire twice.
 */
export class PointerRepeat<T> {
  private pressedTarget: T | null = null;
  private repeatAction: RepeatAction | null = null;
  private suppressRelease = false;
  private nextRepeatAt = 0;

  begin(target: T | null, action: RepeatAction | null, now: number): RepeatAction | null {
    this.pressedTarget = target;
    this.repeatAction = action;
    this.suppressRelease = action !== null;
    this.nextRepeatAt = now + REPEAT_DELAY_SECONDS;
    return action;
  }

  /** Stop repeats when the pointer leaves the pressed target. */
  move(target: T | null): void {
    if (this.suppressRelease && target !== this.pressedTarget) {
      this.repeatAction = null;
    }
  }

  tick(now: number): RepeatAction[] {
    if (!this.repeatAction || !Number.isFinite(now)) return [];

    const due: RepeatAction[] = [];
    while (now >= this.nextRepeatAt) {
      due.push(this.repeatAction);
      this.nextRepeatAt += REPEAT_INTERVAL_SECONDS;
    }
    return due;
  }

  /**
   * Returns whether the current release should activate the ordinary button
   * path. Repeatable presses always return false; a drag-off also cancels the
   * release activation, matching the pointer model's latched target rule.
   */
  end(target: T | null): boolean {
    const activateOnRelease =
      !this.suppressRelease && this.pressedTarget !== null && target === this.pressedTarget;
    this.reset();
    return activateOnRelease;
  }

  /** Cancel the current press without allowing a later release to activate. */
  cancel(): void {
    this.pressedTarget = null;
    this.repeatAction = null;
    this.suppressRelease = true;
    this.nextRepeatAt = 0;
  }

  private reset(): void {
    this.pressedTarget = null;
    this.repeatAction = null;
    this.suppressRelease = false;
    this.nextRepeatAt = 0;
  }
}
