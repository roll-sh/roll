import { Play, Square, TerminalSquare, Trash2 } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { appsApi, followChanges, followLog, runStatus, type CronRun, type Environment, type Job, type LogLine, type Scheduled } from "@/lib/apps-api";
import { btn, EmptyPanel, input, Panel, primary } from "../ui";
import { usePageVisible } from "./live";
import { clock, LogView, message, SureButton, until, type Runner } from "./shared";

const MAX_LINES = 5000;

/** A job's or a run's status: a dot (the accent while it runs) and its word, with the exit code when it failed. */
function JobBadge({ job }: { job: Pick<Job, "status" | "exit_code"> }) {
  const s = runStatus(job.status);
  const dot = { busy: "bg-accent pulse-dot", ok: "bg-ok", error: "bg-danger", muted: "bg-subtle" }[s.tone];
  return (
    <span className={`inline-flex items-center gap-1.5 text-[12px] ${s.tone === "error" ? "text-danger" : s.tone === "muted" ? "text-muted" : "text-fg"}`}>
      <i className={`size-1.5 shrink-0 rounded-full ${dot}`} />
      {s.label}
      {job.status === "failed" && job.exit_code !== null && <span className="font-mono text-muted">exit {job.exit_code}</span>}
    </span>
  );
}

/**
 * Commands on a schedule (Heroku Scheduler, Vercel crons): the service's `run.schedules`, each a
 * job below at its due times, skipped while its last one still runs.
 */
function Schedules({ service, env, schedules, states, runner, onRan }: { service: string; env: string; schedules: Scheduled[]; states: Environment["schedules"]; runner: Runner; onRan: (j: Job) => void }) {
  const [name, setName] = useState("");
  const [schedule, setSchedule] = useState("0 3 * * *");
  const [command, setCommand] = useState("");
  const save = (next: Scheduled[]) => runner.run("schedules", () => appsApi.update(service, { run: { schedules: next } }));
  const ok = /^[a-z0-9-]{1,32}$/.test(name) && !schedules.some((s) => s.name === name) && schedule.trim() !== "" && command.trim() !== "";
  return (
    <Panel title="On a schedule" aside={<span className="text-[12px] text-muted">each a job at its due times, UTC unless a time zone is set</span>}>
      {schedules.length > 0 && (
        <ul className="divide-y divide-line border-b border-line">
          {schedules.map((s) => (
            <li key={s.name} className="flex flex-wrap items-center gap-x-3 gap-y-1 px-5 py-2.5 text-[12.5px]">
              <span className="font-medium">{s.name}</span>
              <code className="font-mono text-[12px] text-muted">{s.schedule}{s.timezone ? ` (${s.timezone})` : ""}</code>
              <code className="min-w-0 flex-1 truncate font-mono text-[12px]">{s.command}</code>
              {(() => {
                const st = states?.find((x) => x.name === s.name);
                return (
                  <>
                    {st?.last && <JobBadge job={{ status: st.last.status, exit_code: null }} />}
                    {st?.next_run_at && (
                      <span className="text-[11.5px] text-subtle" title={new Date(st.next_run_at).toLocaleString()}>
                        next {until(st.next_run_at, Date.now())}
                      </span>
                    )}
                  </>
                );
              })()}
              <button
                type="button"
                className="inline-flex items-center gap-1 text-[12px] text-muted hover:text-fg disabled:opacity-50"
                disabled={runner.busy !== null}
                title="Run it now, as a job (its schedule stays as it is)"
                onClick={() => void runner.run(`run:${s.name}`, () => appsApi.startJob(service, env, { command: s.command, ...(s.timeout_s ? { timeout_s: s.timeout_s } : {}) })).then((j) => j && onRan(j))}
              >
                <Play className="size-3" /> Run now
              </button>
              <SureButton className="p-1 text-subtle hover:text-fg" label={`Remove the schedule ${s.name}`} sure="Sure? Remove" disabled={runner.busy !== null} onSure={() => save(schedules.filter((x) => x.name !== s.name))}>
                <Trash2 className="size-3.5" />
              </SureButton>
            </li>
          ))}
        </ul>
      )}
      <form
        className="flex flex-wrap gap-2 px-5 py-4"
        onSubmit={(e) => {
          e.preventDefault();
          void save([...schedules, { name, schedule: schedule.trim(), command: command.trim() }]).then((r) => {
            if (r) {
              setName("");
              setCommand("");
            }
          });
        }}
      >
        <input value={name} onChange={(e) => setName(e.target.value.toLowerCase())} placeholder="cleanup" aria-label="Schedule name" spellCheck={false} className={`${input} w-32 font-mono text-[12px]`} />
        <input value={schedule} onChange={(e) => setSchedule(e.target.value)} placeholder="0 3 * * *" aria-label="When (cron)" spellCheck={false} className={`${input} w-32 font-mono text-[12px]`} />
        <input value={command} onChange={(e) => setCommand(e.target.value)} placeholder="node scripts/cleanup.js" aria-label="Scheduled command" spellCheck={false} className={`${input} min-w-0 flex-1 font-mono text-[12px]`} />
        <button type="submit" className={primary} disabled={!ok || runner.busy !== null}>
          Add
        </button>
      </form>
    </Panel>
  );
}

