"use client";

/**
 * Adding a project (docs/code/agents.md, "Projects"): one dialog wherever it starts (a machine
 * card's +, a machine's folded projects, ⌘K, the launcher, the empty states), T3's add-project
 * feel (`CommandPaletteAddProject.tsx`, `AddProjectToMachineDialog.tsx`): it grows out of what
 * opened it, picks the machine when there's a choice, and adds a project three ways: a directory
 * already there (`fs.list`), a clone of one of the owner's roll code repos (`project.add` with
 * `clone`), or a new roll code repo started as a project (`POST …/machines/{machine}/projects`).
 * What's slow shows its steps with their time; it ends on the project, with "Start an agent" and
 * "Open settings".
 *
 * Auto-pull is offered right here (project settings): on for a fresh clone, which nobody edits
 * on its default branch, off for a folder that is someone's working checkout.
 */

import { Check, ChevronRight, Folder, FolderOpen, Lock, Plus, Search } from "lucide-react";
import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { commandId, expectOk, NO_SETTINGS, reachable, type FsEntry, type Machine, type Project } from "@/lib/agents-api";
import { setAgentsView } from "@/lib/agents-surface";
import type { RepoChoice } from "@/lib/agents-services";
import { btn, input, primary } from "../ui";
import { MachineIcon, ProjectIcon } from "./identity";
import { Steps } from "./project-actions";
import { shortPath } from "./shared";
import { Segmented, Select, Switch } from "./ui";
import { Dialog } from "./ui/dialog";
import { useAgents } from "./use-agents";

type Tab = "folder" | "clone" | "new";

const TABS: { value: Tab; label: string }[] = [
  { value: "folder", label: "Folder" },
  { value: "clone", label: "Clone a repo" },
  { value: "new", label: "New repo" },
];

/** A roll code repo name: what the server takes (letters, digits, `.`, `_`, `-`; not starting with a dot). */
export function repoNameProblem(name: string): string | null {
  const n = name.trim();
  if (!n) return null;
  if (/\s/.test(n)) return "No spaces: use - or _ instead.";
  if (n.length > 100) return "At most 100 characters.";
  if (!/^[A-Za-z0-9_-][A-Za-z0-9._-]*$/.test(n)) return "Letters, digits, . _ and - only, not starting with a dot.";
  if (n.endsWith(".git")) return "Leave off .git.";
  return null;
}

/** The project already at a path on a machine, if one is. */
function projectAt(projects: Iterable<{ machine: string; value: Project }>, machine: string, path: string): Project | undefined {
  const norm = (p: string) => p.replace(/\/+$/, "") || "/";
  for (const r of projects) if (r.machine === machine && norm(r.value.path) === norm(path)) return r.value;
  return undefined;
}

/** The dialog: `machine` preselects one (else the only one online, else a pick). */
export function AddProjectDialog({ open, machine, onClose }: { open: boolean; machine: string | null; onClose: () => void }) {
  const { client } = useAgents();
  const machines = useMemo(
    () =>
      [...client.data.machines.values()]
        .map((r) => r.value)
        .filter((m) => m.online)
        .sort((a, b) => a.name.localeCompare(b.name)),
    [client.data.machines],
  );
  const [picked, setPicked] = useState<string | null>(null);
  const m = machines.find((x) => x.id === (picked ?? machine)) ?? machines[0];
  const [added, setAdded] = useState<string | null>(null);
  const project = added ? client.data.projects.get(added)?.value : undefined;
  return (
    <Dialog
      open={open}
      onClose={onClose}
      title={project ? "Project added" : "Add a project"}
      lead={project ? undefined : m ? `Agents work in a project: a folder on ${m.name}, a clone of one of your roll repos, or a new one.` : undefined}
    >
      {project && m ? (
        <Added project={project} machine={m} onClose={onClose} onAnother={() => setAdded(null)} />
      ) : !m ? (
        <p className="text-[12.5px] text-muted">No machine is online. Start one with <code className="font-mono">roll agent up</code>, or wake a sandbox, then add a project to it.</p>
      ) : (
        <div className="space-y-3">
          {machines.length > 1 && (
            <div className="flex items-center gap-2">
              <span className="shrink-0 text-[12px] text-muted">On</span>
              <Select
                label="Machine"
                className="min-w-0 flex-1"
                value={m.id}
                onChange={setPicked}
                options={machines.map((x) => ({ value: x.id, label: x.name, hint: x.sandbox ? "sandbox" : x.info?.os }))}
              />
            </div>
          )}
          <AddProjectForm key={m.id} machine={m} onAdded={setAdded} />
        </div>
      )}
    </Dialog>
  );
}

