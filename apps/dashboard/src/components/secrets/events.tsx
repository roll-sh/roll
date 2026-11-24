import { useCallback, useEffect, useRef, useState } from "react";
import type { Environment, SecretsApi, SecretsEvent } from "@/lib/secrets-api";
import { eventDetail, eventKeys, FAMILIES, familyOf, older, open, poll, type EventsFetch, type EventsFilter, type Family } from "@/lib/secrets-events";
import { Select } from "../agents/ui/select";
import { NotifyMenu } from "./notify";
import { useSecrets } from "./secrets-page";
import { ActorMark, ago, Chip, ErrorNote, errorText, ghost, stamp, Teach } from "./ui";

/** How often the tab asks for new events while it's in view. */
export const POLL_MS = 5_000;
const ALL = "*";

/** The project's events feed (docs/secrets.md, "Events"): what changed, rotated, was asked for or leaked, newest first and live. */
export function EventsView() {
  const { api, envs, project } = useSecrets();
  return <EventsFeed load={api.events} envs={envs} project={project.name} notify={api} />;
}

/** Whether the tab is worth polling: its page is visible and the feed is on screen. */
function useInView(el: React.RefObject<HTMLElement | null>) {
  const [page, setPage] = useState(() => typeof document === "undefined" || document.visibilityState === "visible");
  const [onScreen, setOnScreen] = useState(true);
  useEffect(() => {
    const on = () => setPage(document.visibilityState === "visible");
    document.addEventListener("visibilitychange", on);
    return () => document.removeEventListener("visibilitychange", on);
  }, []);
  useEffect(() => {
    if (!el.current || typeof IntersectionObserver === "undefined") return;
    const io = new IntersectionObserver(([entry]) => setOnScreen(entry.isIntersecting));
    io.observe(el.current);
    return () => io.disconnect();
  }, [el]);
  return page && onScreen;
}

/**
 * The feed itself, on any `events` call (tests hand it a fake). Opening reads the head
 * (`latest=1`) and the events just before it; while in view it polls after the cursor, which is
 * always the last `next` the API answered. Filters go to the server (`kinds=`, `environment=`) and
 * start over. With `notify`, "Notify me" sits beside the filters, starting from them.
 */
