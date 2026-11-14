import { HorizonMark } from "@roll/brand";
import { useNavigate } from "@tanstack/react-router";
import { ArrowLeft, ChevronDown, ExternalLink, Moon, Play, RotateCw, Rocket, Sun } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { Link } from "@/components/link";
import {
  appsApi,
  followChanges,
  hostOf,
  mainEnvironment,
  PRODUCTION,
  SERVICE_NAME,
  sourceLabel,
  type Deployment,
  type Environment,
  type NewDeploy,
  type Overview,
  type Service,
} from "@/lib/apps-api";
import { workspace, useWorkspace } from "@/lib/workspace-store";
import { AppShell } from "../app-shell";
import { btn, Connecting, EmptyPanel, ErrorNote, input, Offline, primary } from "../ui";
import { Deployments } from "./deployments";
import { DomainsTab } from "./domains";
import { JobsTab } from "./jobs";
import { ShellTab } from "./shell";
import { anyPending, POLL_BUSY_MS, POLL_IDLE_MS, POLL_STREAMED_MS, useDeployMark, usePageVisible } from "./live";
import { RuntimeLogs } from "./logs";
import { MetricsTab } from "./metrics";
import { SettingsTab } from "./settings";
import { clock, StateBadge, SureButton, until, useRunner } from "./shared";
import { UsageTab } from "./usage";
import { VariablesTab } from "./variables";

export const SERVICE_TABS = ["deployments", "logs", "metrics", "variables", "jobs", "shell", "domains", "usage", "settings"] as const;
export type ServiceTab = (typeof SERVICE_TABS)[number];

const TAB_LABEL: Record<ServiceTab, string> = {
  deployments: "Deployments",
  logs: "Logs",
  metrics: "Metrics",
  variables: "Variables",
  jobs: "Jobs",
  shell: "Shell",
  domains: "Domains",
  usage: "Usage",
  settings: "Settings",
};

interface Loaded {
  service: Service;
  env: Environment | null;
  deploys: Deployment[] | null;
}

/**
 * The service, the environment shown, and its deployments: read again on each of the service's
 * changes (`…/changes`), and polled only as a safety net while that stream is up; without it,
 * polled quickly while a deploy is on its way.
 */
function useService(id: string, envName: string | undefined) {
  const [data, setData] = useState<Loaded | null>(null);
  const [state, setState] = useState<"loading" | "ready" | "missing" | "offline">("loading");
  const [poke, setPoke] = useState(0);
  const [streamed, setStreamed] = useState(false);
  const visible = usePageVisible();

  const load = useCallback(async (): Promise<Loaded | null> => {
    try {
      const service = await appsApi.service(id);
      const name = envName ?? mainEnvironment(service)?.name;
      const [env, deploys] = name
        ? await Promise.all([appsApi.environment(id, name).catch(() => null), appsApi.deploys(id, name).catch(() => [] as Deployment[])])
        : [null, []];
      const next = { service, env, deploys };
      setData(next);
      setState("ready");
      return next;
    } catch (e) {
      const text = e instanceof Error ? e.message : String(e);
      // rolld answers a missing service with a JSON error; fetch failing means it's not there at all.
      setState(e instanceof TypeError ? "offline" : /^no service |not found|^404/i.test(text) ? "missing" : "offline");
      return null;
    }
  }, [id, envName]);

  useEffect(() => {
    if (!visible) return;
    let alive = true;
    let timer: ReturnType<typeof setTimeout>;
    const loop = async () => {
      const got = await load();
      if (!alive) return;
      const busy = got && (anyPending([got.service]) || got.deploys?.some((d) => ["queued", "building", "deploying"].includes(d.status)));
      timer = setTimeout(loop, streamed ? POLL_STREAMED_MS : busy ? POLL_BUSY_MS : POLL_IDLE_MS);
    };
    void loop();
    return () => {
      alive = false;
      clearTimeout(timer);
    };
  }, [load, poke, visible, streamed]);

  // A burst of ticks (a deploy's steps) is one read.
  useEffect(() => {
    if (!visible) return;
    let t: ReturnType<typeof setTimeout> | undefined;
    const stop = followChanges(id, {
      onOpen: setStreamed,
      onChange: () => {
        clearTimeout(t);
        t = setTimeout(() => void load(), 250);
      },
    });
    return () => {
      clearTimeout(t);
      stop();
      setStreamed(false);
    };
  }, [id, load, visible]);

  return { data, state, refresh: () => setPoke((n) => n + 1) };
}

const NEW_ENV = "\u0000new";

/** A new environment: its name, for a repo the branch that deploys to it on every push, and
 *  the environment whose variables it starts with. */