/** Where it ended: the project, and what to do next. */
function Added({ project, machine, onClose, onAnother }: { project: Project; machine: Machine; onClose: () => void; onAnother: () => void }) {
  const { startNew } = useAgents();
  const where = project.repo ? `${project.repo.owner}/${project.repo.repo}` : shortPath(project.path, machine.info?.home);
  return (
    <div className="space-y-4" role="status">
      <div className="flex items-center gap-3 rounded-xl border border-line bg-panel px-3 py-3">
        <ProjectIcon project={project} className="size-8" />
        <span className="min-w-0 flex-1">
          <span className="block truncate text-[14px] font-[590] tracking-[-0.01em]">{project.name}</span>
          <span className="flex items-center gap-1.5 truncate font-mono text-[11px] text-subtle">
            <MachineIcon machine={machine} className="size-3" /> {machine.name} · {where}
          </span>
        </span>
        <Check className="size-4 shrink-0 text-ok" aria-label="Added" />
      </div>
      <div className="flex flex-wrap items-center gap-2">
        {startNew && reachable(machine) && (
          <button
            type="button"
            autoFocus
            className={primary}
            onClick={() => {
              onClose();
              startNew(project.id);
            }}
          >
            <Plus className="size-3.5" /> Start an agent
          </button>
        )}
        <button
          type="button"
          className={btn}
          onClick={() => {
            onClose();
            setAgentsView({ kind: "settings", target: `p:${project.id}` });
          }}
        >
          Open settings
        </button>
        <button type="button" onClick={onAnother} className="ml-auto text-[12px] text-subtle transition-colors hover:text-fg">
          Add another
        </button>
      </div>
    </div>
  );
}

/** The three ways in, on one machine; `onAdded` gets the project's id (an existing one too). */
function AddProjectForm({ machine, onAdded }: { machine: Machine; onAdded: (project: string) => void }) {
  const { client, services } = useAgents();
  const [tab, setTab] = useState<Tab>("folder");
  const [pull, setPull] = useState<boolean | null>(null);
  const autoPull = pull ?? tab !== "folder";
  const tabs = services ? TABS : TABS.slice(0, 1);
  // The project exists once added; turning auto-pull on is best effort (its settings say).
  const added = async (id: string) => {
    const existing = client.data.projects.get(id)?.value.settings;
    if (autoPull && !existing?.auto_pull) {
      await client.send(machine.id, { type: "project.update", project: id, settings: { ...(existing ?? NO_SETTINGS), auto_pull: true } }).catch(() => null);
    }
    onAdded(id);
  };
  return (
    <div className="space-y-3">
      {tabs.length > 1 && <Segmented label="Add a project from" value={tab} onChange={setTab} className="w-full [&>button]:flex-1" options={tabs} />}
      {tab === "folder" && <DirBrowser machine={machine} onAdded={added} onExisting={onAdded} />}
      {tab === "clone" && <CloneRepo machine={machine} onAdded={added} onExisting={onAdded} />}
      {tab === "new" && <NewRepo machine={machine} onAdded={added} />}
      <label className="flex items-start gap-2 border-t border-line pt-3 text-[12.5px] text-muted" title="You can change it later in the project's settings">
        <Switch size="sm" className="mt-0.5" checked={autoPull} onCheckedChange={setPull} aria-label="Pull merged changes" />
        <span>
          Pull merged changes
          <span className="block text-[11.5px] text-subtle">Fast-forwards the default branch while the checkout has no local changes.</span>
        </span>
      </label>
    </div>
  );
}

