import { HorizonMark } from "@roll/brand";
import { ExternalLink, Rocket, RotateCcw, X } from "lucide-react";
import { useEffect, useState } from "react";
import { appsApi, deploymentUrl, followChanges, followLog, formatMs, isPending, shortSha, stepDurations, TRIGGER_LABEL, type Deployment, type DeployCommits, type Environment, type LogLine } from "@/lib/apps-api";
import { btn, panel } from "../ui";
import { PlanDetails } from "./new-service";
import { usePageVisible } from "./live";
import { clock, DeployBadge, LogView, since, SureButton, type Runner } from "./shared";

const MAX_LINES = 5000;

/** A deployment's build log: followed live while it's on its way, read once after. */
function useBuildLog(service: string, env: string, d: Deployment) {
  const [lines, setLines] = useState<LogLine[]>([]);
  const [error, setError] = useState<string | null>(null);
  const pending = isPending(d.status);
  useEffect(() => {
    setError(null);
    if (!pending) {
      let live = true;
      appsApi.buildLog(service, env, d.id, { tail: MAX_LINES }).then(
        (l) => live && setLines(l),
        (e: unknown) => live && setError(e instanceof Error ? e.message : String(e)),
      );
      return () => {
        live = false;
      };
    }
    return followLog(
      { build: { service, env, deployment: d.id } },
      {
        tail: MAX_LINES,
        onReset: () => setLines([]),
        onLine: (l) => setLines((ls) => (ls.length >= MAX_LINES ? [...ls.slice(1 - MAX_LINES), l] : [...ls, l])),
      },
    );
  }, [service, env, d.id, pending]);
  return { lines, error };
}

