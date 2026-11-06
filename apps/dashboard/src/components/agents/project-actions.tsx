"use client";

/**
 * What can be done with a project, the same everywhere it shows (docs/code/agents.md,
 * "Projects"): its card's ⋯, its row under a machine's folded projects, its group in the sidebar
 * and ⌘K's "Project: …". `useProjectActions` is the one list; `ProjectDialogs` shows the dialogs
 * they open (one host for the page).
 *
 * "New sandbox for this project" rents a sandbox of a size, waits for its daemon to say hello,
 * clones the project's roll code repo onto it (`project.add` with `clone`), and offers to start
 * an agent there, each step shown as it happens. T3 has the clone half as "Add to another
 * machine" (`AddProjectToMachineDialog.tsx`); roll can bring the machine too.
 */

import { ArrowDownToLine, ArrowUpRight, Archive, Check, Cloud, Focus, KeyRound, Pin, PinOff, Plus, Settings2, Trash2 } from "lucide-react";
import { useEffect, useRef, useState, type ReactNode } from "react";
import { commandId, expectOk, reachable, type Machine, type Project, type PullOutcome, type Reply } from "@/lib/agents-api";
import type { AgentsData } from "@/lib/agents-store";
import { shelfOf } from "@/lib/agents-inbox";
import { closeProjectDialog, openProjectDialog, useProjectDialog } from "@/lib/agents-project-dialogs";
import { openShelf } from "@/lib/agents-shelf";
import { canRunAgents, readiness } from "@/lib/agents-setup";
import { setAgentsView } from "@/lib/agents-surface";
import { formatHourly } from "@/lib/sandboxes-api";
import { btn, input, primary } from "../ui";
import { SizeOption } from "./add-machine";
import { SandboxesProblem } from "./sandbox";
import { useCanvasProjects, type CanvasProjects } from "./machine-projects";
import { AddProjectDialog } from "./project-new";
import type { MenuItem } from "./menu";
import { showInboxToast } from "./threads-sidebar";
import { SlidePages } from "./slide-pages";
import { Dialog, useKept } from "./ui/dialog";
import { openMachineSettings } from "./provider-setup";
import { useAgents, type AgentsCtx } from "./use-agents";

/** How many of a project's conversations are settled, snoozed or archived. */
export function shelvedIn(data: AgentsData, project: string, now = Date.now()): number {
  let n = 0;
  for (const r of data.threads.values()) if (r.value.project === project && r.value.lineage?.relation !== "subagent" && shelfOf(r.value, now) !== "active") n++;
  return n;
}

/** What building a project's actions needs from the surface. */
export type ProjectActionsEnv = Pick<AgentsCtx, "client" | "startNew" | "focusProject" | "demo" | "services" | "readOnly"> & { canvas: CanvasProjects };

/** What a pull did, in a sentence. */
export function pullSentence(name: string, outcome: PullOutcome): string {
  switch (outcome.status) {
    case "pulled":
      return `Pulled ${outcome.commits} commit${outcome.commits === 1 ? "" : "s"} into ${name}`;
    case "up_to_date":
      return `${name} is up to date`;
    case "skipped":
      return `Didn't pull ${name}: ${outcome.message}`;
    case "failed":
      return `Couldn't pull ${name}: ${outcome.message}`;
  }
}

/**
 * A project's actions, in this order: new agent here, new sandbox for it, focus on it only, pin
 * it onto the view (on the canvas), pull, open it in roll code, settings, its settled and archived
 * conversations; `remove` adds "Remove from roll" last.
 */