interface Step {
  machine: Machine;
  onAdded: (project: string) => void | Promise<void>;
  /** A project that's already there: go to it instead. */
  onExisting?: (project: string) => void;
}

function useAct() {
  const [busy, setBusy] = useState<{ key: string; since: number } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const alive = useRef(true);
  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
    };
  }, []);
  const act = async (key: string, fn: () => Promise<void>) => {
    setBusy({ key, since: Date.now() });
    setError(null);
    try {
      await fn();
    } catch (e) {
      if (alive.current) setError(e instanceof Error ? e.message : String(e));
    } finally {
      if (alive.current) setBusy(null);
    }
  };
  return { busy, error, act };
}

function Problem({ children }: { children: ReactNode }) {
  return (
    <p role="alert" className="text-[12px] text-danger">
      {children}
    </p>
  );
}

/** Pick a directory on the machine (`fs.list`) and add it as a project. */
export function DirBrowser({ machine, onAdded, onExisting }: Step) {
  const { client } = useAgents();
  const { busy, error, act } = useAct();
  const [path, setPath] = useState(machine.info?.home ?? "/");
  const [entries, setEntries] = useState<FsEntry[] | null>(null);
  const [listError, setListError] = useState<string | null>(null);
  const [typed, setTyped] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    setEntries(null);
    setListError(null);
    client
      .run<Extract<Awaited<ReturnType<typeof client.send>>, { type: "entries" }>>(machine.id, { type: "fs.list", path }, "entries")
      .then((r) => live && setEntries(r.entries.filter((e) => e.dir && !e.name.startsWith(".")).sort((a, b) => a.name.localeCompare(b.name))))
      .catch((e: unknown) => {
        if (!live) return;
        setEntries([]);
        setListError(e instanceof Error ? e.message : String(e));
      });
    return () => {
      live = false;
    };
  }, [client, machine.id, path]);

  const up = path.replace(/\/[^/]+\/?$/, "") || "/";
  const join = (name: string) => `${path.replace(/\/$/, "")}/${name}`;
  const existing = projectAt(client.data.projects.values(), machine.id, path);
  const tooWide = path === "/" || path === machine.info?.home;
  return (
    <div className="space-y-2">
      <form
        onSubmit={(e) => {
          e.preventDefault();
          if (typed?.trim()) setPath(typed.trim());
          setTyped(null);
        }}
      >
        <input
          value={typed ?? shortPath(path, machine.info?.home)}
          onFocus={() => setTyped(path)}
          onBlur={() => setTyped(null)}
          onChange={(e) => setTyped(e.target.value)}
          aria-label="Directory"
          spellCheck={false}
          className={`${input} w-full font-mono text-[12px]`}
        />
      </form>
      <ul className="h-48 overflow-auto rounded-lg border border-line bg-bg p-1 text-[12.5px]" data-scroll aria-label="Folders">
        {path !== "/" && (
          <li>
            <button type="button" onClick={() => setPath(up)} className="flex w-full items-center gap-1.5 rounded-md px-2 py-1 text-subtle hover:bg-fg/[0.05] hover:text-fg">
              <FolderOpen className="size-3.5" /> ..
            </button>
          </li>
        )}
        {entries === null ? (
          <li className="px-2 py-1 text-subtle">
            <span className="shimmer">Listing…</span>
          </li>
        ) : entries.length === 0 ? (
          <li className="px-2 py-1 text-subtle">{listError ? `Can't list it: ${listError}` : "No folders"}</li>
        ) : (
          entries.map((e) => {
            const there = projectAt(client.data.projects.values(), machine.id, join(e.name));
            return (
              <li key={e.name}>
                <button type="button" onClick={() => setPath(join(e.name))} className="flex w-full items-center gap-1.5 rounded-md px-2 py-1 text-left text-muted hover:bg-fg/[0.05] hover:text-fg">
                  {there ? <ProjectIcon project={there} className="size-3.5" /> : <Folder className="size-3.5 text-subtle" />}
                  <span className="flex-1 truncate">{e.name}</span>
                  {there && <span className="text-[11px] text-subtle">project</span>}
                  <ChevronRight className="size-3 text-subtle" />
                </button>
              </li>
            );
          })
        )}
      </ul>
      {existing ? (
        <p className="flex items-center gap-2 text-[12px] text-muted">
          <span className="min-w-0 flex-1 truncate">
            Already a project: <span className="text-fg">{existing.name}</span>
          </span>
          <button type="button" className={btn} onClick={() => onExisting?.(existing.id)}>
            Go to it
          </button>
        </p>
      ) : (
        <div className="flex items-center gap-2">
          <button
            type="button"
            disabled={busy !== null || listError !== null || tooWide}
            onClick={() =>
              act("add", async () => {
                const r = await client.run<{ type: "created"; id: string }>(machine.id, { type: "project.add", path }, "created");
                await onAdded(r.id);
              })
            }
            className={primary}
          >
            {busy ? <span className="shimmer">Adding…</span> : `Add ${path.split("/").filter(Boolean).at(-1) ?? "this folder"}`}
          </button>
          {tooWide && <span className="text-[11.5px] text-subtle">Pick the project&apos;s own folder.</span>}
        </div>
      )}
      {error && <Problem>{error}</Problem>}
    </div>
  );
}