/** Deployments, newest first: the active one marked, rollback and cancel per row, the build log of the one open. */
export function Deployments({
  service,
  env,
  deploys,
  runner,
  own = false,
}: {
  service: string;
  env: Environment;
  deploys: Deployment[] | null;
  /** Its deployments have their own URLs (a web service without a volume). */
  own?: boolean;
  runner: Runner;
}) {
  const [open, setOpen] = useState<string | null>(null);
  // A rollback brings its variables back as they were (Heroku's, Railway's), unless they're kept
  // as they are now: after a leaked secret's rotation, the old value must not come back.
  const [keepVars, setKeepVars] = useState(false);
  // The newest opens by itself, so a deploy that just started shows its build.
  const shown = deploys?.find((d) => d.id === open) ?? deploys?.[0] ?? null;
  const activeId = env.active?.id ?? null;
  const now = Date.now();

  if (deploys === null) return <p className="text-[13px] text-subtle"><span className="shimmer">Loading deployments…</span></p>;
  if (deploys.length === 0)
    return (
      <section className={`${panel} flex flex-col items-start gap-2 p-6`}>
        <HorizonMark state="sleep" size={32} />
        <h2 className="text-[15px] font-medium">Nothing deployed to {env.name} yet</h2>
        <p className="text-[13px] text-muted">Deploy builds the service's source and rolls it out here. Upload services deploy with <code className="font-mono text-fg">roll up</code>.</p>
      </section>
    );

  return (
    <div className="grid gap-4 lg:grid-cols-[minmax(0,5fr)_minmax(0,7fr)]">
      <section className={`${panel} overflow-hidden`}>
        <label className="flex items-center gap-1.5 border-b border-line px-4 py-2 text-[12px] text-muted" title="Off: a rollback brings the variables back as they were when it was live">
          <input type="checkbox" checked={keepVars} onChange={(e) => setKeepVars(e.target.checked)} />
          Rollbacks keep today's variables
        </label>
        <ul className="divide-y divide-line">
          {deploys.map((d) => {
            const active = d.id === activeId;
            const selected = shown?.id === d.id;
            return (
              <li key={d.id}>
                <div
                  role="button"
                  tabIndex={0}
                  onClick={() => setOpen(d.id)}
                  onKeyDown={(e) => (e.key === "Enter" || e.key === " ") && (e.preventDefault(), setOpen(d.id))}
                  className={`group flex cursor-pointer items-start gap-3 px-4 py-3 transition-colors outline-none focus-visible:bg-fg/[0.04] ${selected ? "bg-fg/[0.05]" : "hover:bg-fg/[0.03]"}`}
                >
                  <div className="min-w-0 flex-1">
                    <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
                      <DeployBadge status={d.status} />
                      {active && <span className="rounded border border-accent/40 px-1.5 font-mono text-[10.5px] text-accent">live</span>}
                      <span className="text-[11.5px] text-subtle">
                        {TRIGGER_LABEL[d.trigger]}
                        {d.actor && (d.actor.startsWith("git:") ? <span title="The commit's author, as the commit says"> by {d.actor.slice(4)}</span> : <> by {d.actor}</>)}
                      </span>
                      {d.revision && <span className="font-mono text-[11.5px] text-muted">{shortSha(d.revision)}</span>}
                    </div>
                    <p className="mt-1 truncate text-[13px]">{d.message || (d.rollback_of ? `Roll back to ${d.rollback_of}` : <span className="text-muted">No message</span>)}</p>
                    <p className="mt-0.5 flex gap-2 font-mono text-[11px] text-subtle">
                      <span title={clock(d.created_at, true)}>{since(d.created_at, now)}</span>
                      {d.ready_ms != null && <span>ready in {formatMs(d.ready_ms)}</span>}
                      <span className="truncate">{d.id}</span>
                    </p>
                    {d.error && <p className="mt-1 line-clamp-2 text-[12px] text-danger">{d.error}</p>}
                  </div>
                  <div className="flex shrink-0 gap-1" onClick={(e) => e.stopPropagation()}>
                    {isPending(d.status) && (
                      <button
                        className={btn}
                        disabled={runner.busy !== null}
                        onClick={() => runner.run(`cancel:${d.id}`, () => appsApi.cancel(service, env.name, d.id))}
                        title="Stop this deploy"
                      >
                        <X className="size-3.5" /> Cancel
                      </button>
                    )}
                    {own && d.image && d.status !== "removed" && !isPending(d.status) && (
                      <a
                        href={deploymentUrl(env.url, d.id) ?? undefined}
                        target="_blank"
                        rel="noreferrer"
                        className={`${btn} ${d.status === "ready" ? "" : "opacity-0 group-focus-within:opacity-100 group-hover:opacity-100 max-md:opacity-100"}`}
                        title="This deployment at its own URL, whatever is live"
                      >
                        <ExternalLink className="size-3.5" /> Open
                      </a>
                    )}
                    {!active && !isPending(d.status) && d.image && !d.pruned_at && d.status !== "removed" && (
                      <SureButton
                        className={`${btn} ${d.status === "ready" ? "" : "opacity-0 group-focus-within:opacity-100 group-hover:opacity-100 max-md:opacity-100"}`}
                        disabled={runner.busy !== null}
                        sure={d.status === "ready" ? `Sure? Put it live in ${env.name}` : `Sure? Roll ${env.name} back`}
                        onSure={() => runner.run(`rollback:${d.id}`, () => appsApi.rollback(service, env.name, d.id, keepVars))}
                        title={d.status === "ready" ? "Run this deployment's image, without a build" : keepVars ? "Run this deployment's image again, without a build, with today's variables" : "Run this deployment's image again, without a build, with its variables as they were then"}
                      >
                        {d.status === "ready" ? (
                          <>
                            <Rocket className="size-3.5" /> Put live
                          </>
                        ) : (
                          <>
                            <RotateCcw className="size-3.5" /> Roll back
                          </>
                        )}
                      </SureButton>
                    )}
                  </div>
                </div>
              </li>
            );
          })}
        </ul>
      </section>
      {shown && <BuildLog key={shown.id} service={service} env={env.name} d={shown} />}
    </div>
  );
}

