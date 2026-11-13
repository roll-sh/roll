import { useNavigate } from "@tanstack/react-router";
import { ArrowLeftRight, ChevronRight, HeartPulse, KeyRound, LayoutGrid, Lock, RefreshCw, ScrollText, Settings2, ShieldCheck } from "lucide-react";
import { createContext, useContext, useMemo, type ReactNode } from "react";
import { Link } from "@/components/link";
import { secretsApi, type Environment, type Link as SecretLink, type Required, type SecretsApi } from "@/lib/secrets-api";
import { stringifySearch, type Search } from "@/lib/search";
import type { WorkspaceProject } from "@/lib/workspace-api";
import { resourceKey } from "@/lib/workspace-resources";
import { useWorkspace } from "@/lib/workspace-store";
import { AppShell } from "../app-shell";
import { ProjectIcon } from "../workspace/project-icon";
import { AccessView } from "./access";
import { AuditView } from "./audit";
import { CompareView } from "./compare";
import { EnvView } from "./env-view";
import { EnvironmentsView } from "./environments";
import { FindKey } from "./find";
import { HealthView } from "./health";
import { OverviewGrid } from "./grid";
import { SyncsView } from "./syncs";
import { ErrorNote, useLoad } from "./ui";

export type View = "overview" | "health" | "compare" | "environments" | "access" | "syncs" | "audit";

type Ctx = {
  project: WorkspaceProject;
  api: SecretsApi;
  /** The standard environments in order, then the ephemeral ones. */
  envs: Environment[];
  standard: Environment[];
  previews: Environment[];
  reloadEnvs: () => void;
  /** How long old values are kept, in days (null: for good). */
  historyDays: number | null;
  search: Search;
  /** Goes to the surface with these search params (`env`, `path`, `view`, `key`, `tab`…). */
  go: (next: Search, replace?: boolean) => void;
  /** A linked resource's name on the canvas, else its ref. */
  resourceName: (l: SecretLink) => string;
  /** The project's required keys (typed ones say what a value must be). */
  required: Required[];
  reloadRequired: () => void;
};

const SecretsCtx = createContext<Ctx | null>(null);

export function useSecrets(): Ctx {
  const c = useContext(SecretsCtx);
  if (!c) throw new Error("useSecrets outside the Secrets surface");
  return c;
}

export const secretsHref = (project: string, search: Search = {}) => `/dashboard/projects/${project}/secrets${stringifySearch(search)}`;

/** `/dashboard/projects/$id/secrets`: the project's secrets and environment variables (docs/secrets.md). */
export function SecretsPage({ id, search }: { id: string; search: Search }) {
  const ws = useWorkspace();
  const project = ws.projects.find((p) => p.id === id);
  if (!project)
    return (
      <AppShell crumb={ws.status === "loading" ? "" : "Secrets"} bleed>
        <div className="grid h-full place-items-center text-[13px] text-muted">
          {ws.status === "loading" ? "" : ws.status === "error" ? `Projects didn't load: ${ws.error}` : "There's no such project."}
        </div>
      </AppShell>
    );
  return <Surface key={project.id} project={project} search={search} />;
}

