import { ExternalLink } from "lucide-react";
import { useEffect, useState } from "react";
import { appsApi, type Incident, type IncidentState, type StatusPage } from "@/lib/apps-api";
import { btn, input, Panel, primary } from "../ui";
import { since, SureButton, type Runner } from "./shared";

const STATES: IncidentState[] = ["investigating", "identified", "monitoring", "maintenance", "resolved"];
const when = (ms: number) => new Date(ms).toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" });

/** The page's incidents, newest first: each open one updated here (a state and what visitors
 *  should know; `resolved` closes it), as the API and `roll apps status-page` do. */
function Incidents({ project, incidents, runner, reload }: { project: string; incidents: Incident[]; runner: Runner; reload: () => void }) {
  const [draft, setDraft] = useState<Record<string, { state: IncidentState; text: string }>>({});
  if (!incidents.length) return null;
  return (
    <ul className="divide-y divide-line border-t border-line">
      {incidents.map((i) => {
        const d = draft[i.id] ?? { state: i.state === "investigating" ? "identified" : i.state, text: "" };
        const open = i.state !== "resolved";
        return (
          <li key={i.id} className="space-y-2 px-5 py-3 text-[12.5px]">
            <div className="flex flex-wrap items-center gap-2">
              <span className="min-w-0 flex-1 truncate text-fg">{i.title}</span>
              <span className={open ? "text-accent" : "text-muted"}>{i.state}</span>
              <span className="text-subtle">{i.automatic ? "opened by its check" : "posted"} {since(i.started_at)}</span>
            </div>
            {i.scheduled && (
              <p className="text-[12px] text-fg">
                {when(i.scheduled.from)} – {when(i.scheduled.until)}
              </p>
            )}
            {i.updates.length > 0 && <p className="text-[12px] text-muted">{i.updates[i.updates.length - 1]!.text}</p>}
            {open && (
              <form
                className="flex flex-wrap items-center gap-2"
                onSubmit={(e) => {
                  e.preventDefault();
                  void runner.run(`incident:${i.id}`, () => appsApi.updateIncident(project, i.id, { state: d.state, text: d.text.trim() }).then(() => true)).then((r) => {
                    if (!r) return;
                    setDraft((x) => ({ ...x, [i.id]: { state: d.state, text: "" } }));
                    reload();
                  });
                }}
              >
                <select value={d.state} onChange={(e) => setDraft((x) => ({ ...x, [i.id]: { ...d, state: e.target.value as IncidentState } }))} aria-label={`${i.title}'s state`} className={`${input} w-36 text-[12px]`}>
                  {STATES.map((s) => (
                    <option key={s} value={s}>
                      {s}
                    </option>
                  ))}
                </select>
                <input value={d.text} onChange={(e) => setDraft((x) => ({ ...x, [i.id]: { ...d, text: e.target.value } }))} placeholder="What's changed" aria-label={`${i.title}'s update`} maxLength={2000} className={`${input} min-w-56 flex-1 text-[12px]`} />
                <button type="submit" className={btn} disabled={runner.busy !== null || !d.text.trim()}>
                  Post update
                </button>
              </form>
            )}
          </li>
        );
      })}
    </ul>
  );
}

/**
 * A project's status page (Better Stack's, Instatus'): its uptime checks and heartbeats, up or
 * down and their last 90 days, on a public address of its own. Names only, never a path or an
 * error.
 */
