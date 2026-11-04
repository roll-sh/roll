"use client";

import { Folder, Server, X } from "lucide-react";
import { useLayoutEffect, useMemo, useRef, useState } from "react";
import type { Machine, Project } from "@/lib/agents-api";
import { AddMachinePaths } from "./add-machine";
import { Composer } from "./composer";
import { NewProject } from "./project-new";
import { MachineIcon } from "./shared";
import { Select } from "./ui";
import { useAgents } from "./use-agents";

const LAST_PROJECT = "roll:agents:last-project";
const ADD = "+";

/** Where a new agent starts by default: the project asked for, else the last one used, else the one with the newest thread. */
export function defaultProject<P extends Project>(projects: readonly P[], threads: readonly { project: string; updated_at: number }[], wanted?: string, last?: string | null): P | undefined {
  const by = (id: string | null | undefined) => (id ? projects.find((p) => p.id === id) : undefined);
  const newest = [...threads].sort((a, b) => b.updated_at - a.updated_at)[0];
  return by(wanted) ?? by(last) ?? by(newest?.project) ?? projects[0];
}

function lastProject(): string | null {
  try {
    return localStorage.getItem(LAST_PROJECT);
  } catch {
    return null;
  }
}

/**
 * The launcher: a small composer over the canvas (T3's new thread, as a popup). Pick the machine
 * and the project (the asked-for one, else the last used), say what to do with the model,
 * permissions and plan mode the project's defaults give, and Enter starts it: the popup closes and
 * the new thread opens beside the canvas. Opened from "New agent", ⌘N / ⌘⇧O, the + on a machine or
 * project card, or a card's handle dropped on the canvas (then it opens where it was dropped).
 * With no machine yet, the ways to connect one.
 */
export function Launcher({ project: wanted, machine: wantedMachine, at, onClose }: { project?: string; machine?: string; at?: { x: number; y: number }; onClose: () => void }) {
  const { client, openThread, focus } = useAgents();
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
    return defaultProject(pool, ts, wanted, lastProject())?.id ?? null;
  });
  const project = projects.find((p) => p.id === picked);
  const machine = machines.find((m) => m.id === (machineId ?? project?.machine)) ?? machines.find((m) => m.online) ?? machines[0];
  const here = projects.filter((p) => p.machine === machine?.id).sort((a, b) => a.name.localeCompare(b.name));
  const inProject = project && project.machine === machine?.id ? project : here[0];
  const [adding, setAdding] = useState<boolean>(() => !!machine?.online && !inProject);

  const choose = (p: Project) => {
    setPicked(p.id);
    try {
      localStorage.setItem(LAST_PROJECT, p.id);
    } catch {
      // Not remembered.
    }
  };

  // Dropped on the canvas: open there, kept inside the surface.
  const panel = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState(at ? { left: at.x - 24, top: at.y - 20 } : null);
  useLayoutEffect(() => {
    const el = panel.current;
    const host = el?.offsetParent as HTMLElement | null;
    if (!at || !el || !host) return;
    const clamp = (v: number, lo: number, hi: number) => Math.max(lo, Math.min(v, Math.max(lo, hi)));
    setPos({ left: clamp(at.x - 24, 12, host.clientWidth - el.offsetWidth - 12), top: clamp(at.y - 20, 56, host.clientHeight - el.offsetHeight - 12) });
  }, [at]);

  return (
    <>
      <div className="fade-in absolute inset-0 z-40 bg-black/25" onMouseDown={onClose} aria-hidden />
      <div
        ref={panel}
        role="dialog"
        aria-label="New agent"
        data-launcher
        className={`sheet-in absolute z-50 w-[min(640px,calc(100%-24px))] rounded-2xl border border-line-2 bg-panel p-2 shadow-[0_24px_64px_-24px_rgb(0_0_0/0.7)] ${pos ? "" : "top-[min(14vh,120px)] left-1/2 -ml-[min(320px,calc(50%-12px))]"}`}
        style={pos ? { left: pos.left, top: pos.top, transformOrigin: "top left" } : { transformOrigin: "top center" }}
        onKeyDown={(e) => {
          if (e.key !== "Escape" || e.defaultPrevented) return;
          // A picker's Escape closes the picker only (its popup is portaled, outside the panel).
          if (!panel.current?.contains(e.target as Node)) return;
          e.preventDefault();
          onClose();
        }}
      >
        <div className="flex items-center gap-0.5 px-1 pb-2 text-[12px] text-subtle">
          <h2 className="pr-1.5 pl-1.5 text-[13px] font-semibold tracking-[-0.02em] text-fg">New agent</h2>
          {machines.length > 0 && (
            <>
              <Select
                label="Machine"
                variant="ghost"
                icon={machine ? <MachineIcon machine={machine} className="size-3.5 text-subtle" /> : <Server className="size-3.5" />}
                value={machine?.id ?? null}
                onChange={(id) => {
                  setMachineId(id);
                  const first = projects.find((p) => p.machine === id);
                  if (first) choose(first);
                  setAdding(!first && !!machines.find((m) => m.id === id)?.online);
                }}
                options={machines.map((m) => ({ value: m.id, label: m.name, hint: m.online ? undefined : "offline" }))}
              />
              <span aria-hidden>/</span>
              <Select
                label="Project"
                variant="ghost"
                icon={<Folder className="size-3.5 text-subtle" />}
                placeholder="No project yet"
                value={adding ? ADD : (inProject?.id ?? null)}
                onChange={(id) => {
                  if (id === ADD) return setAdding(true);
                  setAdding(false);
                  const p = projects.find((x) => x.id === id);
                  if (p) choose(p);
                }}
                options={[...here.map((p) => ({ value: p.id, label: p.name })), ...(machine?.online ? [{ value: ADD, label: "Add a project…" }] : [])]}
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
        ) : adding && machine ? (
          <div className="rounded-xl border border-line bg-bg p-3">
            <NewProject
              machine={machine}
              onClose={() => setAdding(false)}
              onAdded={(id) => {
                setAdding(false);
                setPicked(id);
              }}
            />
          </div>
        ) : inProject && machine ? (
          <Composer
            key={inProject.id}
            mode={{ kind: "new", project: inProject, machine }}
            onSubmit={async (c) => {
              const r = await client.run<{ type: "created"; id: string }>(machine.id, { type: "thread.create", project: inProject.id, ...c }, "created");
              choose(inProject);
              openThread(r.id);
            }}
          />
        ) : (
          <Offline machine={machine} onAdd={() => setAdding(true)} />
        )}
        {inProject && machine && !adding && (
          <p className="flex flex-wrap gap-x-3 px-2 pt-2 text-[11px] text-subtle">
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
      </div>
    </>
  );
}

function Kbd({ children }: { children: string }) {
  return <kbd className="rounded border border-line px-1 font-sans text-[10.5px] text-muted">{children}</kbd>;
}

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
