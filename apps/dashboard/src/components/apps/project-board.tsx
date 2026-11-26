import { Plus, X } from "lucide-react";
import { useEffect, useState } from "react";
import { appsApi, BOARD_METRICS, PRODUCTION, type BoardPanel, type Service } from "@/lib/apps-api";
import { btn, input, panel, primary } from "../ui";
import { appPanelSeries, BoardChart, panelSeries } from "./metrics";

/** A metric's name on a panel. */
const LABELS: Record<(typeof BOARD_METRICS)[number], string> = {
  requests: "Requests",
  errors: "5xx",
  error_pct: "5xx share",
  p50_ms: "p50",
  p95_ms: "p95",
  p99_ms: "p99",
  cpu: "CPU (cores)",
  memory_bytes: "Memory",
  instances: "Instances",
  cold_starts: "Cold starts",
  egress_bytes: "Egress",
  over_capacity: "Over capacity",
};

export const panelTitle = (p: BoardPanel, services: Pick<Service, "id" | "name">[]) => {
  if (p.title) return p.title;
  const name = services.find((s) => s.id === p.service)?.name ?? p.service;
  const what = p.metric.startsWith("app:") ? p.metric.slice(4) : LABELS[p.metric as keyof typeof LABELS];
  const env = p.environment && p.environment !== PRODUCTION ? ` (${p.environment})` : "";
  return `${name}${env} · ${what}`;
};

/** One panel, read through its service's own Metrics route (as its tab reads it). */
function PanelChart({ p, title, minutes }: { p: BoardPanel; title: string; minutes: number }) {
  const [d, setD] = useState<ReturnType<typeof panelSeries> | undefined | { error: string }>(undefined);
  useEffect(() => {
    let live = true;
    setD(undefined);
    const env = p.environment ?? PRODUCTION;
    const read = p.metric.startsWith("app:") ? appsApi.appMetrics(p.service, env, minutes).then((a) => appPanelSeries(a, p.metric.slice(4))) : appsApi.metrics(p.service, env, minutes).then((m) => panelSeries(m, p.metric));
    read.then(
      (x) => live && setD(x),
      (e) => live && setD({ error: e instanceof Error ? e.message : String(e) }),
    );
    return () => {
      live = false;
    };
  }, [p.service, p.environment, p.metric, minutes]);
  if (d === undefined) return <figure className={`${panel} p-4 text-[12px] text-subtle`}><span className="shimmer">{title}</span></figure>;
  if (d === null || "error" in d) return <figure className={`${panel} p-4 text-[12px]`}><span className="text-muted">{title}</span><p className="mt-2 text-subtle">{d && "error" in d ? d.error : "Nothing to show yet."}</p></figure>;
  return <BoardChart title={title} {...d} rangeMinutes={minutes} />;
}

/**
 * A project's board (Railway's observability widgets, Cloud Monitoring's dashboards): the charts
 * someone picked, each one service's metric in one environment, saved with the project
 * (`…/projects/{project}/board`). Each reads its service's own Metrics route, so it shows nothing
 * its reader couldn't read there.
 */
export function ProjectBoard({ project, services, minutes }: { project: string; services: Service[]; minutes: number }) {
  const [panels, setPanels] = useState<BoardPanel[] | null>(null);
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState<BoardPanel>({ service: "", metric: "requests" });
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    appsApi.board(project).then((b) => setPanels(b.panels), () => setPanels([]));
  }, [project]);
  if (panels === null) return null;
  const save = async (next: BoardPanel[]) => {
    setError(null);
    try {
      setPanels((await appsApi.putBoard(project, next)).panels);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };
  const svc = services.find((s) => s.id === draft.service);
  return (
    <section aria-label="Board" className="space-y-3">
      <div className="flex items-center justify-between">
        <h2 className="text-[13px] font-medium">Board</h2>
        <button type="button" className="text-[12px] text-accent hover:underline" onClick={() => setEditing(!editing)}>
          {editing ? "Done" : panels.length ? "Edit" : "Add charts"}
        </button>
      </div>
      {panels.length > 0 && (
        <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
          {panels.map((p, i) => (
            <div key={`${i}:${p.service}:${p.environment ?? ""}:${p.metric}`} className="relative">
              <PanelChart p={p} title={panelTitle(p, services)} minutes={minutes} />
              {editing && (
                <button type="button" aria-label={`Remove ${panelTitle(p, services)}`} className="absolute top-2 right-2 rounded p-1 text-subtle hover:text-fg" onClick={() => void save(panels.filter((_, j) => j !== i))}>
                  <X className="size-3.5" strokeWidth={1.5} />
                </button>
              )}
            </div>
          ))}
        </div>
      )}
      {!panels.length && !editing && <p className="text-[12.5px] text-muted">Charts you pick from its services' metrics, kept for everyone on the project.</p>}
      {editing && (
        <form
          className="flex flex-wrap items-center gap-2"
          onSubmit={(e) => {
            e.preventDefault();
            void save([...panels, { ...draft, environment: draft.environment || PRODUCTION }]).then(() => setDraft({ ...draft, title: undefined }));
          }}
        >
          <select value={draft.service} onChange={(e) => setDraft({ ...draft, service: e.target.value, environment: PRODUCTION })} aria-label="Service" className={`${input} !w-auto text-[12px]`}>
            <option value="">Service…</option>
            {services.map((s) => (
              <option key={s.id} value={s.id}>
                {s.name}
              </option>
            ))}
          </select>
          <select value={draft.environment ?? PRODUCTION} onChange={(e) => setDraft({ ...draft, environment: e.target.value })} aria-label="Environment" className={`${input} !w-auto text-[12px]`}>
            {(svc?.environments.filter((e) => !e.ephemeral).map((e) => e.name) ?? [PRODUCTION]).map((n) => (
              <option key={n}>{n}</option>
            ))}
          </select>
          <select value={draft.metric} onChange={(e) => setDraft({ ...draft, metric: e.target.value as BoardPanel["metric"] })} aria-label="Metric" className={`${input} !w-auto text-[12px]`}>
            {BOARD_METRICS.map((m) => (
              <option key={m} value={m}>
                {LABELS[m]}
              </option>
            ))}
          </select>
          <input value={draft.title ?? ""} onChange={(e) => setDraft({ ...draft, title: e.target.value || undefined })} placeholder="Title (optional)" aria-label="Title" maxLength={60} className={`${input} w-44 text-[12px]`} />
          <button type="submit" className={primary} disabled={!draft.service || panels.length >= 24}>
            <Plus className="size-3.5" /> Add
          </button>
          {panels.length > 0 && (
            <button type="button" className={btn} onClick={() => void save([])}>
              Clear the board
            </button>
          )}
          {error && <span className="w-full text-[12px] text-danger">{error}</span>}
        </form>
      )}
    </section>
  );
}
