import { RollLogo } from "@roll/brand";
import { GitBranch, House, KeyRound, Plus, Search } from "lucide-react";
import { useLocation } from "@tanstack/react-router";
import { useEffect, useState, type ReactNode } from "react";
import { Link } from "@/components/link";
import { requestSignIn } from "@/lib/code-token";
import { secretsApi } from "@/lib/secrets-api";
import { hideAgents } from "@/lib/agents-surface";
import { monthOf, projectMonth, usd } from "@/lib/workspace-resources";
import { spendOf, useWorkspace } from "@/lib/workspace-store";
import { useMarkState } from "../mark-state";
import { SidebarAgents, threadsOf, useAgents } from "./agents";
import { openNewProject } from "./new-project";
import { ProjectIcon } from "./project-icon";

const row =
  "flex items-center gap-2.5 rounded-lg border px-2 py-1.5 transition-colors duration-150 motion-reduce:transition-none";

/**
 * The dashboard's sidebar: the projects (their icons, what their agents are doing, this month's
 * spend), the agents working or waiting right now, and the month's spend across everything.
 * Agents open over the page beside it, so the sidebar stays in reach.
 */
export function Sidebar({ onSearch, header }: { onSearch: () => void; header?: ReactNode }) {
  const ws = useWorkspace();
  const { state } = useMarkState();
  const pathname = useLocation({ select: (l) => l.pathname });
  const month = monthOf(new Date());
  const all = ws.projects.flatMap((p) => p.resources);
  const total = spendOf(ws, all);

  return (
    <div className="flex h-full flex-col px-2.5 pt-3 pb-2.5">
      <div className="flex items-center gap-2 px-1.5 pb-2.5">
        <Link href="/dashboard" aria-label="roll home" className="py-0.5">
          <RollLogo state={state} size={17} />
        </Link>
        <span className="ml-auto min-w-0">{header}</span>
      </div>

      <button
        onClick={onSearch}
        className="mb-4 flex h-7 items-center gap-2 rounded-lg border border-line bg-white/[0.015] px-2 text-[12px] text-subtle transition-colors hover:border-line-2 hover:text-muted"
      >
        <Search className="size-3.5" />
        Search or ask…
        <kbd className="ml-auto font-mono text-[10px]">⌘K</kbd>
      </button>

      <SideLink href="/dashboard" icon={<House className="size-3.5" />} active={pathname === "/dashboard"}>
        Home
      </SideLink>
      <SideLink href="/dashboard/code" icon={<GitBranch className="size-3.5" />} active={pathname.startsWith("/dashboard/code")}>
        Code
      </SideLink>
      <SideLink href="/dashboard/access" icon={<KeyRound className="size-3.5" />} active={pathname.startsWith("/dashboard/access")}>
        MCP &amp; access
      </SideLink>

      <Projects pathname={pathname} />
      <SidebarAgents />

      <Link
        href="/dashboard#usage"
        className="mt-auto block rounded-[10px] border border-line bg-sheet p-2.5 inset-shadow-highlight transition-colors hover:border-line-2"
      >
        <div className="flex justify-between text-[11px] text-muted">
          <span>{month.label}</span>
          <span className="font-mono text-subtle">
            day {month.elapsed}/{month.days}
          </span>
        </div>
        <div className="mt-1 flex items-baseline gap-1.5 font-mono text-[17px] font-semibold tracking-[-0.04em]">
          {ws.status === "ready" ? usd(total.usd) : "—"}
          <span className="text-[11px] font-normal tracking-normal text-subtle">so far</span>
        </div>
        <MonthMeter elapsed={month.elapsed} days={month.days} />
        <div className="mt-1.5 flex justify-between text-[11px] text-subtle">
          <span>
            On track for <b className="font-mono font-medium text-fg">≈ {usd(projectMonth(total.usd, month))}</b>
          </span>
          <span>by {month.end}</span>
        </div>
      </Link>
    </div>
  );
}

/** How far through the month: spent (solid) and the rest of the month at that rate (hatched). */
function MonthMeter({ elapsed, days }: { elapsed: number; days: number }) {
  return (
    <div className="relative mt-2 h-1 overflow-hidden rounded-full bg-white/[0.06]">
      <i className="absolute inset-0 rounded-full bg-[repeating-linear-gradient(135deg,rgb(255_255_255/0.25)_0_1.5px,transparent_1.5px_4px)]" />
      <i
        className="absolute inset-y-0 left-0 rounded-full bg-fg shadow-[2px_0_0_var(--sheet)]"
        style={{ width: `${(elapsed / days) * 100}%` }}
      />
    </div>
  );
}

function SideLink({ href, icon, active, children }: { href: string; icon: ReactNode; active: boolean; children: ReactNode }) {
  return (
    <Link
      href={href}
      onClick={() => hideAgents()}
      className={`flex h-7 items-center gap-2 rounded-md px-2 text-[12.5px] transition-colors ${
        active ? "bg-white/[0.05] text-fg" : "text-muted hover:bg-white/[0.03] hover:text-fg"
      }`}
    >
      {icon}
      {children}
    </Link>
  );
}