/**
 * One-off jobs (`roll apps run`): a command in a fresh container of the live deployment, with
 * the environment's variables. Run one, watch its output live, cancel it; the recent ones below.
 */
export function JobsTab({ service, env, runner, cron = false, schedules, states }: { service: string; env: string; runner: Runner; cron?: boolean; schedules?: Scheduled[]; states?: Environment["schedules"] }) {
  const [jobs, setJobs] = useState<Job[] | null>(null);
  const [command, setCommand] = useState("");
  const [selected, setSelected] = useState<string | null>(null);
  const [lines, setLines] = useState<LogLine[]>([]);
  const [error, setError] = useState<string | null>(null);
  const buffer = useRef<LogLine[]>([]);

  const load = useCallback(async () => {
    try {
      const js = await appsApi.jobs(service, env);
      setJobs(js);
      setSelected((s) => s ?? js[0]?.id ?? null);
      setError(null);
    } catch (e) {
      setError(message(e));
    }
  }, [service, env]);

  useEffect(() => {
    void load();
  }, [load]);

  // While one runs, its status is polled; its output streams below.
  const running = jobs?.some((j) => j.status === "running") ?? false;
  const visible = usePageVisible();
  useEffect(() => {
    if (!running || !visible) return;
    // A job's end is a tick on the service's change stream; a slow poll is the safety net.
    const stop = followChanges(service, { onChange: () => void load() });
    const t = setInterval(() => void load(), 15000);
    return () => {
      stop();
      clearInterval(t);
    };
  }, [running, load, visible, service]);

  useEffect(() => {
    if (!selected) return;
    buffer.current = [];
    setLines([]);
    let pending = false;
    const flush = () => {
      pending = false;
      setLines(buffer.current.slice());
    };
    return followLog(
      { job: { service, env, job: selected } },
      {
        tail: 2000,
        onReset: () => {
          buffer.current = [];
          flush();
        },
        onLine: (l) => {
          buffer.current.push(l);
          if (buffer.current.length > MAX_LINES) buffer.current.splice(0, buffer.current.length - MAX_LINES);
          if (!pending) {
            pending = true;
            requestAnimationFrame(flush);
          }
        },
      },
    );
  }, [service, env, selected]);

  const run = async () => {
    const cmd = command.trim();
    if (!cmd) return;
    const job = await runner.run("job", () => appsApi.startJob(service, env, { command: cmd }));
    if (job) {
      setSelected(job.id);
      setJobs((js) => [job, ...(js ?? [])]);
    }
  };

  const current = jobs?.find((j) => j.id === selected) ?? null;

  return (
    <div className="space-y-6">
      {cron && <ScheduledRuns service={service} env={env} runner={runner} />}
      {schedules && (
        <Schedules
          service={service}
          env={env}
          schedules={schedules}
          states={states}
          runner={runner}
          onRan={(job) => {
            setSelected(job.id);
            setJobs((js) => [job, ...(js ?? [])]);
          }}
        />
      )}
      <Panel title="Run a command" aside={<span className="text-[12px] text-muted">in a fresh container of the live deployment, with its variables</span>}>
        <form
          className="flex flex-wrap gap-2 px-5 py-4"
          onSubmit={(e) => {
            e.preventDefault();
            void run();
          }}
        >
          <input
            value={command}
            onChange={(e) => setCommand(e.target.value)}
            placeholder="npx prisma db seed"
            aria-label="Command"
            spellCheck={false}
            className={`${input} min-w-0 flex-1 font-mono text-[12px]`}
          />
          <button type="submit" className={primary} disabled={runner.busy !== null || !command.trim()}>
            <Play className="size-3.5" /> Run
          </button>
        </form>
        <p className="border-t border-line px-5 py-3 text-[12px] text-muted">
          From a terminal: <code className="font-mono text-fg">roll apps run -s {service} -e {env} -- {command.trim() || "<command>"}</code>, or{" "}
          <code className="font-mono text-fg">roll apps ssh</code> for a shell in a running instance.
        </p>
      </Panel>

      {error && <p className="text-[13px] text-danger">{error}</p>}

      {jobs && jobs.length === 0 ? (
        <EmptyPanel>
          <TerminalSquare className="mx-auto mb-2 size-4 text-muted" />
          No jobs yet. A job runs once (a seed, a backfill, a one-off script) and its container goes when it ends.
        </EmptyPanel>
      ) : (
        jobs && (
          <div className="grid gap-6 lg:grid-cols-[minmax(0,280px)_minmax(0,1fr)]">
            <Panel title="Recent">
              <ul className="divide-y divide-line">
                {jobs.map((j) => (
                  <li key={j.id}>
                    <button
                      type="button"
                      onClick={() => setSelected(j.id)}
                      className={`w-full px-5 py-3 text-left transition-colors duration-150 hover:bg-panel-2 ${j.id === selected ? "bg-panel-2" : ""}`}
                    >
                      <span className="block truncate font-mono text-[12px]">
                        {j.command}
                        {(j.tasks ?? 1) > 1 && <span className="text-muted"> × {j.tasks}</span>}
                      </span>
                      <span className="mt-1 flex items-center justify-between gap-2">
                        <JobBadge job={j} />
                        <span className="font-mono text-[11px] text-subtle">{clock(j.started_at, true)}</span>
                      </span>
                    </button>
                  </li>
                ))}
              </ul>
            </Panel>
            <Panel
              title={current ? <span className="font-mono text-[13px]">{current.command}</span> : "Output"}
              aside={
                current?.status === "running" ? (
                  <button type="button" className={btn} disabled={runner.busy !== null} onClick={() => void runner.run("cancel job", () => appsApi.cancelJob(service, env, current.id)).then(load)}>
                    <Square className="size-3.5" /> Cancel
                  </button>
                ) : (
                  current && <JobBadge job={current} />
                )
              }
            >
              <LogView lines={lines} empty={<span className="text-muted">No output yet.</span>} className="h-[420px]" />
            </Panel>
          </div>
        )
      )}
    </div>
  );
}

