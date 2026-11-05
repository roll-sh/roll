"use client";

/**
 * Projects without a card (docs/code/agents.md, "Canvas"): a project with nothing on the canvas
 * isn't drawn (`collapseUnused`); its machine says "3 more projects" in one line, their tiles
 * stacked, which opens on hover or a click into a list. Each row starts an agent there (the row
 * itself), pins the project onto the view so it keeps its card, or opens its settings. A project
 * gets its card back as soon as it has a thread. Linear folds empty groups the same way ("3 hidden
 * groups"); T3's sidebar lists every project but only the ones with threads take room.
 */

import { ChevronRight, FolderPlus, Pin, Plus, Settings2 } from "lucide-react";
import { createContext, useContext, useState, type ReactNode } from "react";
import { reachable, type Machine, type Project } from "@/lib/agents-api";
import { setAgentsView } from "@/lib/agents-surface";
import { ProjectIcon } from "./identity";
import { shortPath } from "./shared";
import { Popover, PopoverPopup, PopoverTrigger } from "./ui";
import { useAgents } from "./use-agents";

export interface CanvasProjects {
  /** Per machine id, its projects that aren't drawn. */
  unused: ReadonlyMap<string, readonly string[]>;
  /** Whether a project is pinned onto the view. */
  kept: (project: string) => boolean;
  /** Pin a project onto the view, or unpin it (absent: read-only, a share). */
  keep?: (project: string, on: boolean) => void;
}

const NONE: CanvasProjects = { unused: new Map(), kept: () => false };
const Ctx = createContext<CanvasProjects>(NONE);

export function CanvasProjectsProvider({ value, children }: { value: CanvasProjects; children: ReactNode }) {
  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}

export const useCanvasProjects = () => useContext(Ctx);

const plural = (n: number, one: string, many: string) => `${n} ${n === 1 ? one : many}`;

/** The machine card's line, and the list it opens; nothing when every project has its card. */
export function MoreProjects({ machine, onAdd }: { machine: Machine; onAdd?: () => void }) {
  const { client } = useAgents();
  const { unused } = useCanvasProjects();
  const [open, setOpen] = useState(false);
  const ids = unused.get(machine.id) ?? [];
  const projects = ids.map((id) => client.data.projects.get(id)?.value).filter((p): p is Project => !!p);
  if (!projects.length) return null;
  // "more" only when some of its projects have cards.
  const drawn = [...client.data.projects.values()].some((r) => r.machine === machine.id && !ids.includes(r.value.id));
  const label = drawn ? plural(projects.length, "more project", "more projects") : plural(projects.length, "project", "projects");
  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger
        openOnHover
        delay={250}
        closeDelay={180}
        render={
          <button
            type="button"
            data-nodrag
            aria-label={`${label}: ${projects.map((p) => p.name).join(", ")}`}
            className="-mx-1 flex w-[calc(100%+0.5rem)] items-center gap-2 rounded-md px-1 py-1 text-left text-[11.5px] text-subtle transition-colors duration-150 hover:bg-fg/[0.04] hover:text-fg data-popup-open:bg-fg/[0.06] data-popup-open:text-fg"
          />
        }
      >
        <span className="flex shrink-0 -space-x-1" aria-hidden>
          {projects.slice(0, 3).map((p) => (
            <span key={p.id} className="rounded-[5px] bg-panel p-px">
              <ProjectIcon project={p} className="size-3.5" />
            </span>
          ))}
        </span>
        <span className="min-w-0 flex-1 truncate">{label}</span>
        <ChevronRight className="size-3 shrink-0" />
      </PopoverTrigger>
      <PopoverPopup side="right" align="start" sideOffset={10} aria-label={`Projects on ${machine.name}`} className="w-[300px] flex-col p-1 text-[12.5px]">
        <p className="px-2 pt-1 pb-1.5 text-[11px] font-medium text-subtle">
          {drawn ? "No threads on the canvas" : `Projects on ${machine.name}`}
        </p>
        <ul className="max-h-[min(60vh,420px)] overflow-y-auto">
          {projects.map((p) => (
            <ProjectRow key={p.id} project={p} machine={machine} onDone={() => setOpen(false)} />
          ))}
        </ul>
        {onAdd && machine.online && (
          <button type="button" onClick={() => (setOpen(false), onAdd())} className="mt-1 flex h-8 items-center gap-2 rounded-md border-t border-line px-2 text-left text-[12.5px] text-muted transition-colors hover:bg-fg/[0.05] hover:text-fg">
            <FolderPlus className="size-3.5" strokeWidth={1.75} /> Add a project
          </button>
        )}
      </PopoverPopup>
    </Popover>
  );
}

function ProjectRow({ project, machine, onDone }: { project: Project; machine: Machine; onDone: () => void }) {
  const { startNew } = useAgents();
  const { kept, keep } = useCanvasProjects();
  const online = reachable(machine);
  const settings = () => (onDone(), setAgentsView({ kind: "settings", target: `p:${project.id}` }));
  const start = startNew && online ? () => (onDone(), startNew(project.id)) : null;
  const where = project.repo ? `${project.repo.owner}/${project.repo.repo}` : shortPath(project.path, machine.info?.home);
  return (
    <li className="group/r flex items-center rounded-md transition-colors duration-100 hover:bg-fg/[0.06] has-focus-visible:bg-fg/[0.06]">
      <button
        type="button"
        onClick={start ?? settings}
        title={start ? `Start an agent in ${project.name}` : `${project.name} settings`}
        className="flex min-w-0 flex-1 items-center gap-2 py-1.5 pl-2 text-left outline-none"
      >
        <ProjectIcon project={project} className="size-5" />
        <span className="min-w-0 flex-1">
          <span className="block truncate text-[12.5px] text-fg">{project.name}</span>
          <span className="block truncate font-mono text-[10.5px] text-subtle">{where}</span>
        </span>
      </button>
      <span className="flex shrink-0 items-center gap-0.5 pr-1 opacity-0 transition-opacity duration-150 group-hover/r:opacity-100 group-has-focus-visible/r:opacity-100 [@media(hover:none)]:opacity-100">
        {start && (
          <RowTool label={`New agent in ${project.name}`} onClick={start}>
            <Plus />
          </RowTool>
        )}
        {keep && (
          <RowTool label={kept(project.id) ? "Unpin from the canvas" : "Show on the canvas"} onClick={() => keep(project.id, !kept(project.id))}>
            <Pin />
          </RowTool>
        )}
        <RowTool label="Project settings" onClick={settings}>
          <Settings2 />
        </RowTool>
      </span>
    </li>
  );
}

function RowTool({ label, onClick, children }: { label: string; onClick: () => void; children: ReactNode }) {
  return (
    <button
      type="button"
      onClick={onClick}
      title={label}
      aria-label={label}
      className="grid size-6 place-items-center rounded-md text-subtle transition-colors hover:bg-fg/[0.08] hover:text-fg [&>svg]:size-3.5 [&>svg]:stroke-[1.75]"
    >
      {children}
    </button>
  );
}