function NewEnvironment({
  repo,
  envs,
  busy,
  onCancel,
  onCreate,
}: {
  repo: boolean;
  envs: string[];
  busy: boolean;
  onCancel: () => void;
  onCreate: (name: string, branch: string | null, variablesFrom: string | null, everyService: boolean) => void;
}) {
  const [name, setName] = useState("");
  const [branch, setBranch] = useState("");
  const [every, setEvery] = useState(false);
  const [from, setFrom] = useState(envs.includes(PRODUCTION) ? PRODUCTION : "");
  const ok = SERVICE_NAME.test(name);
  return (
    <form
      className="rise mt-3 flex flex-wrap items-end gap-2 rounded-xl border border-line bg-panel px-4 py-3"
      onSubmit={(e) => {
        e.preventDefault();
        if (ok) onCreate(name, branch.trim() || null, from || null, every);
      }}
    >
      <label className="flex flex-col gap-1 text-[12px] text-muted">
        Name
        <input autoFocus value={name} onChange={(e) => setName(e.target.value.toLowerCase())} placeholder="staging" spellCheck={false} className={`${input} w-40 font-mono text-[12px]`} />
      </label>
      {repo && (
        <label className="flex flex-col gap-1 text-[12px] text-muted">
          Deploys on pushes to
          <input value={branch} onChange={(e) => setBranch(e.target.value)} placeholder="a branch (optional)" spellCheck={false} className={`${input} w-48 font-mono text-[12px]`} />
        </label>
      )}
      {envs.length > 0 && (
        <label className="flex flex-col gap-1 text-[12px] text-muted">
          Variables from
          <select value={from} onChange={(e) => setFrom(e.target.value)} className={`${input} w-40 font-mono text-[12px]`}>
            <option value="">none</option>
            {envs.map((e) => (
              <option key={e} value={e}>
                {e}
              </option>
            ))}
          </select>
        </label>
      )}
      <label className="inline-flex items-center gap-1.5 pb-2 text-[12px] text-muted" title="One environment for the whole project: each of its services gets it">
        <input type="checkbox" checked={every} onChange={(e) => setEvery(e.target.checked)} />
        Every service in the project
      </label>
      <button type="submit" className={primary} disabled={!ok || busy}>
        Create
      </button>
      <button type="button" className={btn} onClick={onCancel}>
        Cancel
      </button>
    </form>
  );
}

