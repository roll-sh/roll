/**
 * The arithmetic behind the live merge rate and the landing pulse, kept pure so it can be tested:
 * a burst of thousands of landings must read as a steady rate, never as flashing.
 */

/** Drop timestamps (ms) older than the window; `times` is oldest first and is changed in place. */
export function prune(times: number[], now: number, windowS: number) {
  while (times.length && times[0] < now - windowS * 1000) times.shift();
  return times;
}

/**
 * Merges per second seen over the last window. `null` until something lands or a whole window has
 * been watched (the server's rate is the better estimate until then). Short watches are averaged
 * over at least `minSpanS`, so the first landing doesn't read as a spike.
 */
export function measuredRate(count: number, watchedS: number, windowS: number, minSpanS: number): number | null {
  if (!count && watchedS < windowS) return null;
  return count / Math.min(windowS, Math.max(minSpanS, watchedS));
}

/** Merges per second over a fixed window (the dashboard's sparkline). */
export function windowRate(count: number, windowS: number) {
  return windowS > 0 ? count / windowS : 0;
}

/** Whether a landing may pulse now: at most one pulse every `everyMs`. */
export function mayPulse(now: number, last: number, everyMs: number) {
  return now - last >= everyMs;
}

/** A batch (oldest first) on top of a newest-first feed, keeping `keep` rows. */
export function prependBatch<T>(feed: T[], batch: T[], keep: number): T[] {
  return [...[...batch].reverse(), ...feed].slice(0, keep);
}
