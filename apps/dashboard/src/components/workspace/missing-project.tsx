import { ArrowUpRight, Plus } from "lucide-react";
import { Link } from "@/components/link";
import { loadWorkspace, useWorkspace, type WorkspaceState } from "@/lib/workspace-store";
import { AppShell } from "../app-shell";
import { openNewProject } from "./new-project";
import { ProjectIcon } from "./project-icon";

const btn =
  "inline-flex h-7 items-center gap-1.5 rounded-md border border-line-2 bg-raised px-2.5 text-[12px] font-medium inset-shadow-highlight transition-colors hover:border-line-3";
const primary = "inline-flex h-7 items-center gap-1.5 rounded-md bg-fg px-2.5 text-[12px] font-medium text-bg shadow-card inset-shadow-highlight transition-opacity hover:opacity-90";

/** How many of the user's projects a missing one's page offers to open instead. */
const PICK = 5;

/**
 * A project page whose project isn't in the workspace (deleted, or a link from another roll): the
 * way out. `to` is the same page in another project (its canvas, its secrets).
 */
export function MissingProject({ crumb, to }: { crumb: string; to: (id: string) => string }) {
  const ws = useWorkspace();
  return (
    <AppShell crumb={ws.status === "loading" ? "" : crumb} bleed>
      <NoSuchProject ws={ws} to={to} />
    </AppShell>
  );
}

export function NoSuchProject({ ws, to }: { ws: WorkspaceState; to: (id: string) => string }) {
  if (ws.status === "loading") return null;
  if (ws.status === "error")
    return (
      <div className="grid h-full place-items-center px-4 text-center text-[13px] text-muted">
        <div>
          <p>Projects didn&apos;t load: {ws.error}</p>
          <button onClick={() => void loadWorkspace()} className={`${btn} mt-3 text-fg`}>
            Try again
          </button>
        </div>
      </div>
    );
  const some = ws.projects.slice(0, PICK);
  return (
    <div className="rise mx-auto w-full max-w-[420px] px-4 py-20 text-center">
      <h1 className="text-[18px] font-semibold tracking-[-0.045em]">There&apos;s no such project</h1>
      <p className="mt-1.5 text-[12.5px] text-muted">
        It may have been deleted, or the link is to another roll.{some.length > 0 ? " Pick one of yours:" : " Make one to start."}
      </p>
      {some.length > 0 && (
        <ul className="mt-5 divide-y divide-line rounded-xl border border-line-2 bg-sheet text-left inset-shadow-highlight">
          {some.map((p) => (
            <li key={p.id}>
              <Link href={to(p.id)} className="group flex items-center gap-2.5 px-3 py-2 transition-colors hover:bg-fg/[0.03]">
                <ProjectIcon project={p} className="size-5" />
                <span className="min-w-0 flex-1 truncate text-[12.5px] font-medium">{p.name}</span>
                <ArrowUpRight className="size-3.5 text-subtle transition-colors group-hover:text-fg" />
              </Link>
            </li>
          ))}
        </ul>
      )}
      <div className="mt-5 flex justify-center gap-2">
        <Link href="/dashboard" className={btn}>
          {ws.projects.length > PICK ? `All ${ws.projects.length} projects` : "All projects"}
        </Link>
        <button onClick={openNewProject} className={some.length ? btn : primary}>
          <Plus className="size-3.5" /> New project
        </button>
      </div>
    </div>
  );
}
