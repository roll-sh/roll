/** Milliseconds, or a string like `"500ms"`, `"30s"`, `"5m"`, `"2h"`, `"7d"`, `"1h 30m"`. */
export type Duration = number | string;

/** A point in time: a `Date`, unix milliseconds, or an RFC 3339 / ISO 8601 string. */
export type TimeInput = Date | number | string;

const UNITS: Record<string, number> = {
  ms: 1,
  msec: 1,
  msecs: 1,
  millisecond: 1,
  milliseconds: 1,
  s: 1_000,
  sec: 1_000,
  secs: 1_000,
  second: 1_000,
  seconds: 1_000,
  m: 60_000,
  min: 60_000,
  mins: 60_000,
  minute: 60_000,
  minutes: 60_000,
  h: 3_600_000,
  hr: 3_600_000,
  hrs: 3_600_000,
  hour: 3_600_000,
  hours: 3_600_000,
  d: 86_400_000,
  day: 86_400_000,
  days: 86_400_000,
  w: 604_800_000,
  week: 604_800_000,
  weeks: 604_800_000,
};

const TOKEN = /^\s*(\d+(?:\.\d+)?)\s*([a-zA-Z]+)\s*/;

/** Parse a {@link Duration} into whole milliseconds. Throws a `TypeError` when it can't. */
export function parseDuration(d: Duration): number {
  if (typeof d === "number") {
    if (!Number.isFinite(d) || d < 0) throw new TypeError(`bad duration ${d}: give non-negative milliseconds`);
    return Math.round(d);
  }
  if (typeof d !== "string") throw new TypeError(`bad duration ${String(d)}`);
  const s = d.trim();
  if (/^\d+(\.\d+)?$/.test(s)) return Math.round(Number(s));
  let rest = s;
  let total = 0;
  let tokens = 0;
  while (rest.length > 0) {
    const m = TOKEN.exec(rest);
    const unit = m ? UNITS[m[2]!.toLowerCase()] : undefined;
    if (!m || unit === undefined) throw new TypeError(`bad duration ${JSON.stringify(d)}: use e.g. "500ms", "30s", "5m", "2h", "7d"`);
    total += Number(m[1]) * unit;
    tokens++;
    rest = rest.slice(m[0].length);
  }
  if (tokens === 0) throw new TypeError(`bad duration ${JSON.stringify(d)}`);
  return Math.round(total);
}

/** Like {@link parseDuration}, passing `undefined` through. */
export function durationMs(d: Duration | undefined): number | undefined {
  return d === undefined ? undefined : parseDuration(d);
}

/** A {@link TimeInput} as unix milliseconds. */
export function toUnixMs(t: TimeInput): number {
  const ms = t instanceof Date ? t.getTime() : typeof t === "number" ? t : Date.parse(t);
  if (!Number.isFinite(ms)) throw new TypeError(`bad time ${String(t)}`);
  return Math.round(ms);
}
