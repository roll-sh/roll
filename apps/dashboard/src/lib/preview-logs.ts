/** A log line's level, read as roll apps reads one (`apps-control/src/logs.rs` `level_of`), so a
 * preview's log and a roll apps service's say the same lines are errors. */
export type LogLevel = "trace" | "debug" | "info" | "warn" | "error" | "fatal";

const NAMES: Record<string, LogLevel> = {
  trace: "trace",
  verbose: "trace",
  debug: "debug",
  dbg: "debug",
  info: "info",
  information: "info",
  notice: "info",
  log: "info",
  warn: "warn",
  warning: "warn",
  error: "error",
  err: "error",
  fatal: "fatal",
  critical: "fatal",
  crit: "fatal",
  panic: "fatal",
  alert: "fatal",
  emergency: "fatal",
};

const named = (s: string): LogLevel | null => NAMES[s.toLowerCase()] ?? null;

/** pino's and bunyan's numbers (10 trace … 60 fatal). */
function numbered(n: number): LogLevel {
  if (n <= 10) return "trace";
  if (n <= 20) return "debug";
  if (n <= 30) return "info";
  if (n <= 40) return "warn";
  if (n <= 50) return "error";
  return "fatal";
}

/** What a line says its level is: a JSON object's `level`, `severity`, `lvl` or `log.level` (a
 * name or pino's number), logfmt's `level=`/`lvl=`, or an upper-case level word among its first
 * three (a bracketed one in any case: nginx's `[error]`). Prose that mentions an error says none. */
export function logLevel(text: string): LogLevel | null {
  const t = text.trimStart();
  if (t.startsWith("{")) {
    let o: unknown;
    try {
      o = JSON.parse(t);
    } catch {
      return null;
    }
    if (!o || typeof o !== "object" || Array.isArray(o)) return null;
    const r = o as Record<string, unknown>;
    const log = r.log && typeof r.log === "object" ? (r.log as Record<string, unknown>).level : undefined;
    const field = r.level ?? r.severity ?? r.lvl ?? log;
    if (typeof field === "string") return named(field);
    if (typeof field === "number") return numbered(field);
    return null;
  }
  const words = t.split(/\s+/).filter(Boolean);
  for (const w of words.slice(0, 12)) {
    const v = w.startsWith("level=") ? w.slice(6) : w.startsWith("lvl=") ? w.slice(4) : null;
    if (v !== null) return named(v.replace(/^"|"$/g, ""));
  }
  for (const raw of words.slice(0, 3)) {
    const bracketed = raw.startsWith("[") && raw.replace(/:+$/, "").endsWith("]");
    const w = raw.replace(/^[[\]():|]+|[[\]():|]+$/g, "");
    if (bracketed || (w.length >= 3 && /^[A-Z]+$/.test(w))) {
      const l = named(w);
      if (l) return l;
    }
  }
  return null;
}

/** Whether a level is a problem worth marking: a warning, or worse. */
export const isProblem = (l: LogLevel | null): l is "warn" | "error" | "fatal" => l === "warn" || l === "error" || l === "fatal";

/** A filter typed over a log: `/regex/` (with `i`), else text matched case-blind. `null` when it
 * can't be read (a broken regex), so the box says so instead of showing nothing. */
export function logFilter(q: string): ((line: string) => boolean) | null {
  const t = q.trim();
  if (!t) return () => true;
  const re = /^\/(.+)\/(i?)$/.exec(t);
  if (re) {
    try {
      const r = new RegExp(re[1], re[2]);
      return (l) => r.test(l);
    } catch {
      return null;
    }
  }
  const lower = t.toLowerCase();
  return (l) => l.toLowerCase().includes(lower);
}
