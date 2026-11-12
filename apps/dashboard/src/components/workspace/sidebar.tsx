import { RollLogo } from "@roll/brand";
import { Bot, GitBranch, House, Plus, Search } from "lucide-react";
import { useLocation, useNavigate } from "@tanstack/react-router";
import { useState, type ReactNode } from "react";
import { Link } from "@/components/link";
import { requestSignIn } from "@/lib/code-token";
import { PROJECT_NAME } from "@/lib/workspace-api";
import { monthOf, projectMonth, usd } from "@/lib/workspace-resources";
import { spendOf, useWorkspace, workspace } from "@/lib/workspace-store";
import { useMarkState } from "../mark-state";
import { MiniMap } from "./kind";

const row =
  "flex items-center gap-2.5 rounded-lg border px-2 py-1.5 transition-colors duration-150 motion-reduce:transition-none";

/**
 * The dashboard's sidebar: the projects (each a miniature of its canvas, with this month's
 * spend), a new project inline, and the month's spend across all of them.
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
      <SideLink href="/dashboard/agents" icon={<Bot className="size-3.5" />} active={pathname.startsWith("/dashboard/agents")}>
        Agents
      </SideLink>
      <SideLink href="/dashboard/code" icon={<GitBranch className="size-3.5" />} active={pathname.startsWith("/dashboard/code")}>
        Code
      </SideLink>

      <Projects pathname={pathname} />

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
  const navigate = useNavigate();
  const [adding, setAdding] = useState(false);

  return (
    <>
      <div className="mt-4 flex items-center justify-between px-2 pb-1 text-[11px] font-medium text-subtle">
        Projects
        <button
          onClick={() => setAdding(true)}
          aria-label="New project"
          title="New project"
          className="grid size-5 place-items-center rounded-md text-muted transition-colors hover:bg-white/[0.06] hover:text-fg"
        >
          <Plus className="size-3.5" />
        </button>
      </div>
      <div className="-mx-0.5 flex min-h-0 flex-col gap-0.5 overflow-y-auto px-0.5 [scrollbar-width:none]">
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
          const active = pathname === `/dashboard/projects/${p.id}`;
          const spend = spendOf(ws, p.resources);
          const staged = ws.staged[p.id] ?? [];
          return (
            <Link
              key={p.id}
              href={`/dashboard/projects/${p.id}`}
              className={`${row} ${
                active ? "border-line-2 bg-raised inset-shadow-highlight" : "border-transparent hover:bg-white/[0.03]"
              }`}
            >
              <MiniMap project={p} staged={staged} />
              <span className="min-w-0 flex-1 leading-tight">
                <span className="block truncate text-[12.5px] font-medium">{p.name}</span>
                <span className="block truncate font-mono text-[10.5px] text-subtle">
                  {p.resources.length ? `${p.resources.length} ${p.resources.length === 1 ? "resource" : "resources"}` : "empty"}
                  {staged.length ? <span className="text-accent"> · {staged.length} staged</span> : null}
                </span>
              </span>
              <span className="font-mono text-[11px] text-muted">{p.resources.length ? usd(spend.usd) : ""}</span>
            </Link>
          );
        })}
        {adding && (
          <NewProject
            onDone={(id) => {
              setAdding(false);
              if (id) navigate({ href: `/dashboard/projects/${id}` });
            }}
          />
        )}
        {ws.status === "ready" && !ws.projects.length && !adding && (
          <button
            onClick={() => setAdding(true)}
            className="rounded-lg border border-dashed border-line-2 px-2 py-2 text-left text-[12px] text-muted transition-colors hover:border-line-3 hover:text-fg"
          >
            Make your first project
          </button>
        )}
      </div>
    </>
  );
}

function NewProject({ onDone }: { onDone: (id?: string) => void }) {
  const [name, setName] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const submit = async () => {
    const n = name.trim().toLowerCase();
    if (!n) return onDone();
    if (!PROJECT_NAME.test(n)) return setError("Lowercase letters, digits and dashes");
    setBusy(true);
    try {
      onDone((await workspace.createProject(n)).id);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setBusy(false);
    }
  };
  return (
    <div className="rise rounded-lg border border-line-2 bg-raised p-1.5 inset-shadow-highlight">
      <input
        autoFocus
        value={name}
        disabled={busy}
        placeholder="project-name"
        onChange={(e) => {
          setName(e.target.value);
          setError(null);
        }}
        onKeyDown={(e) => {
          if (e.key === "Enter") void submit();
          if (e.key === "Escape") onDone();
        }}
        onBlur={() => !name.trim() && onDone()}
        className="w-full bg-transparent px-1 font-mono text-[12px] outline-none placeholder:text-subtle"
      />
      {error && <p className="px-1 pt-1 text-[11px] text-danger">{error}</p>}
    </div>
  );
}