export function projectActions(env: ProjectActionsEnv, project: Project, opts: { remove?: () => void } = {}): MenuItem[] {
  const { client, startNew, focusProject, demo, services, readOnly, canvas } = env;
  const machineId = client.data.projects.get(project.id)?.machine;
  const machine = machineId ? client.data.machines.get(machineId)?.value : undefined;
  const online = reachable(machine);
  const setup = readiness(machine);
  const kept = canvas.kept(project.id);
  const shelved = shelvedIn(client.data, project.id);
  const pull = async () => {
    if (!machine) return;
    try {
      const r = await client.run<Extract<Reply, { type: "pulled" }>>(machine.id, { type: "project.pull", project: project.id }, "pulled");
      showInboxToast({ text: pullSentence(project.name, r.outcome), error: r.outcome.status === "failed" });
    } catch (e) {
      showInboxToast({ text: `Couldn't pull ${project.name}: ${e instanceof Error ? e.message : String(e)}`, error: true });
    }
  };
  const items: (MenuItem | false)[] = [
    !!startNew && !readOnly && { label: "New agent here", icon: <Plus />, disabled: !canRunAgents(machine), onSelect: () => startNew(project.id) },
    // Online but nothing to run an agent with: the step that fixes it, right under.
    !readOnly && !!machine && (setup.state === "none" || setup.state === "login") && {
      label: setup.state === "none" ? `Set up a provider on ${machine.name}…` : `Log in to a provider on ${machine.name}…`,
      icon: <KeyRound />,
      onSelect: () => openMachineSettings(machine.id, "Providers"),
    },
    !!services && !readOnly && !!project.repo && { label: "New sandbox for this project", icon: <Cloud />, onSelect: () => openProjectDialog({ kind: "sandbox", project: project.id }) },
    !!focusProject && { label: "Focus on this project only", icon: <Focus />, onSelect: () => focusProject(project.id) },
    !!canvas.keep && { label: kept ? "Unpin from the canvas" : "Pin to the canvas", icon: kept ? <PinOff /> : <Pin />, onSelect: () => canvas.keep?.(project.id, !kept) },
    !readOnly && !!project.git && { label: "Pull", icon: <ArrowDownToLine />, disabled: !online, onSelect: () => void pull() },
    !!project.repo && !demo && { label: "Open in roll code", icon: <ArrowUpRight />, onSelect: () => window.location.assign(`/${project.repo!.owner}/${project.repo!.repo}`) },
    { label: "Settings", icon: <Settings2 />, onSelect: () => setAgentsView({ kind: "settings", target: `p:${project.id}` }) },
    shelved > 0 && { label: `Settled and archived (${shelved})`, icon: <Archive />, onSelect: () => openShelf(project.id) },
    !!opts.remove && !readOnly && { label: "Remove from roll", confirm: "Remove? (the folder stays)", icon: <Trash2 />, danger: true, disabled: !online, onSelect: opts.remove },
  ];
  return items.filter((i): i is MenuItem => !!i);
}

/** `projectActions` for a component. */
export function useProjectActions(project: Project, opts: { remove?: () => void } = {}): MenuItem[] {
  const ctx = useAgents();
  const canvas = useCanvasProjects();
  return projectActions({ ...ctx, canvas }, project, opts);
}

/** The page's project dialogs (add a project, a new sandbox for one); mount once. */
export function ProjectDialogs() {
  const { client } = useAgents();
  // Kept while it animates shut; each opening starts fresh (its own key).
  const kept = useKept(useProjectDialog());
  const shown = kept.value;
  if (!shown) return null;
  if (shown.kind === "add") return <AddProjectDialog key={kept.key} open={kept.open} onExited={kept.onExited} machine={shown.machine} onClose={closeProjectDialog} />;
  const project = client.data.projects.get(shown.project)?.value;
  if (!project) return null;
  return (
    <Dialog key={kept.key} open={kept.open} onExited={kept.onExited} onClose={closeProjectDialog} title={`New sandbox for ${project.name}`} lead={`A machine roll hosts for you, with ${project.repo ? `${project.repo.owner}/${project.repo.repo}` : project.name} cloned and ready for an agent.`}>
      <SandboxForProject project={project} />
    </Dialog>
  );
}

type Phase =
  | { at: "form" }
  | { at: "renting" }
  | { at: "starting"; machine: Machine; since: number }
  | { at: "cloning"; machine: Machine; since: number }
  | { at: "ready"; machine: Machine; project: string }
  | { at: "failed"; error: string; step: 0 | 1 | 2; machine?: Machine };

/** How long a new sandbox has to say hello before the clone is given up on. */
export const CONNECT_WAIT_MS = 3 * 60_000;