/** A cron service's recent runs: when, how it ended, how long it took; one opened shows its output (and cancels while it runs). */
function ScheduledRuns({ service, env, runner }: { service: string; env: string; runner: Runner }) {
  const [runs, setRuns] = useState<CronRun[] | null>(null);
  const [open, setOpen] = useState<string | null>(null);
  const [lines, setLines] = useState<LogLine[]>([]);
  const visible = usePageVisible();
  const load = useCallback(() => appsApi.runs(service, env).then(setRuns).catch(() => {}), [service, env]);
  useEffect(() => {
    if (!visible) return;
    void load();
    const t = setInterval(load, 10_000);
    return () => clearInterval(t);
  }, [load, visible]);
  useEffect(() => {
    if (!open) return;
    const buffer: LogLine[] = [];
    setLines([]);
    return followLog(
      { run: { service, env, run: open } },
      {
        tail: 2000,
        onReset: () => {
          buffer.length = 0;
          setLines([]);
        },
        onLine: (l) => {
          buffer.push(l);
          if (buffer.length > MAX_LINES) buffer.splice(0, buffer.length - MAX_LINES);
          setLines(buffer.slice());
        },
      },
    );
  }, [service, env, open]);
  const current = runs?.find((r) => r.id === open) ?? null;
  return (
    <Panel
      title="Scheduled runs"
      aside={
        current?.status === "running" && (
          <button type="button" className={btn} disabled={runner.busy !== null} onClick={() => void runner.run("cancel run", () => appsApi.cancelRun(service, env, current.id)).then(load)}>
            <Square className="size-3.5" /> Cancel
          </button>
        )
      }
    >
      {!runs?.length ? (
        <p className="px-5 py-4 text-[13px] text-muted">No runs yet. The schedule starts one; Run now on this page starts one at once.</p>
      ) : (
        <>
          <ul className="divide-y divide-line">
            {runs.slice(0, 10).map((r) => (
              <li key={r.id}>
                <button
                  type="button"
                  aria-expanded={open === r.id}
                  onClick={() => setOpen((o) => (o === r.id ? null : r.id))}
                  className={`flex w-full items-center justify-between gap-3 px-5 py-2.5 text-left transition-colors hover:bg-fg/[0.03] ${open === r.id ? "bg-fg/[0.04]" : ""}`}
                >
                  <JobBadge job={r} />
                  <span className="font-mono text-[11.5px] text-subtle">
                    {clock(r.started_at, true)}
                    {r.finished_at ? ` · ${Math.max(1, Math.round((r.finished_at - r.started_at) / 1000))} s` : ""}
                    {r.attempt > 1 ? ` · try ${r.attempt}` : ""}
                  </span>
                </button>
              </li>
            ))}
          </ul>
          {open && <LogView lines={lines} empty={<span className="text-muted">No output from this run.</span>} className="h-[320px] border-t border-line" />}
        </>
      )}
    </Panel>
  );
}