/** Clone one of the owner's roll code repos onto the machine (into its clone dir, `~/roll/<owner>/<repo>`). */
function CloneRepo({ machine, onAdded, onExisting }: Step) {
  const { client, owner, services } = useAgents();
  const { busy, error, act } = useAct();
  const [repos, setRepos] = useState<RepoChoice[] | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [filter, setFilter] = useState("");
  // One id per repo picked: a retry of the same clone gets the daemon's receipt.
  const [ids] = useState(() => new Map<string, string>());
  useEffect(() => {
    let live = true;
    services
      ?.repos(owner)
      .then((r) => live && setRepos(r))
      .catch((e: Error) => live && setLoadError(e.message));
    return () => {
      live = false;
    };
  }, [services, owner]);
  const words = filter.trim().toLowerCase();
  const shown = (repos ?? []).filter((r) => r.name.toLowerCase().includes(words));
  const here = (r: RepoChoice) => [...client.data.projects.values()].find((p) => p.machine === machine.id && p.value.repo?.owner === r.owner && p.value.repo.repo === r.name)?.value;

  if (busy)
    return (
      <div className="space-y-3 py-1">
        <Steps
          steps={[
            { label: `Clone ${busy.key} onto ${machine.name}`, state: "now", since: busy.since },
            { label: "Add it as a project", state: "next" },
          ]}
        />
        <p className="font-mono text-[11px] text-subtle">into {shortPath(`${machine.info?.home ?? "~"}/roll/${owner}/…`, machine.info?.home)}</p>
      </div>
    );
  return (
    <div className="space-y-2">
      <label className="flex items-center gap-1.5 rounded-lg border border-line bg-bg px-2 py-1.5 focus-within:border-accent/50">
        <Search className="size-3.5 text-subtle" />
        <input autoFocus value={filter} onChange={(e) => setFilter(e.target.value)} placeholder="Find a repo" aria-label="Find a repo" className="min-w-0 flex-1 bg-transparent text-[12.5px] outline-none" />
      </label>
      <ul className="h-48 overflow-auto rounded-lg border border-line bg-bg p-1 text-[12.5px]" data-scroll aria-label="Repos">
        {loadError ? (
          <li className="px-2 py-1 text-danger">{loadError}</li>
        ) : repos === null ? (
          <li className="px-2 py-1 text-subtle">
            <span className="shimmer">Loading repos…</span>
          </li>
        ) : repos.length === 0 ? (
          <li className="px-2 py-1 text-subtle">No repos yet. Start one under &ldquo;New repo&rdquo;.</li>
        ) : shown.length === 0 ? (
          <li className="px-2 py-1 text-subtle">No repo matches &ldquo;{filter}&rdquo;.</li>
        ) : (
          shown.map((r) => {
            const there = here(r);
            return (
              <li key={r.full_name}>
                <button
                  type="button"
                  onClick={() =>
                    there
                      ? onExisting?.(there.id)
                      : act(r.full_name, async () => {
                          if (!ids.has(r.full_name)) ids.set(r.full_name, commandId());
                          const reply = await client.send(machine.id, { type: "project.add", path: "", clone: { owner: r.owner, repo: r.name } }, ids.get(r.full_name));
                          const created = expectOk<{ type: "created"; id: string }>(reply, "created");
                          await onAdded(created.id);
                        })
                  }
                  className="flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left hover:bg-fg/[0.05]"
                >
                  {r.visibility === "private" ? <Lock className="size-3.5 shrink-0 text-subtle" /> : <Folder className="size-3.5 shrink-0 text-subtle" />}
                  <span className="min-w-0 flex-1">
                    <span className="block truncate">{r.name}</span>
                    {r.description && <span className="block truncate text-[11px] text-subtle">{r.description}</span>}
                  </span>
                  {there && <span className="shrink-0 text-[11px] text-subtle">On {machine.name}</span>}
                </button>
              </li>
            );
          })
        )}
      </ul>
      <p className="font-mono text-[11px] text-subtle">into {shortPath(`${machine.info?.home ?? "~"}/roll/${owner}/…`, machine.info?.home)}</p>
      {error && <Problem>{error}</Problem>}
    </div>
  );
}