function SandboxForProject({ project }: { project: Project }) {
  const { client, owner, services, sandboxes, startNew } = useAgents();
  const list = sandboxes?.list ?? null;
  const [size, setSize] = useState<string | null>(null);
  const [name, setName] = useState(`${project.name}-sandbox`);
  const [phase, setPhase] = useState<Phase>({ at: "form" });
  // The clone's idempotency key: a retry after a timeout gets the daemon's receipt.
  const [cloneId] = useState(commandId);
  const chosen = list?.sizes.find((s) => s.name === size) ?? list?.sizes[0];
  const full = list ? list.sandboxes.length >= list.max : false;
  const repo = project.repo!;

  // Waits for the daemon's hello, then clones; closing the dialog stops waiting (the sandbox stays).
  const alive = useRef(true);
  const stopWait = useRef<() => void>(() => {});
  useEffect(() => {
    // Set again on every mount (development mounts twice).
    alive.current = true;
    return () => {
      alive.current = false;
      stopWait.current();
    };
  }, []);
  const show = (p: Phase) => alive.current && setPhase(p);
  const waitThenClone = (machine: string) => {
    let started = false;
    const clone = async (m: Machine) => {
      show({ at: "cloning", machine: m, since: Date.now() });
      try {
        const reply = await client.send(m.id, { type: "project.add", path: "", clone: { owner: repo.owner, repo: repo.repo } }, cloneId);
        const created = expectOk<{ type: "created"; id: string }>(reply, "created");
        show({ at: "ready", machine: m, project: created.id });
      } catch (e) {
        show({ at: "failed", error: e instanceof Error ? e.message : String(e), step: 2, machine: m });
      }
    };
    const check = () => {
      const m = client.data.machines.get(machine)?.value;
      if (started || !m?.online || !m.info) return;
      started = true;
      stop();
      void clone(m);
    };
    const timer = setTimeout(() => {
      if (started) return;
      started = true;
      stop();
      show({ at: "failed", step: 1, error: "It didn't connect in time. It's on the canvas: add the project from its card once it's up.", machine: client.data.machines.get(machine)?.value });
    }, CONNECT_WAIT_MS);
    const off = client.subscribe(check);
    const stop = () => {
      off();
      clearTimeout(timer);
    };
    stopWait.current = stop;
    check();
  };

  // The form gives way to the steps, sliding; the dialog eases to fit each change.
  const page = (key: "form" | "progress", node: ReactNode) => (
    <SlidePages page={key} direction={key === "form" ? -1 : 1}>
      {node}
    </SlidePages>
  );
  if (phase.at === "form") {
    if (!services || !list)
      return page("form", sandboxes?.error ? (
        <SandboxesProblem error={sandboxes.error} onRetry={sandboxes.reload} />
      ) : (
        <p className="text-[12.5px] text-subtle">{list === null && services ? <span className="shimmer">Loading sizes…</span> : "Only the owner can rent sandboxes."}</p>
      ));
    if (!list.backend) return page("form", <p className="text-[12.5px] text-muted">Sandboxes aren&apos;t on for this roll server.</p>);
    return page("form",
      <form
        className="flex flex-col gap-3"
        onSubmit={async (e) => {
          e.preventDefault();
          if (!chosen) return;
          setPhase({ at: "renting" });
          try {
            const r = await services.sandboxes.create(owner, { name: name.trim() || undefined, size: chosen.name });
            client.enqueue({ type: "machine", cursor: 0, machine: r.machine });
            sandboxes?.reload();
            setPhase({ at: "starting", machine: r.machine, since: Date.now() });
            waitThenClone(r.machine.id);
          } catch (err) {
            setPhase({ at: "failed", step: 0, error: err instanceof Error ? err.message : String(err) });
          }
        }}
      >
        <fieldset className="min-w-0 space-y-1.5">
          <legend className="sr-only">Size</legend>
          {list.sizes.map((s) => (
            <SizeOption key={s.name} size={s} checked={s.name === chosen?.name} onChange={() => setSize(s.name)} />
          ))}
        </fieldset>
        <input value={name} onChange={(e) => setName(e.target.value)} placeholder="Name (optional)" aria-label="Sandbox name" className={`${input} w-full`} />
        <div className="flex items-center gap-2">
          <button type="submit" className={primary} disabled={full || !chosen}>
            Rent for {formatHourly(chosen?.hourly_cents ?? 0)} and clone
          </button>
          <span className="text-[11.5px] text-subtle">
            {list.sandboxes.length} of {list.max} in use
          </span>
        </div>
        {full && <p className="text-[12px] text-muted">You have as many sandboxes as you can. Delete one to rent another.</p>}
      </form>,
    );
  }

  // Which step it's on (3: all done); a failure stops on its own step.
  const step = phase.at === "renting" ? 0 : phase.at === "starting" ? 1 : phase.at === "cloning" ? 2 : phase.at === "ready" ? 3 : phase.step;
  const stateOf = (i: number): StepState => (i < step ? "done" : i > step ? "next" : phase.at === "failed" ? "failed" : "now");
  const machineName = "machine" in phase && phase.machine ? phase.machine.name : name.trim() || "the sandbox";
  const ready = phase.at === "ready" ? phase : null;
  const loggedIn = ready?.machine && client.data.machines.get(ready.machine.id)?.value.info?.providers.some((p) => p.installed && p.auth === "ready");
  return page("progress",
    <div className="space-y-4">
      <Steps
        steps={[
          { label: `Rent a ${chosen?.name ?? "new"} sandbox`, state: stateOf(0) },
          { label: `Start ${machineName}`, state: stateOf(1), since: phase.at === "starting" ? phase.since : undefined },
          { label: `Clone ${repo.owner}/${repo.repo}`, state: stateOf(2), since: phase.at === "cloning" ? phase.since : undefined },
        ]}
      />
      {phase.at === "failed" && (
        <p role="alert" className="fade-in text-[12.5px] text-danger">
          {phase.error}
        </p>
      )}
      {ready && (
        <div className="fade-in space-y-3">
          <p className="text-[12.5px] text-muted">
            {project.name} is ready on {ready.machine.name}.{!loggedIn && " Log in to a provider on its card to start working there."}
          </p>
          <div className="flex items-center gap-2">
            {startNew && (
              <button
                type="button"
                autoFocus
                className={primary}
                onClick={() => {
                  closeProjectDialog();
                  startNew(ready.project);
                }}
              >
                <Plus className="size-3.5" /> Start an agent
              </button>
            )}
            <button
              type="button"
              className={btn}
              onClick={() => {
                closeProjectDialog();
                setAgentsView({ kind: "settings", target: `p:${ready.project}` });
              }}
            >
              Open settings
            </button>
          </div>
        </div>
      )}
    </div>,
  );
}

