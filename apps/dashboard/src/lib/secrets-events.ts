import type { EventPage, EventsQuery, SecretsEvent, SecretsEventKind } from "@roll/secrets/types";

/** The feed's call (`SecretsApi.events`). */
export type EventsFetch = (q: EventsQuery) => Promise<EventPage>;
type Kinds = NonNullable<EventsQuery["kinds"]>;

/**
 * The Events tab's kind chips, as the `kinds=` each sends (docs/secrets.md, "Events"). Together they
 * cover every kind once; `secret.leaked` sits with the leaks, not with the value changes.
 */
export const FAMILIES = [
  { id: "secrets", label: "Secrets", kinds: ["secret.changed", "secret.deleted"] },
  { id: "rotations", label: "Rotations", kinds: ["rotation.*"] },
  { id: "requests", label: "Requests", kinds: ["request.*"] },
  { id: "access", label: "Access", kinds: ["access.*", "production.unprotected"] },
  { id: "environments", label: "Environments", kinds: ["env.*", "project.*"] },
  { id: "leaks", label: "Leaks & decoys", kinds: ["secret.leaked", "scan.finding", "decoy.used", "placeholder.*"] },
  { id: "syncs", label: "Syncs", kinds: ["sync.failed", "maker.unreachable"] },
] as const satisfies readonly { id: string; label: string; kinds: Kinds }[];
export type Family = (typeof FAMILIES)[number]["id"];

/** The family a kind's chip is. */
export const familyOf = (kind: SecretsEventKind): Family =>
  FAMILIES.find((f) => (f.kinds as readonly string[]).some((k) => k === kind || (k.endsWith(".*") && kind.startsWith(k.slice(0, -1)))))?.id ?? "secrets";

/** What the server filters by: no chip on is every kind (no `kinds=`). */
export type EventsFilter = { families: Family[]; environment?: string };

const query = (f: EventsFilter): EventsQuery => ({
  kinds: f.families.length ? FAMILIES.filter((x) => f.families.includes(x.id)).flatMap((x) => x.kinds) : undefined,
  environment: f.environment,
});

/** The most events one call answers (the API's `limit` ceiling). */
export const LIMIT = 500;
/** How many events an "older" load aims for before it stops widening its window. */
export const TARGET = 50;
const WIDEST = 64_000;

/**
 * The feed only reads forward (`after`), so going back reads a window of the audit chain's rows
 * before `hi` (an event's `seq` is its row's number): every event in `(lo, hi]`, newest first, and
 * the window's start (the next older load's `hi`). A window of at most `LIMIT` rows holds at most
 * `LIMIT` events, so one call answers it whole; a wider one that comes back cut short (its `next`
 * before `hi`) is narrowed and read again, so a dense stretch never leaves a gap.
 */
async function window(fetch: EventsFetch, f: EventsFilter, hi: number, width: number): Promise<{ events: SecretsEvent[]; lo: number; width: number }> {
  for (;;) {
    const lo = Math.max(0, hi - width);
    const page = await fetch({ ...query(f), after: lo, limit: LIMIT });
    if (page.next < hi && width > LIMIT) {
      width = Math.max(LIMIT, Math.floor(width / 8));
      continue;
    }
    return { events: page.events.filter((e) => e.seq <= hi).reverse(), lo, width };
  }
}

/** Events before `hi`, newest first, about `TARGET` of them: windows widen while they come back sparse. `lo` is where it stopped (0: the start). */
export async function older(fetch: EventsFetch, f: EventsFilter, hi: number, calls = 6): Promise<{ events: SecretsEvent[]; lo: number }> {
  const out: SecretsEvent[] = [];
  let width = LIMIT;
  while (hi > 0 && out.length < TARGET && calls-- > 0) {
    const w = await window(fetch, f, hi, width);
    out.push(...w.events);
    hi = w.lo;
    width = w.events.length < TARGET / 2 ? Math.min(WIDEST, w.width * 4) : w.width;
  }
  return { events: out, lo: hi };
}

/**
 * Opening the tab: the head (`latest=1`), then the events just before it. The head is the live
 * cursor: polls read after it, the first load reads up to it, so nothing falls between.
 */
export async function open(fetch: EventsFetch, f: EventsFilter): Promise<{ events: SecretsEvent[]; lo: number; cursor: number }> {
  const head = await fetch({ ...query(f), latest: true });
  const { events, lo } = await older(fetch, f, head.next);
  return { events, lo, cursor: head.next };
}

/** One live poll: events after `cursor`, newest first, and the cursor to ask with next (`next`, exactly as the API answers it). */
export async function poll(fetch: EventsFetch, f: EventsFilter, cursor: number): Promise<{ events: SecretsEvent[]; cursor: number; more: boolean }> {
  const page = await fetch({ ...query(f), after: cursor, limit: LIMIT });
  return { events: [...page.events].reverse(), cursor: page.next, more: page.events.length === LIMIT };
}

/** An event's key names (one `key`, `keys`, `deleted`, or by folder), never a value. */
export function eventKeys(e: SecretsEvent): { folder?: string; keys: string[] }[] {
  const d = e.data;
  const names = (v: unknown) => (Array.isArray(v) ? v.filter((x): x is string => typeof x === "string") : []);
  if (d.folders && typeof d.folders === "object" && !Array.isArray(d.folders)) return Object.entries(d.folders as Record<string, unknown>).map(([folder, keys]) => ({ folder, keys: names(keys) }));
  const keys = [...(e.key ? [e.key] : names(d.keys)), ...names(d.deleted).filter((k) => k !== e.key && !names(d.keys).includes(k))];
  const path = typeof d.path === "string" && d.path !== "/" ? d.path : undefined;
  return keys.length || path ? [{ folder: path, keys }] : [];
}

/** How it came about (`via`), or why (`reason`), or where a request ended up (`state`). */
export function eventDetail(e: SecretsEvent): string | undefined {
  for (const k of ["via", "reason", "state", "what", "repo"] as const) if (typeof e.data[k] === "string" && e.data[k]) return e.data[k] as string;
  return undefined;
}
