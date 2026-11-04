import { FolderPlus, Server, X } from "lucide-react";
import { useLayoutEffect, useMemo, useRef, useState } from "react";
import type { Machine, Project } from "@/lib/agents-api";
import { AddMachinePaths } from "./add-machine";
import { Composer } from "./composer";
import { openProjectDialog } from "@/lib/agents-project-dialogs";
import { readiness } from "@/lib/agents-setup";
import { ProviderSetup } from "./provider-setup";
import { MachineIcon, ProjectIcon } from "./identity";
import { Select } from "./ui";
import { DialogSheet, DialogTitle } from "./ui/dialog";
import { defaultProject, lastProject, rememberProject, usableIn, useStartThread } from "./new-thread";
import { useAgents } from "./use-agents";

const ADD = "+";

/**
 * The launcher: a small composer over the canvas (T3's new thread, as a popup). Pick the machine
 * and the project (the asked-for one, else the last used), say what to do with the model,
 * permissions and plan mode the project's defaults give, and Enter starts it: the popup closes and
 * the new thread opens beside the canvas. Opened from "New agent", ⌘N / ⌘⇧O, the + on a machine or
 * project card, or a card's handle dropped on the canvas (then it opens where it was dropped).
 * With no machine yet, the ways to connect one.
 */
export function Launcher({
  project: wanted,
  machine: wantedMachine,
  at,
  onClose,
  open = true,
  onExited,
  container,
}: {
  project?: string;
  machine?: string;
  at?: { x: number; y: number };
  onClose: () => void;
  /** False while it plays its way out (kept mounted by `useKept`). */
  open?: boolean;
  onExited?: () => void;
  /** The agents surface: a drop point is in its coordinates. */
  container?: HTMLElement | null;
}) {
  // Dropped on the canvas: open there, kept inside the surface. Asked for somewhere else while it's
  // open (another + or drop), it glides there and starts over inside (it replayed its entrance).
  const panel = useRef<HTMLDivElement | null>(null);
  const [pos, setPos] = useState(at ? { left: at.x - 24, top: at.y - 20 } : null);
  useLayoutEffect(() => {
    const el = panel.current?.parentElement;
    const host = el?.offsetParent as HTMLElement | null;
    if (!at) return setPos(null);
    if (!el || !host) return;
    const clamp = (v: number, lo: number, hi: number) => Math.max(lo, Math.min(v, Math.max(lo, hi)));
    setPos({ left: clamp(at.x - 24, 12, host.clientWidth - el.offsetWidth - 12), top: clamp(at.y - 20, 56, host.clientHeight - el.offsetHeight - 12) });
  }, [at]);
  return (
    // The shared dialog's frame (Base UI): focus taken, kept inside and given back; Escape (a
    // picker's closes the picker only) and a press outside close it; it shrinks back and fades,
    // inert, before it unmounts. Portaled into the surface, so a drop point is where it opens.
    <DialogSheet
      open={open}
      onClose={onClose}
      onExited={onExited}
      label="New agent"
      container={container}
      backdropClassName={`${container ? "absolute" : "fixed"} inset-0 z-40 bg-black/25`}
      viewportClassName={`${container ? "absolute" : "fixed"} inset-0 z-50`}
      className={`absolute w-[min(640px,calc(100%-24px))] rounded-2xl border border-line-2 bg-panel p-2 shadow-[0_24px_64px_-24px_rgb(0_0_0/0.7)] ${pos ? "" : "top-[min(14vh,120px)] left-1/2 -ml-[min(320px,calc(50%-12px))]"}`}
      // Retargeted, it glides to the new point on the same curve.
      style={pos ? { left: pos.left, top: pos.top, transformOrigin: "24px 20px", transitionProperty: "scale, opacity, left, top" } : undefined}
    >
      <div ref={panel} data-launcher>
        <LauncherBody key={`${wanted ?? ""}|${wantedMachine ?? ""}|${at?.x ?? ""},${at?.y ?? ""}`} project={wanted} machine={wantedMachine} onClose={onClose} />
      </div>
    </DialogSheet>
  );
}