export type StepState = "done" | "now" | "next" | "failed";

/** A short list of steps as they happen: done ticks, the current one shimmering with its time, the rest quiet. */
export function Steps({ steps }: { steps: { label: ReactNode; state: StepState; since?: number }[] }) {
  return (
    <ol className="space-y-2" aria-label="Progress">
      {steps.map((s, i) => (
        <li key={i} className="flex items-center gap-2.5 text-[13px]" aria-current={s.state === "now" ? "step" : undefined}>
          <span
            aria-hidden
            className={`grid size-4 shrink-0 place-items-center rounded-full border transition-colors duration-200 ${
              s.state === "done" ? "border-ok/60 bg-ok/15 text-ok" : s.state === "now" ? "border-accent/60" : s.state === "failed" ? "border-danger/60 bg-danger/15" : "border-line-2"
            }`}
          >
            {s.state === "done" ? <Check className="size-2.5" strokeWidth={2.5} /> : s.state === "now" ? <span className="size-1.5 animate-pulse rounded-full bg-accent motion-reduce:animate-none" /> : null}
          </span>
          <span className={s.state === "now" ? "shimmer" : s.state === "next" ? "text-subtle" : s.state === "failed" ? "text-danger" : "text-muted"}>{s.label}</span>
          {s.state === "now" && s.since !== undefined && <Elapsed since={s.since} />}
          <span className="sr-only">{s.state === "done" ? "done" : s.state === "now" ? "in progress" : s.state === "failed" ? "failed" : "next"}</span>
        </li>
      ))}
    </ol>
  );
}

function Elapsed({ since }: { since: number }) {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, []);
  const s = Math.max(0, Math.round((now - since) / 1000));
  return <span className="ml-auto font-mono text-[11px] text-subtle tabular-nums">{s < 60 ? `${s}s` : `${Math.floor(s / 60)}m ${s % 60}s`}</span>;
}
