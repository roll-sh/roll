import { ArrowRight, CloudDownload, X } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { ApiError, formatBytes } from "@/lib/roll-api";
import { esJson, formatCount, indexPath, searchApi, type SearchCluster } from "@/lib/search-api";
import {
  aliasBody,
  aliasOffer,
  defaultDest,
  destProblem,
  displayHost,
  droppedByReason,
  isSaas,
  LOST,
  mergeStarted,
  migrateApi,
  nextSteps,
  migrateRefusal,
  notMappedCount,
  notMappedGroups,
  partial,
  plainHttpWarning,
  queuedPicks,
  QUEUED_RETRY_MS,
  queueRetryDelay,
  replaceRow,
  retryableStart,
  shouldStartQueued,
  sinceProblem,
  SOURCES,
  sourceFrom,
  sourceProblem,
  taskPoll,
  taskProgress,
  type AuthKind,
  type MigrateInspect,
  type MigrateRequest,
  type MigrateStarted,
  type SaasPlan,
  type SourceForm,
  type SourceKind,
  type TaskProgress,
} from "@/lib/search-migrate";
import { Field, Segmented } from "../redis/shared";
import { btn, ErrorNote, input, Panel, primary } from "../ui";

/** How often running copies are checked. */
const POLL_MS = 1500;

const AUTH: { label: string; value: AuthKind }[] = [
  { label: "API key", value: "api_key" },
  { label: "User & password", value: "basic" },
  { label: "None", value: "none" },
];

interface Pick {
  on: boolean;
  dest: string;
}

type AliasState = { alias: string; busy?: boolean; done?: boolean; error?: string };
/** What a SaaS copy's row holds for its next step: the key again (the panel drops it once a
 * copy starts), a catch-up's timestamp attribute, the cutover's alias. */
type NextState = { key: string; since: string; alias: string; busy?: boolean; error?: string };

const KIND_WORDS: Record<string, string> = { setting: "Settings", field: "Fields", replica: "Replicas", synonym: "Synonyms", rule: "Rules" };

/** What an Algolia, Typesense or Meilisearch index becomes, and what didn't map. */
function PlanSummary({ plan, open }: { plan: SaasPlan; open?: boolean }) {
  const groups = notMappedGroups(plan);
  const n = notMappedCount(plan);
  return (
    <div className="space-y-1.5 text-[12px]">
      <p className="text-muted">
        <span className="font-mono">{plan.id_field}</span> as <span className="font-mono">_id</span> · {plan.mappings} mapped fields · {plan.synonyms} synonyms · {plan.rules} rules · search template{" "}
        <span className="font-mono text-fg">{plan.search_template}</span>
        {plan.sort_templates && Object.keys(plan.sort_templates).length > 0 && ` · ${Object.keys(plan.sort_templates).length} replica sorts`}
      </p>
      {(n > 0 || plan.notes.length > 0) && (
        <details className="group" open={open}>
          <summary className="cursor-pointer text-subtle transition hover:text-fg">
            {n === 0 ? "Everything mapped" : `${n} ${n === 1 ? "thing" : "things"} didn't map`}
            {plan.notes.length > 0 && ` · ${plan.notes.length} ${plan.notes.length === 1 ? "note" : "notes"}`}
          </summary>
          <div className="mt-2 space-y-2">
            {groups.map((g) => (
              <div key={g.kind}>
                <p className="text-[11px] text-subtle">{KIND_WORDS[g.kind]}</p>
                <ul className="space-y-1">
                  {g.items.map((m, i) => (
                    <li key={`${m.name}-${i}`}>
                      <span className="break-all font-mono text-[11px] text-fg">{m.name}</span> <span className="text-muted">— {m.reason}</span>
                    </li>
                  ))}
                </ul>
              </div>
            ))}
            {plan.not_mapped_more > 0 && <p className="text-subtle">and {formatCount(plan.not_mapped_more)} more.</p>}
            {plan.notes.length > 0 && (
              <ul className="list-disc space-y-1 pl-4 text-muted">
                {plan.notes.map((note) => (
                  <li key={note}>{note}</li>
                ))}
              </ul>
            )}
          </div>
        </details>
      )}
    </div>
  );
}

/**
 * Migrate indices from Elastic Cloud, a self-hosted Elasticsearch or OpenSearch, Algolia,
 * Typesense or Meilisearch: the source's URL (or Algolia's application id) and credentials go to
 * roll (which reads the source, never the browser), its indices are listed with their size, the
 * picked ones are created on roll and copied by reindex tasks this panel follows, with cancel;
 * once a copy is done an alias can carry the source's index name. Algolia, Typesense and
 * Meilisearch indices are mapped onto roll's (mappings, synonyms, query rules, a search
 * template), and what didn't map is shown before the copy starts.
 */
