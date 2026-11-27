/**
 * The Alerts tab: Elasticsearch's Watcher (Kibana's Stack Management → Watcher), which roll runs on
 * the owner node's scheduler. Every watch with where it stands, its schedule and when it last ran
 * and fired; run one now (simulated: its actions are shown, not sent), pause or resume it, see
 * its last runs from `.watcher-history-*`, delete it; or start one from a template. Everything is
 * the cluster's own Watcher API, and any watch opens in the console as its request.
 *
 * OpenSearch-flavor clusters don't show this tab: their Alerting plugin is a later phase.
 */

import { Bell, History, Pause, Play, Plus, SquareTerminal, Trash2 } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { esJson, type SearchCluster } from "@/lib/search-api";
import {
  historyQuery,
  INTERVAL_RE,
  runsOf,
  scheduleText,
  TEMPLATES,
  WATCH_ID_RE,
  watchFromTemplate,
  watchState,
  type TemplateInput,
  type WatchRow,
  type WatchRun,
} from "@/lib/search-watcher";
import { btn, Confirm, input, Panel, primary } from "../ui";
import { errText } from "./relevance-shared";

const enc = encodeURIComponent;

function when(iso: string | undefined | null): string {
  if (!iso) return "—";
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? iso : d.toLocaleString(undefined, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
}

const STATE_CLASS: Record<ReturnType<typeof watchState>, string> = {
  firing: "text-danger",
  failing: "text-danger",
  ok: "text-ok",
  paused: "text-subtle",
  "not run yet": "text-subtle",
};

export function WatcherTab({ cluster, onOpenConsole }: { cluster: SearchCluster; onOpenConsole: (text: string) => void }) {
  const [watches, setWatches] = useState<WatchRow[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [history, setHistory] = useState<{ id: string; runs: WatchRun[] | null } | null>(null);
  const [ran, setRan] = useState<{ id: string; text: string } | null>(null);
  const [deleting, setDeleting] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const v = await esJson<{ watches: WatchRow[] }>(cluster.id, "POST", "/_watcher/_query/watches", { size: 100 });
      setWatches(v.watches ?? []);
      setError(null);
    } catch (e) {
      setError(errText(e));
    }
  }, [cluster.id]);
  useEffect(() => {
    void load();
  }, [load]);

  const act = async (key: string, f: () => Promise<void>) => {
    setBusy(key);
    setError(null);
    try {
      await f();
    } catch (e) {
      setError(errText(e));
    } finally {
      setBusy(null);
    }
  };

  const runNow = (id: string) =>
    act(`run:${id}`, async () => {
      const v = await esJson<{ watch_record?: { state?: string; result?: { condition?: { met?: boolean }; actions?: { id: string; status: string }[] } } }>(
        cluster.id,
        "POST",
        `/_watcher/watch/${enc(id)}/_execute`,
        { action_modes: { _all: "simulate" } },
      );
      const r = v.watch_record;
      const met = r?.result?.condition?.met;
      const actions = (r?.result?.actions ?? []).map((a) => `${a.id} ${a.status}`).join(", ");
      setRan({ id, text: met ? `Condition met: it would fire${actions ? ` (${actions})` : ""}.` : `Condition not met (${r?.state ?? "done"}): nothing would fire.` });
    });

  const setActive = (id: string, active: boolean) =>
    act(`active:${id}`, async () => {
      await esJson(cluster.id, "PUT", `/_watcher/watch/${enc(id)}/${active ? "_activate" : "_deactivate"}`);
      await load();
    });

  const showHistory = (id: string) =>
    act(`history:${id}`, async () => {
      setHistory({ id, runs: null });
      try {
        const v = await esJson<{ hits: { hits: { _id: string; _source?: Record<string, unknown> }[] } }>(
          cluster.id,
          "POST",
          "/.watcher-history-*/_search?expand_wildcards=all&ignore_unavailable=true",
          historyQuery(id),
        );
        setHistory({ id, runs: runsOf(v.hits?.hits ?? []) });
      } catch {
        setHistory({ id, runs: [] });
      }
    });

  const remove = (id: string) =>
    act(`delete:${id}`, async () => {
      await esJson(cluster.id, "DELETE", `/_watcher/watch/${enc(id)}`);
      setDeleting(null);
      if (history?.id === id) setHistory(null);
      await load();
    });

  const newButton = (
    <button className={btn} onClick={() => setCreating(true)} disabled={creating}>
      <Plus className="size-3.5" /> New watch
    </button>
  );

  return (
    <div className="space-y-6">
      <Panel title="Watches" aside={watches && watches.length > 0 && newButton}>
        {error && <p className="px-5 pt-4 text-[13px] text-danger">{error}</p>}
        {!watches && !error && (
          <p className="px-5 py-4 text-[13px] text-subtle">
            <span className="shimmer">Loading watches…</span>
          </p>
        )}
        {watches && watches.length === 0 && !creating && (
          <div className="flex flex-col items-start gap-2 px-5 pt-8 pb-6">
            <p className="flex items-center gap-2 text-[14px] font-medium">
              <Bell className="size-4 text-subtle" /> Get told when something changes
            </p>
            <p className="max-w-xl text-[13px] text-muted">
              A watch runs a search on a schedule and acts when its condition holds: too many errors in your logs, or a data stream
              that stopped receiving documents. It posts to a webhook or writes a log line.
            </p>
            {newButton}
          </div>
        )}
        {creating && (
          <NewWatch
            cluster={cluster}
            taken={(watches ?? []).map((w) => w._id)}
            onOpenConsole={onOpenConsole}
            onCancel={() => setCreating(false)}
            onCreated={async () => {
              setCreating(false);
              await load();
            }}
          />
        )}
        {watches && watches.length > 0 && (
          <div className="overflow-x-auto">
            <table className="w-full min-w-[48rem] text-[13px]">
              <thead>
                <tr className="border-b border-line text-left text-[11px] text-subtle">
                  <th className="px-5 py-2 font-normal">Watch</th>
                  <th className="px-2 py-2 font-normal">State</th>
                  <th className="px-2 py-2 font-normal">Schedule</th>
                  <th className="px-2 py-2 font-normal">Last run</th>
                  <th className="px-2 py-2 font-normal">Last fired</th>
                  <th className="px-5 py-2" />
                </tr>
              </thead>
              <tbody>
                {watches.map((w) => {
                  const state = watchState(w.status);
                  const active = w.status.state?.active !== false;
                  return (
                    <tr key={w._id} className="border-b border-line last:border-0 align-top">
                      <td className="px-5 py-2 font-mono text-[12px]">{w._id}</td>
                      <td className={`px-2 py-2 text-[12px] ${STATE_CLASS[state]}`}>{state}</td>
                      <td className="px-2 py-2 font-mono text-[12px] text-muted">{scheduleText(w.watch.trigger?.schedule)}</td>
                      <td className="px-2 py-2 text-[12px] text-muted tabular-nums">{when(w.status.last_checked)}</td>
                      <td className="px-2 py-2 text-[12px] text-muted tabular-nums">{when(w.status.last_met_condition)}</td>
                      <td className="px-5 py-1.5">
                        <div className="flex justify-end gap-1">
                          <button className={btn} disabled={busy !== null} onClick={() => void runNow(w._id)} title="Run now, its actions simulated">
                            <Play className="size-3.5" /> Run
                          </button>
                          <button className={btn} disabled={busy !== null} onClick={() => void setActive(w._id, !active)} title={active ? "Pause" : "Resume"}>
                            {active ? <Pause className="size-3.5" /> : <Play className="size-3.5" />}
                            {active ? "Pause" : "Resume"}
                          </button>
                          <button className={btn} disabled={busy !== null} onClick={() => void showHistory(w._id)} title="Last runs">
                            <History className="size-3.5" />
                          </button>
                          <button
                            className={btn}
                            onClick={() => onOpenConsole(`PUT _watcher/watch/${w._id}\n${JSON.stringify(w.watch, null, 2)}`)}
                            title="Open in console"
                          >
                            <SquareTerminal className="size-3.5" />
                          </button>
                          <button className={btn} disabled={busy !== null} onClick={() => setDeleting(w._id)} title="Delete">
                            <Trash2 className="size-3.5" />
                          </button>
                        </div>
                        {ran?.id === w._id && <p className="rise pt-1.5 text-right text-[12px] text-muted">{ran.text}</p>}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
        {deleting && (
          <div className="px-5 py-4">
            <Confirm word={deleting} action="Delete watch" busy={busy === `delete:${deleting}`} onConfirm={() => void remove(deleting)} onCancel={() => setDeleting(null)}>
              Deletes <span className="font-mono text-fg">{deleting}</span>: it stops running. Its past runs stay in the watch history.
            </Confirm>
          </div>
        )}
      </Panel>
      {history && (
        <Panel title={`Last runs of ${history.id}`} aside={<button className={btn} onClick={() => setHistory(null)}>Close</button>}>
          {history.runs === null ? (
            <p className="px-5 py-4 text-[13px] text-subtle">
              <span className="shimmer">Loading runs…</span>
            </p>
          ) : history.runs.length === 0 ? (
            <p className="px-5 py-4 text-[13px] text-subtle">No runs recorded yet.</p>
          ) : (
            <table className="w-full text-[13px]">
              <tbody>
                {history.runs.map((r) => (
                  <tr key={r.id} className="border-b border-line last:border-0">
                    <td className="px-5 py-2 text-[12px] text-muted tabular-nums">{when(r.triggered)}</td>
                    <td className="px-2 py-2 font-mono text-[12px]">{r.state}</td>
                    <td className={`px-2 py-2 text-[12px] ${r.met ? "text-danger" : "text-subtle"}`}>{r.met === null ? "—" : r.met ? "fired" : "quiet"}</td>
                    <td className="truncate px-5 py-2 text-[12px] text-muted">{r.note ?? ""}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </Panel>
      )}
    </div>
  );
}

function NewWatch({
  cluster,
  taken,
  onOpenConsole,
  onCancel,
  onCreated,
}: {
  cluster: SearchCluster;
  taken: string[];
  onOpenConsole: (text: string) => void;
  onCancel: () => void;
  onCreated: () => void;
}) {
  const [id, setId] = useState("");
  const [t, setT] = useState<TemplateInput>({ kind: "errors", index: "logs-*", threshold: 10, interval: "5m", query: "log.level:error", webhook: "" });
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const body = watchFromTemplate(t);
  const problem = !WATCH_ID_RE.test(id)
    ? "A name: lowercase letters, digits, - and _."
    : taken.includes(id)
      ? "A watch has that name."
      : !t.index.trim()
        ? "An index or pattern to watch."
        : !INTERVAL_RE.test(t.interval)
          ? "An interval such as 5m, 1h or 1d."
          : t.webhook.trim() && !/^https:\/\//.test(t.webhook.trim())
            ? "A webhook URL starting with https://."
            : null;
  const create = async () => {
    setBusy(true);
    setError(null);
    try {
      await esJson(cluster.id, "PUT", `/_watcher/watch/${enc(id)}`, body);
      onCreated();
    } catch (e) {
      setError(errText(e));
    } finally {
      setBusy(false);
    }
  };
  const set = (patch: Partial<TemplateInput>) => setT((x) => ({ ...x, ...patch }));
  return (
    <div className="rise space-y-3 border-b border-line px-5 py-4">
      <div className="flex flex-wrap gap-2">
        {TEMPLATES.map((x) => (
          <button
            key={x.kind}
            onClick={() => set({ kind: x.kind })}
            className={`rounded-lg border px-3 py-2 text-left text-[12px] transition ${t.kind === x.kind ? "border-line-2 text-fg" : "border-line text-muted hover:text-fg"}`}
            aria-pressed={t.kind === x.kind}
          >
            <div className="font-medium">{x.label}</div>
            <div className="max-w-xs text-subtle">{x.hint}</div>
          </button>
        ))}
      </div>
      <div className="grid gap-3 sm:grid-cols-2">
        <label className="space-y-1">
          <span className="text-[12px] text-subtle">Name</span>
          <input className={`${input} w-full font-mono text-[12px]`} value={id} onChange={(e) => setId(e.target.value.toLowerCase())} placeholder="checkout-errors" />
        </label>
        <label className="space-y-1">
          <span className="text-[12px] text-subtle">Index, data stream or pattern</span>
          <input className={`${input} w-full font-mono text-[12px]`} value={t.index} onChange={(e) => set({ index: e.target.value })} />
        </label>
        {t.kind === "errors" && (
          <>
            <label className="space-y-1">
              <span className="text-[12px] text-subtle">Matching (query string)</span>
              <input className={`${input} w-full font-mono text-[12px]`} value={t.query} onChange={(e) => set({ query: e.target.value })} />
            </label>
            <label className="space-y-1">
              <span className="text-[12px] text-subtle">Fire above this many</span>
              <input type="number" min={0} className={`${input} w-full font-mono text-[12px]`} value={t.threshold} onChange={(e) => set({ threshold: Number(e.target.value) })} />
            </label>
          </>
        )}
        <label className="space-y-1">
          <span className="text-[12px] text-subtle">Every (and looking back as far)</span>
          <input className={`${input} w-full font-mono text-[12px]`} value={t.interval} onChange={(e) => set({ interval: e.target.value })} />
        </label>
        <label className="space-y-1">
          <span className="text-[12px] text-subtle">Webhook (optional; else a log line)</span>
          <input className={`${input} w-full font-mono text-[12px]`} value={t.webhook} onChange={(e) => set({ webhook: e.target.value })} placeholder="https://hooks.example.com/…" />
        </label>
      </div>
      <div className="flex flex-wrap items-center gap-2">
        <button className={primary} disabled={busy || problem !== null} onClick={() => void create()}>
          {busy ? "Creating…" : "Create watch"}
        </button>
        <button className={btn} onClick={() => onOpenConsole(`PUT _watcher/watch/${id || "my-watch"}\n${JSON.stringify(body, null, 2)}`)}>
          <SquareTerminal className="size-3.5" /> Open in console
        </button>
        <button className={btn} onClick={onCancel}>
          Cancel
        </button>
        {(error ?? problem) && <span className={`text-[12px] ${error ? "text-danger" : "text-subtle"}`}>{error ?? problem}</span>}
      </div>
      <p className="text-[12px] text-subtle">
        Webhooks go out from the cluster to public addresses only. A webhook&apos;s credentials in a header are kept sealed and shown as{" "}
        <span className="font-mono">::es_redacted::</span>.
      </p>
    </div>
  );
}