export function EventsFeed({ load, envs, project, notify }: { load: EventsFetch; envs: Environment[]; project: string; notify?: Pick<SecretsApi, "notifications" | "notify" | "stopNotification"> }) {
  const [families, setFamilies] = useState<Family[]>([]);
  const [env, setEnv] = useState(ALL);
  const [rows, setRows] = useState<SecretsEvent[] | null>(null);
  const [fresh, setFresh] = useState<Set<number>>(new Set());
  const [lo, setLo] = useState(0);
  const [loadingOlder, setLoadingOlder] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const root = useRef<HTMLDivElement>(null);
  const inView = useInView(root);
  // The live cursor, and which filter it belongs to: a reply for an older filter is dropped.
  const cursor = useRef<number | null>(null);
  const gen = useRef(0);
  const filter: EventsFilter = { families, environment: env === ALL ? undefined : env };
  const sig = JSON.stringify(filter);

  useEffect(() => {
    const g = ++gen.current;
    cursor.current = null;
    setRows(null);
    setFresh(new Set());
    setError(null);
    open(load, filter).then(
      (r) => {
        if (g !== gen.current) return;
        cursor.current = r.cursor;
        setRows(r.events);
        setLo(r.lo);
      },
      (e) => g === gen.current && setError(errorText(e)),
    );
  }, [load, sig]);

  const tick = useCallback(async () => {
    const g = gen.current;
    for (let more = true; more && cursor.current !== null; ) {
      const r = await poll(load, filter, cursor.current);
      if (g !== gen.current) return;
      cursor.current = r.cursor;
      more = r.more;
      if (r.events.length) {
        setRows((old) => [...r.events, ...(old ?? [])]);
        setFresh(new Set(r.events.map((e) => e.seq)));
      }
    }
  }, [load, sig]);

  useEffect(() => {
    if (!inView || rows === null) return;
    let busy = false;
    const run = () => {
      if (busy) return;
      busy = true;
      tick().then(
        () => setError(null),
        (e) => setError(errorText(e)),
      ).finally(() => (busy = false));
    };
    const t = setInterval(run, POLL_MS);
    return () => clearInterval(t);
  }, [inView, rows === null, tick]);

  const loadOlder = () => {
    const g = gen.current;
    setLoadingOlder(true);
    older(load, filter, lo)
      .then(
        (r) => {
          if (g !== gen.current) return;
          setRows((old) => [...(old ?? []), ...r.events]);
          setLo(r.lo);
        },
        (e) => setError(errorText(e)),
      )
      .finally(() => setLoadingOlder(false));
  };

  const toggle = (f: Family) => setFamilies((on) => (on.includes(f) ? on.filter((x) => x !== f) : [...on, f]));

  return (
    <div ref={root} className="mx-auto max-w-5xl px-4 py-5">
      <div className="flex flex-wrap items-center gap-2">
        <Chip on={families.length === 0} onClick={() => setFamilies([])}>
          Everything
        </Chip>
        {FAMILIES.map((f) => (
          <Chip key={f.id} on={families.includes(f.id)} onClick={() => toggle(f.id)} title={f.kinds.join(", ")}>
            {f.label}
          </Chip>
        ))}
        <span className="ml-auto flex items-center gap-2">
          <span className="flex items-center gap-1.5 text-[11.5px] text-muted" title={inView ? `New events every ${POLL_MS / 1000} s` : "Paused while the tab is out of view"}>
            <span className={`size-1.5 rounded-full ${inView && rows ? "pulse-dot bg-accent" : "bg-faint"}`} />
            {inView ? "Live" : "Paused"}
          </span>
          <Select label="Environment" value={env} onChange={setEnv} options={[{ value: ALL, label: "All environments" }, ...envs.map((e) => ({ value: e.slug, label: e.name }))]} className="w-44" />
          {notify && <NotifyMenu api={notify} envs={envs} families={families} environment={filter.environment} />}
        </span>
      </div>
      <ErrorNote error={error} onDismiss={() => setError(null)} className="mt-3" />
      {rows && rows.length === 0 && lo === 0 ? (
        <Teach title="No events yet" cli={[`roll secrets events -p ${project} --follow`]}>
          Value changes, rotations, requests, access changes and leaks show here as they happen, with who did them. Names only: an event never holds a value.
        </Teach>
      ) : (
        <div className="mt-3 overflow-x-auto">
          <table className="w-full min-w-[760px] text-[12px]">
            <thead>
              <tr className="text-left text-[11px] text-subtle">
                <th className="py-1.5 pr-3 font-medium">When</th>
                <th className="py-1.5 pr-3 font-medium">Kind</th>
                <th className="py-1.5 pr-3 font-medium">Environment</th>
                <th className="py-1.5 pr-3 font-medium">Keys</th>
                <th className="py-1.5 pr-3 font-medium">Actor</th>
                <th className="py-1.5 font-medium">How</th>
              </tr>
            </thead>
            <tbody>
              {rows?.map((e) => (
                <EventRow key={e.seq} e={e} fresh={fresh.has(e.seq)} />
              ))}
            </tbody>
          </table>
          {rows && rows.length === 0 && <p className="py-4 text-center text-[12px] text-muted">Nothing recent matches.</p>}
          {rows && lo > 0 && (
            <button onClick={loadOlder} disabled={loadingOlder} className={`${ghost} mx-auto mt-3 flex`}>
              {loadingOlder ? "Loading…" : "Older events"}
            </button>
          )}
        </div>
      )}
    </div>
  );
}

function EventRow({ e, fresh }: { e: SecretsEvent; fresh: boolean }) {
  const groups = eventKeys(e);
  const detail = eventDetail(e);
  const actor = typeof e.data.actor === "string" ? e.data.actor : undefined;
  const danger = familyOf(e.kind) === "leaks" || e.kind.endsWith(".failed") || e.kind === "production.unprotected";
  return (
    <tr className={`border-t border-line align-top ${fresh ? "fade-in motion-reduce:animate-none" : ""}`}>
      <td className="py-1.5 pr-3 whitespace-nowrap text-muted" title={stamp(e.at)}>
        {ago(e.at)}
      </td>
      <td className={`py-1.5 pr-3 font-mono text-[11.5px] whitespace-nowrap ${danger ? "text-danger" : ""}`}>
        {e.kind}
      </td>
      <td className="py-1.5 pr-3 font-mono text-[11px] whitespace-nowrap text-muted">{e.environment ?? "—"}</td>
      <td className="max-w-72 py-1.5 pr-3 font-mono text-[11px] text-soft">
        {groups.length === 0
          ? <span className="text-faint">—</span>
          : groups.map((g) => (
              <span key={g.folder ?? "/"} className="block truncate" title={[g.folder, g.keys.join(" ")].filter(Boolean).join(" ")}>
                {g.folder && <span className="text-subtle">{g.folder} </span>}
                {g.keys.join(" ")}
              </span>
            ))}
      </td>
      <td className="max-w-44 py-1.5 pr-3 text-soft">{actor ? <ActorMark who={actor} /> : <span className="text-faint">—</span>}</td>
      <td className="py-1.5 text-muted">{detail ?? <span className="text-faint">—</span>}</td>
    </tr>
  );
}