function LauncherBody({ project: wanted, machine: wantedMachine, onClose }: { project?: string; machine?: string; onClose: () => void }) {
  const { client, focus } = useAgents();
  const start = useStartThread();
  const data = client.data;
  const machines = useMemo(() => [...data.machines.values()].map((r) => r.value).sort((a, b) => Number(b.online) - Number(a.online) || a.name.localeCompare(b.name)), [data.machines]);
  // With the machine each one is on (the record's, not the project's own).
  const projects = useMemo(() => [...data.projects.values()].map((r) => ({ ...r.value, machine: r.machine })), [data.projects]);
  const threads = useMemo(() => [...data.threads.values()].map((r) => r.value), [data.threads]);

  const [machineId, setMachineId] = useState<string | null>(() => wantedMachine ?? null);
  const [picked, setPicked] = useState<string | null>(() => {
    // On a machine asked for, only its projects count.
    const pool = wantedMachine ? projects.filter((p) => p.machine === wantedMachine) : projects;
    const ts = wantedMachine ? threads.filter((t) => pool.some((p) => p.id === t.project)) : threads;
    return defaultProject(pool, ts, wanted, lastProject(), usableIn(new Map(machines.map((m) => [m.id, m]))))?.id ?? null;
  });
  const project = projects.find((p) => p.id === picked);
  const machine = machines.find((m) => m.id === (machineId ?? project?.machine)) ?? machines.find((m) => m.online) ?? machines[0];
  const here = projects.filter((p) => p.machine === machine?.id).sort((a, b) => a.name.localeCompare(b.name));
  const inProject = project && project.machine === machine?.id ? project : here[0];
  // Adding a project is the one dialog (project-new.tsx); its "Start an agent" comes back here.
  const addProject = () => machine && openProjectDialog({ kind: "add", machine: machine.id });

  const choose = (p: Project) => {
    setPicked(p.id);
    rememberProject(p.id);
  };

  return (
    <>
        <div className="flex items-center gap-0.5 px-1 pb-2 text-[12px] text-subtle">
          <DialogTitle className="pr-1.5 pl-1.5 text-[13px] font-semibold tracking-[-0.02em] text-fg">New agent</DialogTitle>
          {machines.length > 0 && (
            <>
              <Select
                label="Machine"
                variant="ghost"
                icon={machine ? <MachineIcon machine={machine} className="size-3.5" /> : <Server className="size-3.5" />}
                value={machine?.id ?? null}
                onChange={(id) => {
                  setMachineId(id);
                  const first = projects.find((p) => p.machine === id);
                  if (first) choose(first);
                }}
                options={machines.map((m) => ({ value: m.id, label: m.name, hint: m.online ? "online" : m.sandbox ? "asleep" : "offline", icon: <MachineIcon machine={m} className="size-3.5" /> }))}
              />
              <span aria-hidden>/</span>
              <Select
                label="Project"
                variant="ghost"
                icon={inProject ? <ProjectIcon project={inProject} className="size-3.5" /> : <FolderPlus className="size-3.5 text-subtle" />}
                placeholder="No project yet"
                value={inProject?.id ?? null}
                onChange={(id) => {
                  if (id === ADD) return void addProject();
                  const p = projects.find((x) => x.id === id);
                  if (p) choose(p);
                }}
                options={[...here.map((p) => ({ value: p.id, label: p.name, icon: <ProjectIcon project={p} className="size-3.5" /> })), ...(machine?.online ? [{ value: ADD, label: "Add a project…", icon: <FolderPlus className="size-3.5 text-subtle" /> }] : [])]}
              />
            </>
          )}
          <button type="button" onClick={onClose} aria-label="Close (Esc)" title="Close (Esc)" className="ml-auto grid size-7 place-items-center rounded-md hover:bg-fg/[0.06] hover:text-fg">
            <X className="size-4" />
          </button>
        </div>
        {machines.length === 0 ? (
          <div className="space-y-4 px-2 pt-2 pb-3 text-center">
            <h3 className="text-[20px] leading-tight font-semibold tracking-[-0.045em]">Connect a machine first</h3>
            <p className="mx-auto max-w-md text-[13px] text-muted">Agents run on your own computer or on a sandbox roll hosts.</p>
            <AddMachinePaths
              onCreated={(m) => {
                onClose();
                focus(`m:${m.id}`);
              }}
            />
          </div>
        ) : machine && needsSetup(machine) ? (
          // Nothing could run the prompt there yet: the next step instead of a composer that fails.
          <ProviderSetup machine={machine} className="mx-1" />
        ) : inProject && machine ? (
          <Composer
            key={inProject.id}
            mode={{ kind: "new", project: inProject, machine }}
            onSubmit={(c, opts) => start(machine, { type: "thread.create", project: inProject.id, ...c }, undefined, opts?.open !== false)}
          />
        ) : (
          <Offline machine={machine} onAdd={addProject} />
        )}
        {inProject && machine && !needsSetup(machine) && (
          // Keys, for a keyboard: not on a touch screen.
          <p className="flex flex-wrap gap-x-3 px-2 pt-2 text-[11px] text-subtle pointer-coarse:hidden">
            <span>
              <Kbd>Enter</Kbd> starts it
            </span>
            <span>
              <Kbd>⇧ Enter</Kbd> new line
            </span>
            <span>
              <Kbd>⇧ Tab</Kbd> plan first
            </span>
            <span className="ml-auto">
              <Kbd>Esc</Kbd> close
            </span>
          </p>
        )}
    </>
  );
}

function Kbd({ children }: { children: string }) {
  return <kbd className="rounded border border-line px-1 font-sans text-[10.5px] text-muted">{children}</kbd>;
}

/** Online, but no provider installed or none logged in. */
const needsSetup = (m: Machine) => m.online && ["none", "login"].includes(readiness(m).state);

function Offline({ machine, onAdd }: { machine: Machine | undefined; onAdd: () => void }) {
  return (
    <div className="rounded-xl border border-dashed border-line-2 px-4 py-6 text-center text-[13px] text-muted">
      {!machine?.online ? (
        <>
          {machine?.name ?? "The machine"} is offline. Start it with <code className="font-mono text-fg">roll agent up</code>.
        </>
      ) : (
        <>
          No project on {machine.name} yet.{" "}
          <button onClick={onAdd} className="text-accent hover:underline">
            Add one
          </button>
          .
        </>
      )}
    </div>
  );
}
