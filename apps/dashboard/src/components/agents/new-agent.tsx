"use client";

import { ChevronDown, Folder, FolderPlus, Server } from "lucide-react";
import { useMemo, useState, type ReactNode } from "react";
import { isBusy, type Machine, type Project } from "@/lib/agents-api";
import { input } from "../ui";
import { AddMachinePaths } from "./add-machine";
import { Composer } from "./composer";
import { NewProject } from "./project-new";
import { MachineIcon, StatusDot } from "./shared";
import { useAgents } from "./use-agents";

const LAST_PROJECT = "roll:agents:last-project";

/** Where a new agent starts by default: the project asked for, else the last one used, else the one with the newest thread. */
export function defaultProject<P extends Project>(projects: readonly P[], threads: readonly { project: string; updated_at: number }[], wanted?: string, last?: string | null): P | undefined {
  const by = (id: string | null | undefined) => (id ? projects.find((p) => p.id === id) : undefined);
  const newest = [...threads].sort((a, b) => b.updated_at - a.updated_at)[0];
  return by(wanted) ?? by(last) ?? by(newest?.project) ?? projects[0];
}

/**
 * A new agent: pick where it works (machine, project), say what to do, and it opens as a
 * conversation. With no machine yet, the ways to connect one.
 */
export function NewAgent({ project: wanted, start, end }: { project?: string; start?: ReactNode; end?: ReactNode }) {
  const { client, openThread, focus } = useAgents();
  const data = client.data;
  const machines = useMemo(() => [...data.machines.values()].map((r) => r.value).sort((a, b) => Number(b.online) - Number(a.online) || a.name.localeCompare(b.name)), [data.machines]);
  // With the machine each one is on (the record's, not the project's own).
  const projects = useMemo(() => [...data.projects.values()].map((r) => ({ ...r.value, machine: r.machine })), [data.projects]);
  const threads = useMemo(() => [...data.threads.values()].map((r) => r.value), [data.threads]);
  const [picked, setPicked] = useState<string | null>(() => {
    let last: string | null = null;
    try {
      last = localStorage.getItem(LAST_PROJECT);
    } catch {}
    return defaultProject(projects, threads, wanted, last)?.id ?? null;
  });
  const project = projects.find((p) => p.id === picked) ?? defaultProject(projects, threads, wanted);
  const [machineId, setMachineId] = useState<string | null>(null);
  const machine = machines.find((m) => m.id === (machineId ?? project?.machine)) ?? machines.find((m) => m.online) ?? machines[0];
  const here = projects.filter((p) => p.machine === machine?.id).sort((a, b) => a.name.localeCompare(b.name));
  const inProject = project && project.machine === machine?.id ? project : here[0];
  const [adding, setAdding] = useState(false);
  const recent = threads
    .filter((t) => t.archived_at === null)
    .sort((a, b) => Number(isBusy(b.status)) - Number(isBusy(a.status)) || b.updated_at - a.updated_at)
    .slice(0, 4);

  const choose = (p: Project) => {
    setPicked(p.id);
    try {
      localStorage.setItem(LAST_PROJECT, p.id);
    } catch {}
  };

  return (
    <div className="flex h-full min-h-0 flex-col bg-bg">
      <div className="flex h-12 shrink-0 items-center gap-1.5 border-b border-line px-2 sm:px-3" data-drag-region>
        {start}
        <h2 className="pl-1 text-[13.5px] font-semibold tracking-[-0.02em]">New agent</h2>
        <span className="ml-auto flex items-center gap-1" data-nodrag>
          {end}
        </span>
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto overscroll-contain">
        <div className="mx-auto flex w-full max-w-[700px] flex-col px-4 pt-[min(14vh,120px)] pb-10 sm:px-6">
          {machines.length === 0 ? (
            <div className="space-y-5 text-center">
              <h1 className="text-[26px] leading-tight font-semibold tracking-[-0.045em]">Connect a machine first</h1>
              <p className="mx-auto max-w-md text-[13.5px] text-muted">Agents run on your own computer or on a sandbox roll hosts. Pick one and come back here.</p>
              <AddMachinePaths onCreated={(m) => focus(`m:${m.id}`)} />
            </div>
          ) : (
            <>
              <h1 className="text-[26px] leading-tight font-semibold tracking-[-0.045em]">What should the agent do?</h1>
              <div className="mt-4 mb-3 flex flex-wrap items-center gap-1.5 text-[12.5px] text-subtle">
                <span>On</span>
                <Chip label="Machine" icon={machine ? <MachineIcon machine={machine} className="size-3.5" /> : <Server className="size-3.5" />} text={machine?.name ?? "a machine"} dot={machine ? (machine.online ? "online" : "offline") : undefined}>
                  <select
                    aria-label="Machine"
                    value={machine?.id ?? ""}
                    onChange={(e) => {
                      setMachineId(e.target.value);
                      setAdding(false);
                      const first = projects.find((p) => p.machine === e.target.value);
                      if (first) choose(first);
                    }}
                    className={`${input} absolute inset-0 cursor-pointer opacity-0`}
                  >
                    {machines.map((m) => (
                      <option key={m.id} value={m.id}>
                        {m.name}
                        {m.online ? "" : " (offline)"}
                      </option>
                    ))}
                  </select>
                </Chip>
                <span>in</span>
                <Chip label="Project" icon={<Folder className="size-3.5" />} text={inProject?.name ?? "no project yet"}>
                  <select
                    aria-label="Project"
                    value={adding ? "+" : (inProject?.id ?? "+")}
                    onChange={(e) => {
                      if (e.target.value === "+") return setAdding(true);
                      setAdding(false);
                      const p = projects.find((x) => x.id === e.target.value);
                      if (p) choose(p);
                    }}
                    className={`${input} absolute inset-0 cursor-pointer opacity-0`}
                  >
                    {here.map((p) => (
                      <option key={p.id} value={p.id}>
                        {p.name}
                      </option>
                    ))}
                    <option value="+">Add a project…</option>
                  </select>
                </Chip>
                {!adding && machine?.online && (
                  <button onClick={() => setAdding(true)} className="inline-flex items-center gap-1 rounded-md px-1.5 py-1 hover:bg-panel-2 hover:text-fg" title="Add a project on this machine">
                    <FolderPlus className="size-3.5" />
                  </button>
                )}
              </div>
              {adding && machine ? (
                <div className="rounded-xl border border-line bg-panel p-3">
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
              {recent.length > 0 && (
                <section className="mt-10" aria-label="Recent conversations">
                  <p className="mb-1.5 px-2 text-[11px] tracking-wide text-subtle uppercase">Recent</p>
                  <ul>
                    {recent.map((t) => (
                      <li key={t.id}>
                        <button onClick={() => openThread(t.id)} className="flex w-full items-center gap-2.5 rounded-lg px-2 py-2 text-left hover:bg-panel">
                          <StatusDot status={t.status} className="shrink-0" />
                          <span className="min-w-0 flex-1 truncate text-[13px]">{t.title}</span>
                          <span className="shrink-0 truncate text-[11.5px] text-subtle">{isBusy(t.status) ? <span className="shimmer">{t.activity ?? "working"}</span> : data.projects.get(t.project)?.value.name}</span>
                        </button>
                      </li>
                    ))}
                  </ul>
                </section>
              )}
            </>
          )}
        </div>
      </div>
    </div>
  );
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

function Chip({ label, icon, text, dot, children }: { label: string; icon: ReactNode; text: string; dot?: "online" | "offline"; children: ReactNode }) {
  return (
    <label className="relative inline-flex items-center gap-1.5 rounded-lg border border-line bg-panel px-2 py-1 text-[12.5px] text-fg hover:border-line-2" title={label}>
      <span className="text-subtle">{icon}</span>
      <span className="max-w-44 truncate">{text}</span>
      {dot && <StatusDot status={dot} />}
      <ChevronDown className="size-3 text-subtle" />
      {children}
    </label>
  );
}