/** Start a new roll code repo and clone it onto the machine as a project, in one step. */
function NewRepo({ machine, onAdded }: Step) {
  const { owner, services, demo } = useAgents();
  const { busy, error, act } = useAct();
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [priv, setPriv] = useState(true);
  // The idempotency key of this form's create: a retry after a timeout answers the same.
  const [id, setId] = useState(commandId);
  const problem = repoNameProblem(name);
  const valid = name.trim().length > 0 && !problem;
  if (busy)
    return (
      <Steps
        steps={[
          { label: `Create ${owner}/${name.trim()} and clone it onto ${machine.name}`, state: "now", since: busy.since },
          { label: "Add it as a project", state: "next" },
        ]}
      />
    );
  return (
    <form
      className="space-y-2"
      onSubmit={(e) => {
        e.preventDefault();
        if (!services || !valid) return;
        void act("create", async () => {
          const reply = await services.createProject(owner, machine.id, {
            id,
            repo: { name: name.trim(), description: description.trim() || undefined, visibility: priv ? "private" : "public" },
          });
          const created = expectOk<{ type: "created"; id: string }>(reply, "created");
          setId(commandId());
          await onAdded(created.id);
        });
      }}
    >
      <div className="flex items-center gap-1 font-mono text-[12.5px]">
        <span className="text-subtle">{owner}/</span>
        <input
          autoFocus
          value={name}
          onChange={(e) => setName(e.target.value)}
          placeholder="repo-name"
          aria-label="Repo name"
          aria-invalid={!!problem}
          spellCheck={false}
          className={`${input} min-w-0 flex-1 font-mono text-[12.5px] aria-invalid:border-danger/50`}
        />
      </div>
      {problem && <p className="text-[11.5px] text-danger">{problem}</p>}
      <input value={description} onChange={(e) => setDescription(e.target.value)} placeholder="Description (optional)" aria-label="Description" className={`${input} w-full text-[12.5px]`} />
      <label className="flex items-center gap-1.5 text-[12.5px] text-muted">
        <Switch size="sm" checked={priv} onCheckedChange={setPriv} aria-label="Private" /> Private
      </label>
      <button type="submit" disabled={!valid} className={primary}>
        Create and clone
      </button>
      {!demo && <p className="text-[11.5px] text-subtle">A roll code repo, cloned onto {machine.name}. Threads publish to it as changes.</p>}
      {error && <Problem>{error}</Problem>}
    </form>
  );
}