/** The open deployment's steps with their times (its own GET has them); polled while it's on its way. */
function useSteps(service: string, env: string, d: Deployment) {
  const [full, setFull] = useState<Deployment | null>(null);
  const pending = isPending(d.status);
  const visible = usePageVisible();
  useEffect(() => {
    let live = true;
    const load = () => appsApi.deployment(service, env, d.id).then((x) => live && setFull(x), () => {});
    void load();
    // A deploy's steps are ticks on the service's change stream; a slow poll is the safety net.
    const stop = pending && visible ? followChanges(service, { onChange: () => void load() }) : undefined;
    const t = pending && visible ? setInterval(load, 15000) : undefined;
    return () => {
      live = false;
      stop?.();
      if (t) clearInterval(t);
    };
  }, [service, env, d.id, pending, visible]);
  return full ? stepDurations(full) : [];
}

/** A repo deployment's commits over the live one (Render's deploy view), read once it's opened. */
function useCommits(service: string, env: string, d: Deployment) {
  const [c, setC] = useState<DeployCommits | null>(null);
  useEffect(() => {
    if (!d.revision) return setC(null);
    let live = true;
    appsApi.commits(service, env, d.id).then((x) => live && setC(x), () => live && setC(null));
    return () => {
      live = false;
    };
  }, [service, env, d.id, d.revision, d.status]);
  return c;
}

function Commits({ c }: { c: DeployCommits }) {
  if (!c.commits || !c.since) return null;
  if (!c.commits.length) return <p className="mb-2 text-[12px] text-subtle">The same commit as {shortSha(c.since.revision)}.</p>;
  return (
    <details className="mb-2 text-[12px]">
      <summary className="cursor-pointer text-muted">
        {c.commits.length}
        {c.complete ? "" : "+"} {c.commits.length === 1 ? "commit" : "commits"} since {shortSha(c.since.revision)}
        {!c.complete && " (it isn't in this one's history: a rollback, or another branch)"}
      </summary>
      <ul className="mt-1.5 space-y-1">
        {c.commits.map((x) => (
          <li key={x.sha} className="flex min-w-0 gap-2">
            <span className="shrink-0 font-mono text-[11.5px] text-subtle">{shortSha(x.sha)}</span>
            <span className="min-w-0 flex-1 truncate">{x.summary}</span>
            <span className="shrink-0 text-subtle">{x.author}</span>
          </li>
        ))}
      </ul>
    </details>
  );
}

function BuildLog({ service, env, d }: { service: string; env: string; d: Deployment }) {
  const { lines, error } = useBuildLog(service, env, d);
  const steps = useSteps(service, env, d);
  const commits = useCommits(service, env, d);
  return (
    <section className="min-w-0">
      <div className="mb-2 flex items-center gap-2">
        <h3 className="text-[13px] font-medium">Build log</h3>
        <DeployBadge status={d.status} className="text-[11.5px]" />
        <span className="ml-auto truncate font-mono text-[11px] text-subtle" title={d.image ?? undefined}>
          {d.image ?? ""}
        </span>
      </div>
      {commits && <Commits c={commits} />}
      {steps.length > 0 && (
        <ol className="mb-2 flex flex-wrap items-center gap-x-2 gap-y-1 text-[11.5px]" aria-label="Steps">
          {steps.map((s, i) => (
            <li key={s.label} className="inline-flex items-center gap-1.5">
              {i > 0 && <span className="text-subtle">→</span>}
              <span className={s.label === "Failed" ? "text-danger" : "text-muted"}>{s.label}</span>
              {s.ms !== null && <span className="font-mono text-subtle tabular-nums">{formatMs(s.ms)}</span>}
            </li>
          ))}
        </ol>
      )}
      <LogView
        lines={lines}
        className="h-[460px]"
        empty={error ? <span className="text-danger">{error}</span> : isPending(d.status) ? <span className="shimmer">Waiting for the build…</span> : "This deployment has no build log (an image runs as it is)."}
      />
      {d.plan && (
        <div className="mt-3 rounded-lg border border-line bg-panel p-3">
          <PlanDetails plan={d.plan} />
        </div>
      )}
    </section>
  );
}