function Surface({ project, search }: { project: WorkspaceProject; search: Search }) {
  const navigate = useNavigate();
  const ws = useWorkspace();
  const api = useMemo(() => secretsApi(project.id), [project.id]);
  const overview = useLoad(() => api.overview(), [api]);
  const required = useLoad(() => api.required(), [api]);
  const envs = overview.data?.environments ?? [];
  const standard = envs.filter((e) => e.kind === "standard").sort((a, b) => a.position - b.position);
  const previews = envs.filter((e) => e.kind === "ephemeral");

  const ctx: Ctx = {
    project,
    api,
    envs,
    standard,
    previews,
    reloadEnvs: overview.reload,
    historyDays: overview.data?.history_days ?? null,
    search,
    go: (next, replace) => navigate({ href: secretsHref(project.id, next), replace }),
    resourceName: (l) => ws.inventory.get(resourceKey({ kind: l.kind as never, ref: l.ref }))?.name ?? l.ref,
    required: required.data ?? [],
    reloadRequired: required.reload,
  };

  const env = search.env ? envs.find((e) => e.slug === search.env) : undefined;
  const view: View = (["health", "compare", "environments", "access", "syncs", "audit"] as const).find((v) => v === search.view) ?? "overview";

  let body: ReactNode;
  if (overview.error && !overview.data) body = <ErrorNote error={`Secrets didn't load: ${overview.error}`} className="m-4" />;
  else if (!overview.data) body = null;
  else if (search.env && !env) body = <ErrorNote error={`There's no environment "${search.env}" in ${project.name}.`} className="m-4" />;
  else if (env) body = <EnvView key={env.slug} env={env} />;
  else if (view === "health") body = <HealthView />;
  else if (view === "compare") body = <CompareView />;
  else if (view === "environments") body = <EnvironmentsView />;
  else if (view === "access") body = <AccessView />;
  else if (view === "syncs") body = <SyncsView />;
  else if (view === "audit") body = <AuditView />;
  else body = <OverviewGrid />;

  return (
    <SecretsCtx.Provider value={ctx}>
      <AppShell
        bleed
        crumb={
          <>
            <Link href={`/dashboard/projects/${project.id}`} className="flex min-w-0 items-center gap-2 rounded-md py-0.5 pr-1.5 pl-0.5 transition-colors hover:bg-white/[0.05]" title="The project's canvas">
              <ProjectIcon project={project} className="size-5" />
              <span className="truncate">{project.name}</span>
            </Link>
            <ChevronRight className="size-3.5 shrink-0 text-faint" />
            <Link href={secretsHref(project.id)} className="flex items-center gap-1.5 rounded-md px-1 py-0.5 transition-colors hover:bg-white/[0.05]">
              <KeyRound className="size-3.5 text-muted" />
              Secrets
            </Link>
          </>
        }
        actions={
          <>
            <FindKey />
            <Link href={`/dashboard/projects/${project.id}`} className="flex h-7 items-center gap-1.5 rounded-md px-2 text-[12px] text-muted transition-colors hover:bg-white/[0.05] hover:text-fg">
              <LayoutGrid className="size-3.5" />
              Canvas
            </Link>
          </>
        }
      >
        <div className="absolute inset-0 flex flex-col">
          <ViewsBar env={env?.slug ?? null} view={env ? null : view} />
          <div className="relative min-h-0 flex-1 overflow-y-auto">{body}</div>
        </div>
      </AppShell>
    </SecretsCtx.Provider>
  );
}

const tab = (on: boolean) =>
  `flex h-7 shrink-0 items-center gap-1.5 rounded-md border px-2.5 text-[12.5px] font-[510] tracking-[-0.01em] transition-colors duration-150 ${
    on ? "border-line-2 bg-panel-2 text-fg shadow-card inset-shadow-highlight" : "border-transparent text-muted hover:bg-fg/[0.04] hover:text-fg"
  }`;

/** Overview, one tab per standard environment, then the project-wide views. */
function ViewsBar({ env, view }: { env: string | null; view: View | null }) {
  const { standard, previews, search, project } = useSecrets();
  const path = search.path && search.path !== "/" ? search.path : undefined;
  const side: { view: View; label: string; icon: ReactNode }[] = [
    { view: "health", label: "Health", icon: <HeartPulse className="size-3.5" /> },
    { view: "compare", label: "Compare", icon: <ArrowLeftRight className="size-3.5" /> },
    { view: "environments", label: "Environments", icon: <Settings2 className="size-3.5" /> },
    { view: "access", label: "Access", icon: <ShieldCheck className="size-3.5" /> },
    { view: "syncs", label: "Syncs", icon: <RefreshCw className="size-3.5" /> },
    { view: "audit", label: "Audit", icon: <ScrollText className="size-3.5" /> },
  ];
  return (
    <nav className="flex h-11 shrink-0 items-center gap-1 overflow-x-auto border-b border-line px-2 [scrollbar-width:none]" aria-label="Secrets views">
      <Link href={secretsHref(project.id, { path })} className={tab(view === "overview")}>
        Overview
      </Link>
      <span className="mx-1 h-4 w-px shrink-0 bg-line-2" />
      {standard.map((e) => (
        <Link key={e.slug} href={secretsHref(project.id, { env: e.slug, path })} className={tab(env === e.slug)} title={`${e.name} (${e.slug})`}>
          {e.protected && <Lock className="size-3 text-muted" aria-label="protected" />}
          {e.name}
          <span className="grid h-4 min-w-4 place-items-center rounded-full bg-fg/[0.06] px-1 font-mono text-[10px] text-muted tabular-nums">{e.secrets}</span>
        </Link>
      ))}
      {previews.length > 0 && (
        <Link href={secretsHref(project.id, { view: "environments" })} className={`${tab(false)} text-subtle`} title="Preview environments (ephemeral)">
          +{previews.length} {previews.length === 1 ? "preview" : "previews"}
        </Link>
      )}
      <span className="ml-auto" />
      {side.map((s) => (
        <Link key={s.view} href={secretsHref(project.id, { view: s.view })} className={tab(view === s.view)} title={s.label}>
          {s.icon}
          <span className="max-lg:hidden">{s.label}</span>
        </Link>
      ))}
    </nav>
  );
}