export function StatusPagePanel({ project, projectName, runner }: { project: string; projectName: string; runner: Runner }) {
  const [page, setPage] = useState<StatusPage | null | undefined>(undefined);
  const [title, setTitle] = useState("");
  const [incident, setIncident] = useState({ title: "", text: "" });
  // A scheduled maintenance's window, as the browser's local time (datetime-local).
  const [window_, setWindow] = useState<{ on: boolean; from: string; until: string }>({ on: false, from: "", until: "" });
  const [incidents, setIncidents] = useState<Incident[]>([]);
  const reload = () => void appsApi.incidents(project).then(setIncidents, () => setIncidents([]));
  useEffect(() => {
    appsApi.statusPage(project).then((p) => {
      setPage(p);
      if (p) reload();
    }, () => setPage(null));
  }, [project]);
  const busy = runner.busy !== null;
  const save = (t: string) => runner.run("status-page", async () => setPage(await appsApi.putStatusPage(project, { title: t.trim() || undefined })));
  if (page === undefined) return null;
  return (
    <Panel title="Status page" aside={<span className="text-[12px] text-muted">its uptime checks and heartbeats, in public</span>}>
      {page ? (
        <div className="flex flex-wrap items-center gap-3 px-5 py-4 text-[12.5px]">
          <a href={page.url} target="_blank" rel="noreferrer" className="inline-flex min-w-0 flex-1 items-center gap-1 truncate font-mono text-[12px] text-accent hover:underline">
            {page.url} <ExternalLink className="size-3.5 shrink-0" />
          </a>
          <span className="text-muted">{page.title}</span>
          <SureButton className="text-[12px] text-muted hover:text-fg" label="Take the status page down" sure="Sure? Take it down" disabled={busy} onSure={() => runner.run("status-page", async () => { await appsApi.deleteStatusPage(project); setPage(null); })}>
            Take down
          </SureButton>
        </div>
      ) : null}
      {page && (
        <form
          className="flex flex-wrap items-center gap-2 border-t border-line px-5 py-4"
          onSubmit={(e) => {
            e.preventDefault();
            const body = window_.on ? { ...incident, state: "scheduled", starts_at: new Date(window_.from).getTime(), ends_at: new Date(window_.until).getTime() } : incident;
            void runner.run("incident", () => appsApi.postIncident(project, body)).then((r) => {
              if (!r) return;
              setIncident({ title: "", text: "" });
              setWindow({ on: false, from: "", until: "" });
              reload();
            });
          }}
        >
          <input value={incident.title} onChange={(e) => setIncident({ ...incident, title: e.target.value })} placeholder="An incident's title (slow checkouts)" aria-label="Incident title" maxLength={120} className={`${input} min-w-48 flex-1 text-[12px]`} />
          <input value={incident.text} onChange={(e) => setIncident({ ...incident, text: e.target.value })} placeholder="What visitors should know" aria-label="Incident text" maxLength={2000} className={`${input} min-w-64 flex-[2] text-[12px]`} />
          <button type="submit" className={primary} disabled={busy || !incident.title.trim() || !incident.text.trim() || (window_.on && (!window_.from || !window_.until))}>
            {window_.on ? "Schedule it" : "Post an incident"}
          </button>
          <label className="flex w-full items-center gap-2 text-[12px] text-muted">
            <input type="checkbox" checked={window_.on} onChange={(e) => setWindow({ ...window_, on: e.target.checked })} />
            Scheduled maintenance: shown ahead, begun and over on its own
          </label>
          {window_.on && (
            <div className="flex w-full flex-wrap items-center gap-2">
              <input type="datetime-local" value={window_.from} onChange={(e) => setWindow({ ...window_, from: e.target.value })} aria-label="Maintenance starts" className={`${input} w-52 text-[12px]`} />
              <span className="text-[12px] text-subtle">to</span>
              <input type="datetime-local" value={window_.until} onChange={(e) => setWindow({ ...window_, until: e.target.value })} aria-label="Maintenance ends" className={`${input} w-52 text-[12px]`} />
            </div>
          )}
          <span className="w-full text-[12px] text-muted">Checks going down and heartbeats missed open and close their own.</span>
        </form>
      )}
      {page && <Incidents project={project} incidents={incidents} runner={runner} reload={reload} />}
      {!page && (
        <form
          className="flex flex-wrap items-center gap-2 px-5 py-4"
          onSubmit={(e) => {
            e.preventDefault();
            void save(title);
          }}
        >
          <input value={title} onChange={(e) => setTitle(e.target.value)} placeholder={`${projectName} status`} aria-label="Title" maxLength={80} className={`${input} min-w-48 flex-1 text-[12px]`} />
          <button type="submit" className={primary} disabled={busy}>
            Publish a status page
          </button>
          <span className="w-full text-[12px] text-muted">Every check and heartbeat of the project's environments, by name: never a path, an address or an error.</span>
        </form>
      )}
    </Panel>
  );
}