export function MigratePanel({ cluster, existing, onClose, onChanged }: { cluster: SearchCluster; existing: string[]; onClose: () => void; onChanged: () => void }) {
  const [form, setForm] = useState<SourceForm>({ kind: "elasticsearch", host: "", appId: "", auth: "api_key", apiKey: "", username: "", password: "" });
  /** Algolia, Typesense, Meilisearch: what the picked indices become (a `dry_run`), shown
   * before the copy starts. */
  const [plans, setPlans] = useState<MigrateStarted[] | null>(null);
  const kind: SourceKind = form.kind ?? "elasticsearch";
  const saas = isSaas(kind);
  const meta = SOURCES.find((x) => x.value === kind) ?? SOURCES[0];
  const [remote, setRemote] = useState<MigrateInspect | null>(null);
  const [picks, setPicks] = useState<Record<string, Pick>>({});
  const [rps, setRps] = useState("");
  const [size, setSize] = useState("");
  const [busy, setBusy] = useState<"inspect" | "start" | "review" | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [started, setStarted] = useState<MigrateStarted[] | null>(null);
  const [progress, setProgress] = useState<Record<string, TaskProgress>>({});
  const [aliases, setAliases] = useState<Record<string, AliasState>>({});
  const [next, setNext] = useState<Record<string, NextState>>({});
  /** Partial copies being deleted (`true`) or deleted (`"done"`), or why the delete failed. */
  const [removed, setRemoved] = useState<Record<string, true | "done" | string>>({});
  const taken = useMemo(() => new Set(existing), [existing]);
  const reported = useRef(false);
  /** The remote and options while copies wait for a free slot (in memory only, never in the
   * page; dropped once nothing waits, or the panel closes). */
  const waiting = useRef<Omit<MigrateRequest, "indices"> | null>(null);
  const starting = useRef(false);
  /** Bumped `QUEUED_RETRY_MS` after an attempt that left copies waiting, or after a backoff
   * when an attempt failed: the only thing that starts the next attempt, besides copies ending. */
  const [nudge, setNudge] = useState(0);
  /** Attempts in a row that failed (429 or 5xx; the pause before the next grows). */
  const failures = useRef(0);
  /** The one pending ask-again timer: replaced (never added to) by each attempt, so a copy ending
   * mid-wait can't leave a second chain of asks running, nor cut a failure's backoff short. */
  const retry = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const askAgain = (ms: number) => {
    clearTimeout(retry.current);
    retry.current = setTimeout(() => setNudge((n) => n + 1), ms);
  };
  useEffect(
    () => () => {
      waiting.current = null;
      clearTimeout(retry.current);
    },
    [],
  );

  const set = (patch: Partial<SourceForm>) => setForm((f) => ({ ...f, ...patch }));
  const problem = sourceProblem(form);

  const inspect = async () => {
    setBusy("inspect");
    setError(null);
    try {
      const r = await migrateApi.inspect(cluster.id, sourceFrom(form));
      setRemote(r);
      setPlans(null);
      setPicks(Object.fromEntries(r.indices.map((i) => [i.index, { on: false, dest: defaultDest(i.index, taken) }])));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(null);
    }
  };

  const chosen = remote?.indices.filter((i) => picks[i.index]?.on) ?? [];
  const dests = chosen.map((i) => picks[i.index].dest.trim());
  const problems = Object.fromEntries(
    chosen.map((i, n) => [i.index, destProblem(dests[n], taken, dests.filter((_, m) => m !== n))]),
  );
  const blocked = chosen.length === 0 || Object.values(problems).some(Boolean);
  const rpsNum = rps.trim() ? Number(rps) : undefined;
  const sizeNum = size.trim() ? Number(size) : undefined;
  const optionsProblem =
    rpsNum !== undefined && !(rpsNum > 0) ? "Documents per second is a positive number." : sizeNum !== undefined && !(Number.isInteger(sizeNum) && sizeNum >= 1 && sizeNum <= 10000) ? "The batch size is 1 to 10,000 documents." : null;

  const picked = () => chosen.map((i) => ({ index: i.index, dest: picks[i.index].dest.trim() }));

  const review = async () => {
    setBusy("review");
    setError(null);
    try {
      const r = await migrateApi.start(cluster.id, { source: sourceFrom(form), indices: picked(), size: sizeNum, dry_run: true });
      setPlans(r.indices);
    } catch (e) {
      setError(migrateRefusal(e instanceof ApiError ? e.status : undefined, e instanceof Error ? e.message : String(e)));
    } finally {
      setBusy(null);
    }
  };

  const start = async () => {
    setBusy("start");
    setError(null);
    try {
      const options = { source: sourceFrom(form), requests_per_second: rpsNum, size: sizeNum };
      const r = await migrateApi.start(cluster.id, { ...options, indices: picked() });
      setStarted(r.indices);
      // The remote's key leaves the page; it's kept in memory only while copies wait.
      waiting.current = r.indices.some((x) => x.queued) ? options : null;
      setForm((f) => ({ ...f, apiKey: "", password: "" }));
      const offers: Record<string, AliasState> = {};
      for (const s of r.indices) {
        const a = aliasOffer(s.index, s.dest, new Set([...taken, ...r.indices.map((x) => x.dest)]));
        if (a && s.task) offers[s.index] = { alias: a };
      }
      setAliases(offers);
      onChanged();
    } catch (e) {
      setError(migrateRefusal(e instanceof ApiError ? e.status : undefined, e instanceof Error ? e.message : String(e)));
    } finally {
      setBusy(null);
    }
  };

  // Follow the tasks until each has ended.
  const running = (started ?? []).filter((s) => s.task && (progress[s.task]?.state ?? "running") === "running");
  const runningKey = running.map((s) => s.task).join(",");
  useEffect(() => {
    if (!runningKey) return;
    let stop = false;
    const tick = async () => {
      for (const task of runningKey.split(",")) {
        try {
          const r = await searchApi.request(cluster.id, "GET", `/_tasks/${encodeURIComponent(task)}`);
          const poll = taskPoll(r.status, r.body);
          if (stop || poll.kind === "retry") continue;
          setProgress((p) => ({ ...p, [task]: poll.kind === "lost" ? LOST : taskProgress(poll.body) }));
        } catch {
          // A blip: the next tick asks again.
        }
      }
    };
    tick();
    const t = setInterval(tick, POLL_MS);
    return () => {
      stop = true;
      clearInterval(t);
    };
  }, [cluster.id, runningKey]);

  // Waiting copies start as earlier ones end (roll creates a copy only when it can start it).
  const states = Object.fromEntries(Object.entries(progress).map(([t, p]) => [t, p.state]));
  const ready = started !== null && shouldStartQueued(started, states);
  // Read through refs: an answer replaces `started`, and that alone mustn't start the next
  // attempt (each attempt calls the remote and keeps the cluster's migration slot busy).
  const startedRef = useRef(started);
  startedRef.current = started;
  const onChangedRef = useRef(onChanged);
  onChangedRef.current = onChanged;
  useEffect(() => {
    const rows = startedRef.current;
    const options = waiting.current;
    if (!ready || !rows || !options || starting.current) return;
    starting.current = true;
    clearTimeout(retry.current);
    (async () => {
      try {
        const r = await migrateApi.start(cluster.id, { ...options, indices: queuedPicks(rows) });
        setStarted((rows) => {
          const next = rows ? mergeStarted(rows, r.indices) : rows;
          if (next && !next.some((x) => x.queued)) waiting.current = null;
          return next;
        });
        // Roll answered: errors aren't piling up, and a copy still waiting keeps its turn by asking
        // again soon.
        failures.current = 0;
        if (r.indices.some((x) => x.queued)) askAgain(QUEUED_RETRY_MS);
        onChangedRef.current();
      } catch (e) {
        const status = e instanceof ApiError ? e.status : undefined;
        if (retryableStart(status)) {
          // Busy or a deadline (roll answered 429 or 5xx): ask again later rather than stall.
          askAgain(queueRetryDelay(failures.current++));
        } else {
          waiting.current = null;
          setError(e instanceof Error ? e.message : String(e));
        }
      } finally {
        starting.current = false;
      }
    })();
    // `nudge` asks again after an attempt that left copies waiting or failed.
  }, [ready, cluster.id, nudge]);

  const allEnded = started !== null && running.length === 0 && !started.some((x) => x.queued);
  useEffect(() => {
    if (allEnded && !reported.current) {
      reported.current = true;
      onChanged();
    }
  }, [allEnded, onChanged]);

  const cancel = async (task: string) => {
    try {
      await esJson(cluster.id, "POST", `/_tasks/${encodeURIComponent(task)}/_cancel`);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  const pointAlias = async (index: string, dest: string) => {
    const a = aliases[index];
    if (!a) return;
    setAliases((x) => ({ ...x, [index]: { ...a, busy: true, error: undefined } }));
    try {
      await esJson(cluster.id, "POST", "/_aliases", aliasBody(a.alias.trim(), dest));
      setAliases((x) => ({ ...x, [index]: { ...a, busy: false, done: true } }));
      onChanged();
    } catch (e) {
      setAliases((x) => ({ ...x, [index]: { ...a, busy: false, error: e instanceof Error ? e.message : String(e) } }));
    }
  };

  const removePartial = async (dest: string) => {
    setRemoved((x) => ({ ...x, [dest]: true }));
    try {
      await esJson(cluster.id, "DELETE", indexPath(dest));
      setRemoved((x) => ({ ...x, [dest]: "done" }));
      onChanged();
    } catch (e) {
      setRemoved((x) => ({ ...x, [dest]: e instanceof Error ? e.message : String(e) }));
    }
  };

  const nextOf = (s: MigrateStarted): NextState => next[s.index] ?? { key: "", since: "", alias: s.index };
  const setNextOf = (s: MigrateStarted, patch: Partial<NextState>) => setNext((x) => ({ ...x, [s.index]: { ...nextOf(s), ...x[s.index], ...patch } }));

  /** Resumes a stopped copy, catches a done one up, or cuts over to it. */
  const runStep = async (s: MigrateStarted, step: "copy" | "catch_up" | "cutover") => {
    const n = nextOf(s);
    setNextOf(s, { busy: true, error: undefined });
    try {
      const source = sourceFrom({ ...form, apiKey: step === "cutover" ? "" : n.key || form.apiKey });
      const r = await migrateApi.start(cluster.id, {
        source,
        indices: [{ index: s.index, dest: s.dest, ...(step === "cutover" ? { alias: n.alias.trim() || s.index } : {}) }],
        step,
        ...(step === "catch_up" && n.since.trim() ? { since_attribute: n.since.trim() } : {}),
        size: sizeNum,
      });
      const row = r.indices[0];
      if (row?.error) setNextOf(s, { busy: false, error: row.error });
      else {
        setStarted((rows) => (rows && row ? replaceRow(rows, row) : rows));
        // The key leaves the page again.
        setNextOf(s, { busy: false, key: "" });
        onChanged();
      }
    } catch (e) {
      setNextOf(s, { busy: false, error: migrateRefusal(e instanceof ApiError ? e.status : undefined, e instanceof Error ? e.message : String(e)) });
    }
  };

  const restart = () => {
    setStarted(null);
    setProgress({});
    setAliases({});
    setRemoved({});
    setRemote(null);
    setPlans(null);
    reported.current = false;
  };

  return (
    <Panel
      title="Migrate into roll"
      className="rise"
      aside={
        <button onClick={onClose} className={btn} aria-label="Close migration">
          <X className="size-3.5" />
        </button>
      }
    >
      <div className="space-y-4 px-5 py-4">
        {!remote && (
          <>
            <p className="text-[13px] text-muted">
              Copy indices into <span className="font-mono text-fg">{cluster.name}</span>. roll&apos;s servers read the source page by page; your browser never connects to it.
              {saas && " Its settings, synonyms and rules are mapped onto roll's, and you see what didn't map before the copy starts."}
            </p>
            <Field label="Source">
              <div className="overflow-x-auto">
                <Segmented value={kind} options={SOURCES.map((x) => ({ label: x.label, value: x.value }))} onChange={(k) => set({ kind: k, auth: isSaas(k) ? "api_key" : form.auth })} />
              </div>
            </Field>
            {kind === "algolia" ? (
              <Field label="Application ID">
                <input value={form.appId ?? ""} onChange={(e) => set({ appId: e.target.value })} placeholder="YourAppID" className={`${input} w-full font-mono`} spellCheck={false} autoComplete="off" />
              </Field>
            ) : (
              <div className={`grid gap-3 ${saas ? "" : "sm:grid-cols-[minmax(0,1fr)_auto]"}`}>
                <Field label={saas ? `${meta.label} URL` : "Cluster URL"} hint={form.host.trim() ? `roll calls ${displayHost(form.host)}` : undefined}>
                  <input value={form.host} onChange={(e) => set({ host: e.target.value })} placeholder={meta.placeholder} className={`${input} w-full font-mono`} spellCheck={false} autoComplete="off" />
                </Field>
                {!saas && (
                  <Field label="Sign in with">
                    <Segmented value={form.auth} options={AUTH} onChange={(auth) => set({ auth })} />
                  </Field>
                )}
              </div>
            )}
            {plainHttpWarning(form) && <p className="text-[12px] text-danger">{plainHttpWarning(form)}</p>}
            {form.auth === "api_key" && (
              <div className="space-y-1">
                <Field label="API key">
                  <input type="password" value={form.apiKey} onChange={(e) => set({ apiKey: e.target.value })} className={`${input} w-full font-mono`} autoComplete="off" spellCheck={false} />
                </Field>
                <p className="text-[11px] text-subtle">{meta.keyHint}</p>
              </div>
            )}
            {!saas && form.auth === "basic" && (
              <div className="grid gap-3 sm:grid-cols-2">
                <Field label="Username">
                  <input value={form.username} onChange={(e) => set({ username: e.target.value })} className={`${input} w-full font-mono`} autoComplete="off" spellCheck={false} />
                </Field>
                <Field label="Password">
                  <input type="password" value={form.password} onChange={(e) => set({ password: e.target.value })} className={`${input} w-full font-mono`} autoComplete="off" />
                </Field>
              </div>
            )}
            <div className="flex flex-wrap items-center gap-3">
              <button onClick={inspect} disabled={!!problem || busy !== null} className={primary}>
                <CloudDownload className="size-3.5" strokeWidth={1.5} /> List indices
              </button>
              <span className={`text-[12px] ${problem && form.host.trim() ? "text-danger" : "text-subtle"}`}>
                {busy === "inspect" ? (
                  <span className="shimmer">Reading {saas ? meta.label : "the remote cluster"}…</span>
                ) : problem && (form.host.trim() || form.appId?.trim()) ? (
                  problem
                ) : (
                  "Sent to roll for this migration only: never stored, never shown again."
                )}
              </span>
            </div>
          </>
        )}

        {remote && !started && plans && (
          <div className="space-y-3">
            <p className="text-[13px] text-muted">What the picked indices become on roll. Nothing is made until you start.</p>
            {plans.map((p) => (
              <div key={p.index} className="space-y-2 rounded-lg border border-line px-4 py-3">
                <p className="flex min-w-0 items-center gap-2 font-mono text-[12px]">
                  <span className="truncate text-muted">{p.index}</span>
                  <ArrowRight className="size-3.5 shrink-0 text-subtle" strokeWidth={1.5} />
                  <span className="truncate">{p.dest}</span>
                </p>
                {p.error && <p className="break-words font-mono text-[11px] text-danger">{p.error}</p>}
                {p.restart && <p className="text-[12px] text-muted">An earlier copy of it is on roll: starting copies into it again (documents overwritten by id).</p>}
                {p.plan && <PlanSummary plan={p.plan} open />}
              </div>
            ))}
            <div className="flex flex-wrap items-center justify-between gap-3 border-t border-line pt-3">
              <button onClick={() => setPlans(null)} className={btn}>
                Back
              </button>
              <button onClick={start} disabled={busy !== null || plans.every((p) => p.error)} className={primary}>
                {busy === "start" ? <span className="shimmer">Starting…</span> : `Migrate ${chosen.length} ${chosen.length === 1 ? "index" : "indices"}`}
              </button>
            </div>
          </div>
        )}

        {remote && !started && !plans && (
          <>
            <div className="flex flex-wrap items-center justify-between gap-2">
              <p className="min-w-0 truncate font-mono text-[12px] text-muted">
                {remote.remote.distribution} {remote.remote.version}
                {remote.remote.cluster_name && ` · ${remote.remote.cluster_name}`} · {remote.remote.host}
              </p>
              <button onClick={() => setRemote(null)} className={btn}>
                Change
              </button>
            </div>
            {remote.indices.length === 0 ? (
              <p className="text-[13px] text-muted">{saas ? "The source has no indices to copy." : "The remote cluster has no open indices to copy."}</p>
            ) : (
              <div className="overflow-x-auto rounded-lg border border-line">
                <table className="w-full text-[12px]">
                  <thead className="text-left text-[11px] text-subtle">
                    <tr className="border-b border-line">
                      <th className="w-8 px-3 py-2 font-normal">
                        <input
                          type="checkbox"
                          aria-label="Pick every index"
                          checked={chosen.length === remote.indices.length}
                          onChange={(e) => setPicks((p) => Object.fromEntries(Object.entries(p).map(([k, v]) => [k, { ...v, on: e.target.checked }])))}
                          className="accent-accent"
                        />
                      </th>
                      <th className="px-3 py-2 font-normal">Index</th>
                      <th className="px-3 py-2 text-right font-normal">Docs</th>
                      <th className="px-3 py-2 text-right font-normal">Size</th>
                      <th className="hidden px-3 py-2 font-normal sm:table-cell">Aliases</th>
                      <th className="px-3 py-2 font-normal">Copy as</th>
                    </tr>
                  </thead>
                  <tbody>
                    {remote.indices.map((i) => {
                      const p = picks[i.index];
                      const why = problems[i.index];
                      return (
                        <tr key={i.index} className={`border-b border-line transition last:border-0 ${p?.on ? "bg-panel-2/60" : ""}`}>
                          <td className="px-3 py-1.5">
                            <input
                              type="checkbox"
                              aria-label={`Pick ${i.index}`}
                              checked={!!p?.on}
                              onChange={(e) => setPicks((x) => ({ ...x, [i.index]: { ...x[i.index], on: e.target.checked } }))}
                              className="accent-accent"
                            />
                          </td>
                          <td className="max-w-56 truncate px-3 py-1.5 font-mono" title={i.index}>
                            {i.index}
                          </td>
                          <td className="px-3 py-1.5 text-right font-mono tabular-nums">{formatCount(i.docs)}</td>
                          <td className="px-3 py-1.5 text-right font-mono tabular-nums">{formatBytes(i.size_bytes)}</td>
                          <td className="hidden max-w-48 truncate px-3 py-1.5 font-mono text-subtle sm:table-cell" title={i.aliases_on_roll.length ? "Aliases roll has already aren't copied" : undefined}>
                            {i.aliases.length === 0
                              ? i.replica_of
                                ? `replica of ${i.replica_of}`
                                : "—"
                              : i.aliases.map((a, n) => (
                                  <span key={a}>
                                    {n > 0 && ", "}
                                    {i.aliases_on_roll.includes(a) ? <span className="line-through">{a}</span> : a}
                                  </span>
                                ))}
                            {(i.aliases_more ?? 0) > 0 && <span> +{formatCount(i.aliases_more ?? 0)}</span>}
                          </td>
                          <td className="px-3 py-1.5">
                            <input
                              value={p?.dest ?? ""}
                              onChange={(e) => setPicks((x) => ({ ...x, [i.index]: { ...x[i.index], dest: e.target.value } }))}
                              aria-label={`Name of ${i.index}'s copy on roll`}
                              aria-invalid={!!why}
                              className={`${input} w-full min-w-28 py-1 font-mono text-[12px] ${why ? "border-danger" : ""}`}
                              spellCheck={false}
                            />
                            {why && <span className="mt-1 block text-[11px] text-danger">{why}</span>}
                          </td>
                        </tr>
                      );
                    })}
                  </tbody>
                </table>
              </div>
            )}
            {chosen.some((i) => i.aliases_on_roll.length > 0) && (
              <p className="text-[12px] text-subtle">
                Aliases roll has already (struck through) aren&apos;t copied: the copy never joins what they point at.
              </p>
            )}
            {(remote.more_indices ?? 0) > 0 && (
              <p className="text-[12px] text-subtle">
                {formatCount(remote.more_indices ?? 0)} more indices on the remote aren&apos;t listed: roll lists the first 10,000.
              </p>
            )}
            {remote.data_streams.length > 0 && (
              <p className="text-[12px] text-subtle">
                {remote.data_streams.length} data {remote.data_streams.length === 1 ? "stream isn't" : "streams aren't"} listed (
                <span className="font-mono">{remote.data_streams.slice(0, 3).join(", ")}</span>
                {remote.data_streams.length > 3 && "…"}): copy them with a reindex into a data stream, <span className="font-mono">op_type: create</span>.
              </p>
            )}
            <details className="group text-[12px]">
              <summary className="cursor-pointer text-subtle transition hover:text-fg">Throttle and batch size</summary>
              <div className="mt-3 grid gap-3 sm:grid-cols-2">
                <Field label="Documents per second, per index" hint="Empty: as fast as the remote answers.">
                  <input value={rps} onChange={(e) => setRps(e.target.value)} inputMode="decimal" placeholder="unthrottled" className={`${input} w-full font-mono`} />
                </Field>
                <Field label="Batch size" hint="Documents per page read; lower it for large documents.">
                  <input value={size} onChange={(e) => setSize(e.target.value)} inputMode="numeric" placeholder="1000" className={`${input} w-full font-mono`} />
                </Field>
              </div>
            </details>
            <div className="flex flex-wrap items-center justify-between gap-3 border-t border-line pt-3">
              <span className="font-mono text-[12px] text-muted">
                {chosen.length} {chosen.length === 1 ? "index" : "indices"} · {formatCount(chosen.reduce((n, i) => n + (i.docs ?? 0), 0))} docs ·{" "}
                {formatBytes(chosen.reduce((n, i) => n + (i.size_bytes ?? 0), 0))}
              </span>
              <span className="flex items-center gap-3">
                {optionsProblem && <span className="text-[12px] text-danger">{optionsProblem}</span>}
                {saas ? (
                  <button onClick={review} disabled={blocked || !!optionsProblem || busy !== null} className={primary}>
                    {busy === "review" ? <span className="shimmer">Reading definitions…</span> : "Review mapping"}
                  </button>
                ) : (
                  <button onClick={start} disabled={blocked || !!optionsProblem || busy !== null} className={primary}>
                    {busy === "start" ? <span className="shimmer">Starting…</span> : chosen.length === 0 ? "Migrate" : `Migrate ${chosen.length} ${chosen.length === 1 ? "index" : "indices"}`}
                  </button>
                )}
              </span>
            </div>
          </>
        )}

        {started && (
          <div className="space-y-3" aria-live="polite">
            {started.map((s) => {
              const pr = s.task ? progress[s.task] : undefined;
              // A row with no task (a cutover's) is done when it answered without an error.
              const state = s.queued ? "queued" : s.error ? "failed" : !s.task ? "done" : (pr?.state ?? "running");
              const after = nextSteps(s, state === "queued" ? undefined : state);
              const n = nextOf(s);
              const width = state === "done" ? 100 : pr?.fraction != null ? Math.round(pr.fraction * 100) : 0;
              const a = aliases[s.index];
              const groups = droppedByReason(s.dropped ?? []);
              return (
                <div key={s.index} className="rise space-y-2 rounded-lg border border-line px-4 py-3">
                  <div className="flex flex-wrap items-center justify-between gap-2">
                    <p className="flex min-w-0 items-center gap-2 font-mono text-[12px]">
                      <span className="truncate text-muted">{s.index}</span>
                      <ArrowRight className="size-3.5 shrink-0 text-subtle" strokeWidth={1.5} />
                      <span className="truncate">{s.dest}</span>
                    </p>
                    <span className="flex items-center gap-2">
                      <span className={`text-[12px] ${state === "failed" || state === "lost" ? "text-danger" : state === "done" ? "text-fg" : state === "running" ? "text-accent" : "text-subtle"}`}>
                        {state === "running" ? <span className="shimmer">Copying</span> : state === "queued" ? "Waiting for a free slot" : state === "done" ? "Done" : state === "cancelled" || state === "lost" ? "Stopped" : "Failed"}
                      </span>
                      {state === "running" && s.task && (
                        <button onClick={() => cancel(s.task!)} className={btn}>
                          Cancel
                        </button>
                      )}
                    </span>
                  </div>
                  {s.task && (
                    <>
                      <div className="h-1 overflow-hidden rounded-full bg-panel-2">
                        <div
                          className={`h-full rounded-full transition-[width] duration-300 ease-out motion-reduce:transition-none ${state === "failed" ? "bg-danger" : state === "done" ? "bg-fg/70" : "bg-accent"}`}
                          style={{ width: `${width}%` }}
                        />
                      </div>
                      <p className="font-mono text-[11px] text-subtle">
                        {formatCount(pr?.copied ?? 0)} of {pr && pr.total > 0 ? formatCount(pr.total) : "…"} docs · task {s.task}
                      </p>
                    </>
                  )}
                  {(s.error || pr?.error) && <p className="break-words font-mono text-[11px] text-danger">{s.error ?? pr?.error}</p>}
                  {s.note && <p className="text-[12px] text-muted">{s.note}</p>}
                  {s.plan && !s.step && <PlanSummary plan={s.plan} />}
                  {after && (
                    <div className="space-y-2 border-t border-line pt-2 text-[12px]">
                      <p className="text-muted">
                        {after === "resume"
                          ? "Resume the copy from its last page (roll kept its place), or delete the partial copy."
                          : "Catch up with what changed at the source since the copy started, then cut over: an alias moves to the copy in one step."}
                      </p>
                      <div className="grid gap-2 sm:grid-cols-2">
                        {!form.apiKey && kind !== "meilisearch" && (
                          <input type="password" value={n.key} onChange={(e) => setNextOf(s, { key: e.target.value })} placeholder="API key again" aria-label={`API key for ${s.index}`} className={`${input} py-1 font-mono text-[12px]`} autoComplete="off" spellCheck={false} />
                        )}
                        {after === "after" && (
                          <input value={n.since} onChange={(e) => setNextOf(s, { since: e.target.value })} placeholder="updated_at (optional)" aria-label={`Timestamp attribute for ${s.index}`} className={`${input} py-1 font-mono text-[12px]`} spellCheck={false} />
                        )}
                      </div>
                      {after === "after" && sinceProblem(n.since) && <p className="text-danger">{sinceProblem(n.since)}</p>}
                      <div className="flex flex-wrap items-center gap-2">
                        {after === "resume" ? (
                          <button onClick={() => runStep(s, "copy")} disabled={n.busy || (!form.apiKey && kind !== "meilisearch" && !n.key.trim())} className={btn}>
                            Resume
                          </button>
                        ) : (
                          <>
                            <button onClick={() => runStep(s, "catch_up")} disabled={n.busy || !!sinceProblem(n.since) || (!form.apiKey && kind !== "meilisearch" && !n.key.trim())} className={btn}>
                              Catch up
                            </button>
                            <input value={n.alias} onChange={(e) => setNextOf(s, { alias: e.target.value })} aria-label={`Alias for ${s.dest}`} className={`${input} w-40 py-1 font-mono text-[12px]`} spellCheck={false} />
                            <button onClick={() => runStep(s, "cutover")} disabled={n.busy || !n.alias.trim()} className={btn}>
                              Cut over
                            </button>
                          </>
                        )}
                        {n.busy && <span className="shimmer text-subtle">Asking roll…</span>}
                      </div>
                      {n.error && <p className="break-words text-danger">{n.error}</p>}
                    </div>
                  )}
                  {s.task && state !== "queued" && partial(state) && (
                    <div className="flex flex-wrap items-center gap-2 text-[12px]">
                      {removed[s.dest] === "done" ? (
                        <span className="text-muted">
                          <span className="font-mono text-fg">{s.dest}</span> deleted. Copy it again with Migrate more.
                        </span>
                      ) : (
                        <>
                          <span className="text-muted">The copy holds what was written before it stopped.</span>
                          <button onClick={() => removePartial(s.dest)} disabled={removed[s.dest] === true} className={btn}>
                            Delete the partial copy
                          </button>
                        </>
                      )}
                      {typeof removed[s.dest] === "string" && removed[s.dest] !== "done" && <span className="w-full text-danger">{removed[s.dest] as string}</span>}
                    </div>
                  )}
                  {groups.length > 0 && (
                    <details className="text-[12px]">
                      <summary className="cursor-pointer text-subtle transition hover:text-fg">
                        {s.dropped!.length} {s.dropped!.length === 1 ? "setting" : "settings"} not copied
                      </summary>
                      <ul className="mt-2 space-y-1.5">
                        {groups.map((g) => (
                          <li key={g.reason}>
                            <span className="text-muted">{g.reason}</span>
                            <span className="block break-all font-mono text-[11px] text-subtle">{g.settings.join(", ")}</span>
                          </li>
                        ))}
                      </ul>
                    </details>
                  )}
                  {a && state === "done" && !s.plan && !s.step && (
                    <div className="flex flex-wrap items-center gap-2 border-t border-line pt-2 text-[12px]">
                      {a.done ? (
                        <span className="text-muted">
                          Alias <span className="font-mono text-fg">{a.alias}</span> points at <span className="font-mono text-fg">{s.dest}</span>.
                        </span>
                      ) : (
                        <>
                          <span className="text-muted">Keep clients working under the old name:</span>
                          <input
                            value={a.alias}
                            onChange={(e) => setAliases((x) => ({ ...x, [s.index]: { ...a, alias: e.target.value } }))}
                            aria-label="Alias name"
                            className={`${input} w-40 py-1 font-mono text-[12px]`}
                            spellCheck={false}
                          />
                          <button onClick={() => pointAlias(s.index, s.dest)} disabled={a.busy || !a.alias.trim()} className={btn}>
                            Point alias at {s.dest}
                          </button>
                        </>
                      )}
                      {a.error && <span className="w-full text-danger">{a.error}</span>}
                    </div>
                  )}
                </div>
              );
            })}
            <div className="flex flex-wrap items-center justify-between gap-3">
              <p className="text-[12px] text-subtle">
                Copies run on roll as reindex tasks: closing this panel doesn&apos;t stop them.
                {started.some((x) => x.queued) &&
                  " Copies still waiting start only while this panel stays open, and if the remote can't be reached when their turn comes, they stop: start them again with Migrate more."}
              </p>
              {allEnded && (
                <button onClick={restart} className={btn}>
                  Migrate more
                </button>
              )}
            </div>
          </div>
        )}
        <ErrorNote error={error} />
      </div>
    </Panel>
  );
}