/** `/dashboard/apps/$id`: a service, one environment at a time. */
export function ServicePage({ id, env: envParam, tab: tabParam }: { id: string; env?: string; tab?: string }) {
  const navigate = useNavigate();
  const ws = useWorkspace();
  const tab: ServiceTab = SERVICE_TABS.includes(tabParam as ServiceTab) ? (tabParam as ServiceTab) : "deployments";
  const { data, state, refresh } = useService(id, envParam);
  const runner = useRunner(refresh);
  const [overview, setOverview] = useState<Overview | null>(null);
  const [adding, setAdding] = useState(false);
  // Deploy's options: a revision (Render's deploy a specific commit), no traffic (Vercel's
  // preview, then promote), without the cache.
  const [opts, setOpts] = useState(false);
  const [rev, setRev] = useState("");
  const [noTraffic, setNoTraffic] = useState(false);
  const [noCache, setNoCache] = useState(false);
  useEffect(() => {
    appsApi.overview().then(setOverview, () => {});
  }, []);
  useDeployMark(data ? [data.service] : null);

  const go = (next: { env?: string; tab?: ServiceTab }) => {
    const e = next.env ?? envParam;
    const t = next.tab ?? tab;
    const q = new URLSearchParams();
    if (e && e !== PRODUCTION) q.set("env", e);
    if (t !== "deployments") q.set("tab", t);
    const s = q.toString();
    runner.setError(null);
    void navigate({ href: `/dashboard/apps/${encodeURIComponent(id)}${s ? `?${s}` : ""}`, replace: true });
  };

  const service = data?.service;
  const project = service ? ws.projects.find((p) => p.id === service.project) : undefined;
  const crumb = (
    <>
      {project ? (
        <Link href={`/dashboard/projects/${encodeURIComponent(project.id)}`} className="text-muted hover:text-fg">
          {project.name}
        </Link>
      ) : (
        <span className="text-muted">apps</span>
      )}{" "}
      <span className="text-subtle">/</span> {service?.name ?? id}
    </>
  );

  if (state === "offline" && !data)
    return (
      <AppShell crumb={crumb}>
        <Offline />
      </AppShell>
    );
  if (state === "missing" || (state !== "loading" && !service))
    return (
      <AppShell crumb={crumb}>
        <EmptyPanel>
          <HorizonMark state="sleep" size={40} />
          <h2 className="text-[15px] font-medium">No service {id}</h2>
          <p className="max-w-md text-[13px] text-muted">It may have been deleted, or the link is wrong.</p>
          <Link href="/dashboard" className={btn}>
            <ArrowLeft className="size-3.5" /> Projects
          </Link>
        </EmptyPanel>
      </AppShell>
    );
  if (!data || !service)
    return (
      <AppShell crumb={crumb}>
        <Connecting />
      </AppShell>
    );

  const env = data.env;
  const envName = env?.name ?? envParam ?? mainEnvironment(service)?.name ?? PRODUCTION;
  const st = env?.state ?? "none";
  const asleep = st === "stopped" || st === "suspended";
  const localUrl = env?.port && typeof window !== "undefined" ? `http://${window.location.hostname}:${env.port}` : null;
  const deploy = (body: NewDeploy = {}) => runner.run("deploy", () => appsApi.deploy(service.id, envName, body));

  const redeploy = () => runner.run("redeploy", () => appsApi.redeploy(service.id, envName));
  const upload = service.source.kind === "upload";
  // What a crash's "Roll back" goes to: the newest deployment that was live before this one.
  const previous = data.deploys?.find((d) => d.status === "inactive" && d.id !== env?.active?.id && d.image && !d.pruned_at) ?? null;
  const busy = runner.busy !== null;

  return (
    <AppShell
      crumb={crumb}
      actions={
        <>
          {service.run.type === "cron" && (
            <button className={`${btn} shrink-0`} disabled={busy || !env?.active} onClick={() => runner.run("run", () => appsApi.runNow(service.id, envName))} title="Start a run of the live deployment now">
              <Play className="size-3.5" /> Run now
            </button>
          )}
          {asleep ? (
            <button className={`${btn} shrink-0`} disabled={busy || !env?.active} onClick={() => runner.run("wake", () => appsApi.wake(service.id, envName))} title="Start an instance now">
              <Sun className="size-3.5" /> Wake
            </button>
          ) : (
            <SureButton className={`${btn} shrink-0`} disabled={busy || st !== "running"} sure="Sure? Stop it now" onSure={() => runner.run("sleep", () => appsApi.sleep(service.id, envName))} title="Scale to zero now">
              <Moon className="size-3.5" /> Sleep
            </SureButton>
          )}
          <SureButton className={`${btn} shrink-0`} disabled={busy || !env?.active} sure="Sure? Restart" onSure={() => runner.run("restart", () => appsApi.restart(service.id, envName))} title="Replace the instances with fresh ones">
            <RotateCw className="size-3.5" /> Restart
          </SureButton>
          {upload ? (
            <button className={`${primary} !py-1 !text-[12px]`} disabled={busy || !env?.active} onClick={redeploy} title="Run the live image again with the current variables and settings (new code: roll up)">
              <Rocket className="size-3.5" /> Redeploy
            </button>
          ) : (
            <span className="inline-flex">
              <button className={`${primary} !rounded-r-none !py-1 !text-[12px]`} disabled={busy} onClick={() => deploy()} title="Build the source and roll it out">
                <Rocket className="size-3.5" /> Deploy
              </button>
              <button className={`${primary} !rounded-l-none border-l border-bg/20 !px-1.5 !py-1 !text-[12px]`} disabled={busy} aria-expanded={opts} aria-label="Deploy options" title="A revision, no traffic, without the cache" onClick={() => setOpts((o) => !o)}>
                <ChevronDown className="size-3.5" />
              </button>
            </span>
          )}
        </>
      }
    >
      <div className="flex flex-wrap items-end justify-between gap-4">
        <div className="min-w-0">
          <h1 className="truncate text-2xl font-semibold tracking-[-0.04em]">{service.name}</h1>
          <p className="mt-1 flex flex-wrap items-center gap-x-2 gap-y-1 text-[13px] text-muted">
            <StateBadge state={st} />
            {env && env.instances.length > 0 && (
              <>
                <span className="text-subtle">·</span>
                <span className="font-mono text-[12px]">
                  {env.instances.length} {env.instances.length === 1 ? "instance" : "instances"}
                </span>
              </>
            )}
            <span className="text-subtle">·</span>
            <span className="truncate font-mono text-[12px] text-subtle">{sourceLabel(service.source)}</span>
            <span className="text-subtle">·</span>
            <span className="font-mono text-[12px] text-subtle">{service.run.type}</span>
            {env?.next_run_at && (
              <>
                <span className="text-subtle">·</span>
                <span className="text-[12px] text-muted" title={new Date(env.next_run_at).toISOString()}>
                  next run {clock(env.next_run_at, true)} ({until(env.next_run_at)})
                </span>
              </>
            )}
          </p>
          <p className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-1 text-[13px]">
            {env?.url && (
              <a href={env.url} target="_blank" rel="noreferrer" className="inline-flex items-center gap-1 font-mono text-[12.5px] text-accent hover:underline">
                {hostOf(env.url)} <ExternalLink className="size-3" />
              </a>
            )}
            {localUrl && (
              <a href={localUrl} target="_blank" rel="noreferrer" className="inline-flex items-center gap-1 font-mono text-[12.5px] text-muted hover:text-fg" title="The service's own port on this machine">
                :{env!.port} <ExternalLink className="size-3" />
              </a>
            )}
          </p>
        </div>
        <label className="flex items-center gap-2 text-[12px] text-subtle">
          Environment
          <select
            value={envName}
            onChange={(e) => (e.target.value === NEW_ENV ? setAdding(true) : go({ env: e.target.value }))}
            className="rounded-lg border border-line bg-panel px-2.5 py-1.5 font-mono text-[12px] text-fg outline-none focus:border-line-2"
          >
            {(service.environments.some((e) => e.name === envName) ? service.environments : [...service.environments, { name: envName }]).map((e) => (
              <option key={e.name} value={e.name}>
                {e.name}
              </option>
            ))}
            <option value={NEW_ENV}>+ New environment…</option>
          </select>
        </label>
      </div>
      {opts && !upload && (
        <form
          className="rise mt-3 flex flex-wrap items-end gap-3 rounded-xl border border-line bg-panel px-4 py-3"
          onSubmit={async (e) => {
            e.preventDefault();
            const body: NewDeploy = { ...(rev.trim() ? { revision: rev.trim() } : {}), ...(noTraffic ? { no_traffic: true } : {}), ...(noCache ? { no_cache: true } : {}) };
            if (await deploy(body)) {
              setOpts(false);
              setRev("");
            }
          }}
        >
          {service.source.kind === "repo" && (
            <label className="flex flex-col gap-1 text-[12px] text-muted">
              Revision
              <input value={rev} onChange={(e) => setRev(e.target.value)} placeholder="the branch's head" spellCheck={false} className={`${input} w-56 font-mono text-[12px]`} />
            </label>
          )}
          <label className="inline-flex items-center gap-1.5 pb-2 text-[12px] text-muted" title="Built and health-checked at its own URL; put it live from the deployments list">
            <input type="checkbox" checked={noTraffic} onChange={(e) => setNoTraffic(e.target.checked)} />
            No traffic (try it first)
          </label>
          <label className="inline-flex items-center gap-1.5 pb-2 text-[12px] text-muted" title="No cached layer; base images pulled again">
            <input type="checkbox" checked={noCache} onChange={(e) => setNoCache(e.target.checked)} />
            Without the cache
          </label>
          <button type="submit" className={primary} disabled={busy}>
            Deploy
          </button>
          <button type="button" className={btn} onClick={() => setOpts(false)}>
            Cancel
          </button>
        </form>
      )}
      {adding && (
        <NewEnvironment
          repo={service.source.kind === "repo"}
          envs={service.environments.map((e) => e.name)}
          busy={busy}
          onCancel={() => setAdding(false)}
          onCreate={async (name, branch, variablesFrom, everyService) => {
            const body = { branch, ...(variablesFrom ? { variables_from: variablesFrom } : {}) };
            const made = await runner.run("environment", async () => {
              if (!everyService) return appsApi.putEnvironment(service.id, name, body);
              // Each service's own call (each allowed or refused on its own): what didn't get it is said.
              const all = await appsApi.services(service.project);
              const done = await Promise.allSettled(all.map((s) => appsApi.putEnvironment(s.id, name, body)));
              const failed = done.flatMap((r, i) => (r.status === "rejected" ? [`${all[i].name}: ${r.reason instanceof Error ? r.reason.message : String(r.reason)}`] : []));
              if (failed.length) throw new Error(`${failed.length} of ${all.length} services didn't get ${name}: ${failed.join("; ")}`);
              return done;
            });
            if (made) {
              setAdding(false);
              go({ env: name });
            }
          }}
        />
      )}

      <ErrorNote error={runner.error} />
      {env?.crash_loop && (
        <div role="alert" className="rise mt-4 flex flex-wrap items-center gap-x-3 gap-y-1 rounded-xl border border-danger/40 bg-danger/[0.06] px-4 py-3 text-[13px]">
          <span className="font-medium text-danger">Crashing</span>
          <span className="min-w-0 flex-1 text-muted">
            {env.crash_loop.crashes} times in the last 10 minutes; the last at {clock(env.crash_loop.last_at)}, because {env.crash_loop.last_error}.
          </span>
          <button type="button" className={btn} onClick={() => go({ tab: "logs" })}>
            See the logs
          </button>
          {previous && (
            <button
              type="button"
              className={btn}
              disabled={busy}
              title="Run the deployment live before this one again (its image: no build, its variables)"
              onClick={() => runner.run("rollback", () => appsApi.rollbackPrevious(service.id, envName))}
            >
              <RotateCw className="size-3.5" /> Roll back
            </button>
          )}
        </div>
      )}

      {env?.canary && (
        <div role="status" className="rise mt-4 flex flex-wrap items-center gap-x-3 gap-y-1 rounded-xl border border-line bg-panel px-4 py-3 text-[13px]">
          <span className="flex items-center gap-2 font-medium">
            <span className="size-1.5 animate-pulse rounded-full bg-accent motion-reduce:animate-none" />
            Rolling out
          </span>
          <span className="min-w-0 flex-1 text-muted">
            {env.canary.deployment.slice(-6)} takes {env.canary.share}% of requests; it rolls back by itself if it answers more 5xx than allowed.
          </span>
          <button type="button" className={btn} disabled={busy} onClick={() => runner.run("cancel", () => appsApi.cancel(service.id, envName, env.canary!.deployment))}>
            Roll back
          </button>
        </div>
      )}

      <nav className="mt-6 flex gap-1 overflow-x-auto border-b border-line [scrollbar-width:none]" aria-label="Service">
        {SERVICE_TABS.map((t) => (
          <button
            key={t}
            onClick={() => go({ tab: t })}
            aria-current={t === tab ? "page" : undefined}
            className={`relative shrink-0 px-2.5 pt-1 pb-2.5 text-[13px] transition-colors ${t === tab ? "text-fg" : "text-muted hover:text-fg"}`}
          >
            {TAB_LABEL[t]}
            {t === "variables" && env?.needs_deploy && <i className="ml-1.5 inline-block size-1.5 rounded-full bg-accent align-middle" aria-label="needs a deploy" />}
            <span className={`absolute inset-x-1.5 -bottom-px h-[1.5px] rounded-full bg-fg transition-opacity duration-150 ${t === tab ? "opacity-100" : "opacity-0"}`} />
          </button>
        ))}
      </nav>

      <div key={`${tab}:${envName}`} className="rise mt-5">
        {!env && tab !== "settings" ? (
          <p className="text-[13px] text-muted">
            {service.source.kind === "upload" ? (
              <>
                Nothing's deployed to {envName} yet. From the app's directory, run <code className="font-mono text-fg">roll up -s {service.name}</code>.
              </>
            ) : (
              <>Nothing's deployed to {envName} yet. Deploy builds the source and rolls it out here.</>
            )}
          </p>
        ) : tab === "deployments" ? (
          <Deployments service={service.id} env={env!} deploys={data.deploys} runner={runner} own={service.run.type === "web" && !service.run.volume} />
        ) : tab === "logs" ? (
          <RuntimeLogs service={service.id} env={envName} />
        ) : tab === "metrics" ? (
          <MetricsTab service={service.id} env={envName} memoryMb={service.run.memory_mb} />
        ) : tab === "variables" ? (
          <VariablesTab service={service.id} project={service.project} env={env!} envs={service.environments.map((e) => e.name).filter((n) => n !== env!.name)} runner={runner} onRedeploy={redeploy} />
        ) : tab === "jobs" ? (
          <JobsTab service={service.id} env={envName} runner={runner} cron={service.run.type === "cron"} />
        ) : tab === "shell" ? (
          <ShellTab service={service.id} env={envName} />
        ) : tab === "domains" ? (
          <DomainsTab service={service.id} env={env!} runner={runner} />
        ) : tab === "usage" ? (
          <UsageTab service={service.id} env={envName} prices={overview?.prices ?? null} />
        ) : (
          <SettingsTab
            service={service}
            env={env}
            prices={overview?.prices ?? null}
            onEnvDeleted={() => go({ env: PRODUCTION })}
            projectName={project?.name ?? service.project}
            runner={runner}
            onDeleted={() => {
              workspace.forget("service", service.id);
              void navigate({ href: project ? `/dashboard/projects/${encodeURIComponent(project.id)}` : "/dashboard" });
            }}
          />
        )}
      </div>
    </AppShell>
  );
}