function Projects({ pathname }: { pathname: string }) {
  const ws = useWorkspace();
  const agents = useAgents();

  return (
    <>
      <div className="mt-4 flex items-center justify-between px-2 pb-1 text-[11px] font-medium text-subtle">
        Projects
        <button
          onClick={openNewProject}
          aria-label="New project"
          title="New project"
          className="grid size-5 place-items-center rounded-md text-muted transition-colors hover:bg-white/[0.06] hover:text-fg"
        >
          <Plus className="size-3.5" />
        </button>
      </div>
      <div className="-mx-0.5 flex min-h-0 flex-col gap-px overflow-y-auto px-0.5 [scrollbar-width:none]">
        {ws.status === "error" ? (
          /401|operator|token/i.test(ws.error ?? "") ? (
            <button onClick={requestSignIn} className="px-2 py-1.5 text-left text-[12px] text-muted hover:text-fg">
              Sign in to see your projects
            </button>
          ) : (
            <p className="px-2 py-1.5 text-[12px] text-subtle">Projects didn't load: {ws.error}</p>
          )
        ) : null}
        {ws.projects.map((p) => {
          const base = `/dashboard/projects/${p.id}`;
          const inside = pathname === base || pathname.startsWith(`${base}/`);
          const active = pathname === base;
          const secrets = pathname === `${base}/secrets`;
          const spend = spendOf(ws, p.resources);
          const staged = ws.staged[p.id] ?? [];
          const repos = new Set(p.resources.filter((r) => r.kind === "repo").map((r) => r.ref));
          const working = agents && repos.size ? threadsOf(agents, repos).filter((t) => t.status !== "idle").length : 0;
          return (
            <div key={p.id}>
            <Link
              href={base}
              onClick={() => hideAgents()}
              className={`${row} ${active ? "border-line-2 bg-raised inset-shadow-highlight" : inside ? "border-transparent bg-white/[0.03]" : "border-transparent hover:bg-white/[0.03]"}`}
            >
              <ProjectIcon project={p} className="size-6" />
              <span className="min-w-0 flex-1 leading-tight">
                <span className="block truncate text-[12.5px] font-medium">{p.name}</span>
                <span className="flex items-center gap-1 truncate text-[10.5px] text-subtle">
                  {working > 0 ? (
                    <>
                      <i className="pulse-dot size-1.5 rounded-full bg-accent" />
                      <span className="text-muted">
                        {working} {working === 1 ? "agent" : "agents"} working
                      </span>
                    </>
                  ) : staged.length ? (
                    <span className="text-accent">{staged.length} staged</span>
                  ) : p.resources.length ? (
                    `${p.resources.length} ${p.resources.length === 1 ? "resource" : "resources"}`
                  ) : (
                    "empty"
                  )}
                </span>
              </span>
              <span className="font-mono text-[10.5px] text-subtle">{p.resources.length ? usd(spend.usd) : ""}</span>
            </Link>
            {inside && (
              <Link
                href={`${base}/secrets`}
                onClick={() => hideAgents()}
                className={`fade-in mt-px ml-[1.375rem] flex h-6 items-center gap-1.5 rounded-md border-l border-line-2 pl-3 text-[12px] transition-colors ${
                  secrets ? "text-fg" : "text-muted hover:text-fg"
                }`}
              >
                <KeyRound className={`size-3 ${secrets ? "text-accent" : ""}`} />
                Secrets
                <SecretsToFix project={p.id} at={pathname} />
              </Link>
            )}
            </div>
          );
        })}
        {ws.status === "ready" && !ws.projects.length && (
          <button
            onClick={openNewProject}
            className="rounded-lg border border-dashed border-line-2 px-2 py-2 text-left text-[12px] text-muted transition-colors hover:border-line-3 hover:text-fg"
          >
            Make your first project
          </button>
        )}
      </div>
    </>
  );
}

/** How many of a project's secrets findings need fixing now (expired, missing, exposed), looked at
 *  again as the page changes; nothing while it's none or can't be read. */
function SecretsToFix({ project, at }: { project: string; at: string }) {
  const [n, setN] = useState(0);
  useEffect(() => {
    let live = true;
    secretsApi(project)
      .health()
      .then(
        (found) => live && setN(found.filter((f) => f.issue === "expired" || f.issue === "missing" || f.issue === "exposed").length),
        () => live && setN(0),
      );
    return () => {
      live = false;
    };
  }, [project, at]);
  if (!n) return null;
  return (
    <span className="ml-auto mr-1 grid h-4 min-w-4 place-items-center rounded-full bg-danger/15 px-1 font-mono text-[10px] text-danger tabular-nums" title={`${n} to fix now: open Health`}>
      {n}
    </span>
  );
}
