import { HorizonMark } from "@roll/brand";
import { useNavigate } from "@tanstack/react-router";
import { ArrowLeft, ExternalLink, Moon, Play, RotateCw, Rocket, Sun } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { Link } from "@/components/link";
import {
  appsApi,
  hostOf,
  mainEnvironment,
  PRODUCTION,
  sourceLabel,
  type Deployment,
  type Environment,
  type Overview,
  type Service,
} from "@/lib/apps-api";
import { workspace, useWorkspace } from "@/lib/workspace-store";
import { AppShell } from "../app-shell";
import { btn, Connecting, EmptyPanel, ErrorNote, Offline, primary } from "../ui";
import { Deployments } from "./deployments";
import { DomainsTab } from "./domains";
import { anyPending, POLL_BUSY_MS, POLL_IDLE_MS, useDeployMark } from "./live";
import { RuntimeLogs } from "./logs";
import { MetricsTab } from "./metrics";
import { SettingsTab } from "./settings";
import { StateBadge, useRunner } from "./shared";
import { UsageTab } from "./usage";
import { VariablesTab } from "./variables";

export const SERVICE_TABS = ["deployments", "logs", "metrics", "variables", "domains", "usage", "settings"] as const;
export type ServiceTab = (typeof SERVICE_TABS)[number];

const TAB_LABEL: Record<ServiceTab, string> = {
  deployments: "Deployments",
  logs: "Logs",
  metrics: "Metrics",
  variables: "Variables",
  domains: "Domains",
  usage: "Usage",
  settings: "Settings",
};

interface Loaded {
  service: Service;
  env: Environment | null;
  deploys: Deployment[] | null;
}

/** The service, the environment shown, and its deployments; polled quickly while a deploy is on its way. */
function useService(id: string, envName: string | undefined) {
  const [data, setData] = useState<Loaded | null>(null);
  const [state, setState] = useState<"loading" | "ready" | "missing" | "offline">("loading");
  const [poke, setPoke] = useState(0);

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
    let alive = true;
    let timer: ReturnType<typeof setTimeout>;
    const loop = async () => {
      const got = await load();
      if (!alive) return;
      const busy = got && (anyPending([got.service]) || got.deploys?.some((d) => ["queued", "building", "deploying"].includes(d.status)));
      timer = setTimeout(loop, busy ? POLL_BUSY_MS : POLL_IDLE_MS);
    };
    void loop();
    return () => {
      alive = false;
      clearTimeout(timer);
    };
  }, [load, poke]);

  return { data, state, refresh: () => setPoke((n) => n + 1) };
}

/** `/dashboard/apps/$id`: a service, one environment at a time. */
export function ServicePage({ id, env: envParam, tab: tabParam }: { id: string; env?: string; tab?: string }) {
  const navigate = useNavigate();
  const ws = useWorkspace();
  const tab: ServiceTab = SERVICE_TABS.includes(tabParam as ServiceTab) ? (tabParam as ServiceTab) : "deployments";
  const { data, state, refresh } = useService(id, envParam);
  const runner = useRunner(refresh);
  const [overview, setOverview] = useState<Overview | null>(null);
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
  const deploy = () => runner.run("deploy", () => appsApi.deploy(service.id, envName));
  const redeploy = () => runner.run("redeploy", () => appsApi.redeploy(service.id, envName));
  const upload = service.source.kind === "upload";
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
            <button className={`${btn} shrink-0`} disabled={busy || st !== "running"} onClick={() => runner.run("sleep", () => appsApi.sleep(service.id, envName))} title="Scale to zero now">
              <Moon className="size-3.5" /> Sleep
            </button>
          )}
          <button className={`${btn} shrink-0`} disabled={busy || !env?.active} onClick={() => runner.run("restart", () => appsApi.restart(service.id, envName))} title="Replace the instances with fresh ones">
            <RotateCw className="size-3.5" /> Restart
          </button>
          {upload ? (
            <button className={`${primary} !py-1 !text-[12px]`} disabled={busy || !env?.active} onClick={redeploy} title="Run the live image again with the current variables and settings (new code: roll up)">
              <Rocket className="size-3.5" /> Redeploy
            </button>
          ) : (
            <button className={`${primary} !py-1 !text-[12px]`} disabled={busy} onClick={deploy} title="Build the source and roll it out">
              <Rocket className="size-3.5" /> Deploy
            </button>
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
            onChange={(e) => go({ env: e.target.value })}
            className="rounded-lg border border-line bg-panel px-2.5 py-1.5 font-mono text-[12px] text-fg outline-none focus:border-line-2"
          >
            {(service.environments.some((e) => e.name === envName) ? service.environments : [...service.environments, { name: envName }]).map((e) => (
              <option key={e.name} value={e.name}>
                {e.name}
              </option>
            ))}
          </select>
        </label>
      </div>

      <ErrorNote error={runner.error} />

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
          <Deployments service={service.id} env={env!} deploys={data.deploys} runner={runner} />
        ) : tab === "logs" ? (
          <RuntimeLogs service={service.id} env={envName} />
        ) : tab === "metrics" ? (
          <MetricsTab service={service.id} env={envName} />
        ) : tab === "variables" ? (
          <VariablesTab service={service.id} project={service.project} env={env!} runner={runner} onRedeploy={redeploy} />
        ) : tab === "domains" ? (
          <DomainsTab service={service.id} env={env!} runner={runner} />
        ) : tab === "usage" ? (
          <UsageTab service={service.id} env={envName} prices={overview?.prices ?? null} />
        ) : (
          <SettingsTab
            service={service}
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
