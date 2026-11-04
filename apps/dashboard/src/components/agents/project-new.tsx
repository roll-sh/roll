"use client";

import { ChevronRight, Folder, FolderOpen, Lock, Search } from "lucide-react";
import { useEffect, useState } from "react";
import { commandId, expectOk, NO_SETTINGS, type FsEntry, type Machine } from "@/lib/agents-api";
import type { RepoChoice } from "@/lib/agents-services";
import { btn, input, primary } from "../ui";
import { shortPath } from "./shared";
import { useAgents } from "./use-agents";

type Tab = "folder" | "clone" | "new";

const TABS: { id: Tab; label: string }[] = [
  { id: "folder", label: "Folder" },
  { id: "clone", label: "Clone a repo" },
  { id: "new", label: "New repo" },
];

/**
 * Add a project to a machine three ways: a directory already there (`fs.list`), a clone of one of
 * the owner's roll code repos (`project.add` with `clone`), or a new roll code repo started as a
 * project (`POST …/machines/{machine}/projects`). `onAdded` gets the new project's id.
 *
 * Auto-pull is offered right here (project settings): on for a fresh clone, which nobody edits
 * on its default branch, off for a folder that is someone's working checkout.
 */
export function NewProject({ machine, onClose, onAdded }: { machine: Machine; onClose: () => void; onAdded: (project: string) => void }) {
  const { client, services, demo } = useAgents();
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
    <div data-nodrag className="space-y-2 rounded-lg border border-line bg-bg p-2">
      {tabs.length > 1 && (
        <div role="tablist" aria-label="Add a project from" className="flex gap-0.5 rounded-md bg-panel p-0.5 text-[11.5px]">
          {tabs.map((t) => (
            <button
              key={t.id}
              role="tab"
              aria-selected={tab === t.id}
              onClick={() => setTab(t.id)}
              className={`flex-1 rounded px-1.5 py-1 transition ${tab === t.id ? "bg-panel-2 text-fg" : "text-subtle hover:text-fg"}`}
            >
              {t.label}
            </button>
          ))}
        </div>
      )}
      {tab === "folder" && <DirBrowser machine={machine} onClose={onClose} onAdded={added} />}
      {tab === "clone" && <CloneRepo machine={machine} onClose={onClose} onAdded={added} />}
      {tab === "new" && <NewRepo machine={machine} onClose={onClose} onAdded={added} demo={demo} />}
      <label className="flex items-start gap-1.5 border-t border-line px-0.5 pt-2 text-[12px] text-muted" title="You can change it later in the project's settings">
        <input type="checkbox" checked={autoPull} onChange={(e) => setPull(e.target.checked)} className="mt-0.5 accent-[var(--accent)]" />
        <span>
          Pull merged changes
          <span className="block text-[11px] text-subtle">Fast-forwards the default branch while the checkout has no local changes.</span>
        </span>
      </label>
    </div>
  );
}

interface Step {
  machine: Machine;
  onClose: () => void;
  onAdded: (project: string) => void | Promise<void>;
}

function useAct() {
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const act = async (key: string, fn: () => Promise<void>) => {
    setBusy(key);
    setError(null);
    try {
      await fn();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(null);
    }
  };
  return { busy, error, act };
}

/** Pick a directory on the machine (`fs.list`) and add it as a project. */
export function DirBrowser({ machine, onClose, onAdded }: Step) {
  const { client } = useAgents();
  const { busy, error, act } = useAct();
  const [path, setPath] = useState(machine.info?.home ?? "/");
  const [entries, setEntries] = useState<FsEntry[] | null>(null);
  const [typed, setTyped] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    setEntries(null);
    client
      .run<Extract<Awaited<ReturnType<typeof client.send>>, { type: "entries" }>>(machine.id, { type: "fs.list", path }, "entries")
      .then((r) => live && setEntries(r.entries.filter((e) => e.dir && !e.name.startsWith(".")).sort((a, b) => a.name.localeCompare(b.name))))
      .catch(() => live && setEntries([]));
    return () => {
      live = false;
    };
  }, [client, machine.id, path]);

  const up = path.replace(/\/[^/]+\/?$/, "") || "/";
  const join = (name: string) => `${path.replace(/\/$/, "")}/${name}`;
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
          className={`${input} w-full font-mono text-[11.5px]`}
        />
      </form>
      <ul className="max-h-44 overflow-auto text-[12px]" data-scroll aria-label="Folders">
        {path !== "/" && (
          <li>
            <button onClick={() => setPath(up)} className="flex w-full items-center gap-1.5 rounded px-1.5 py-1 text-subtle hover:bg-panel-2 hover:text-fg">
              <FolderOpen className="size-3.5" /> ..
            </button>
          </li>
        )}
        {entries === null ? (
          <li className="px-1.5 py-1 text-subtle">
            <span className="shimmer">Listing…</span>
          </li>
        ) : entries.length === 0 ? (
          <li className="px-1.5 py-1 text-subtle">No folders</li>
        ) : (
          entries.map((e) => (
            <li key={e.name}>
              <button onClick={() => setPath(join(e.name))} className="flex w-full items-center gap-1.5 rounded px-1.5 py-1 text-left text-muted hover:bg-panel-2 hover:text-fg">
                <Folder className="size-3.5 text-subtle" />
                <span className="flex-1 truncate">{e.name}</span>
                <ChevronRight className="size-3 text-subtle" />
              </button>
            </li>
          ))
        )}
      </ul>
      <div className="flex items-center gap-1.5">
        <button
          disabled={busy !== null}
          onClick={() =>
            act("add", async () => {
              const r = await client.run<{ type: "created"; id: string }>(machine.id, { type: "project.add", path }, "created");
              await onAdded(r.id);
              onClose();
            })
          }
          className={primary}
        >
          {busy ? <span className="shimmer">Adding…</span> : "Add this folder"}
        </button>
        <button onClick={onClose} className={btn}>
          Cancel
        </button>
      </div>
      {error && <p className="text-[12px] text-danger">{error}</p>}
    </div>
  );
}

/** Clone one of the owner's roll code repos onto the machine (into its clone dir, `~/roll/<owner>/<repo>`). */
function CloneRepo({ machine, onClose, onAdded }: Step) {
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
  const shown = (repos ?? []).filter((r) => r.name.toLowerCase().includes(filter.trim().toLowerCase()));

  return (
    <div className="space-y-2">
      {repos && repos.length > 6 && (
        <label className="flex items-center gap-1.5 rounded-lg border border-line bg-panel px-2 py-1">
          <Search className="size-3.5 text-subtle" />
          <input value={filter} onChange={(e) => setFilter(e.target.value)} placeholder="Find a repo" aria-label="Find a repo" className="min-w-0 flex-1 bg-transparent text-[12px] outline-none" />
        </label>
      )}
      <ul className="max-h-48 overflow-auto text-[12px]" data-scroll aria-label="Repos">
        {loadError ? (
          <li className="px-1.5 py-1 text-danger">{loadError}</li>
        ) : repos === null ? (
          <li className="px-1.5 py-1 text-subtle">
            <span className="shimmer">Loading repos…</span>
          </li>
        ) : repos.length === 0 ? (
          <li className="px-1.5 py-1 text-subtle">No repos yet. Start one under &ldquo;New repo&rdquo;.</li>
        ) : (
          shown.map((r) => (
            <li key={r.full_name}>
              <button
                disabled={busy !== null}
                onClick={() =>
                  act(r.full_name, async () => {
                    if (!ids.has(r.full_name)) ids.set(r.full_name, commandId());
                    const reply = await client.send(machine.id, { type: "project.add", path: "", clone: { owner: r.owner, repo: r.name } }, ids.get(r.full_name));
                    const created = expectOk<{ type: "created"; id: string }>(reply, "created");
                    await onAdded(created.id);
                    onClose();
                  })
                }
                className="flex w-full items-center gap-1.5 rounded px-1.5 py-1.5 text-left hover:bg-panel-2 disabled:opacity-50"
              >
                {r.visibility === "private" ? <Lock className="size-3.5 shrink-0 text-subtle" /> : <Folder className="size-3.5 shrink-0 text-subtle" />}
                <span className="min-w-0 flex-1">
                  <span className="block truncate">{busy === r.full_name ? <span className="shimmer">Cloning {r.name}…</span> : r.name}</span>
                  {r.description && <span className="block truncate text-[11px] text-subtle">{r.description}</span>}
                </span>
              </button>
            </li>
          ))
        )}
      </ul>
      <p className="font-mono text-[10.5px] text-subtle">
        into {shortPath(`${machine.info?.home ?? "~"}/roll/${owner}/…`, machine.info?.home)}
      </p>
      <button onClick={onClose} className={btn}>
        Cancel
      </button>
      {error && <p className="text-[12px] text-danger">{error}</p>}
    </div>
  );
}

/** Start a new roll code repo and clone it onto the machine as a project, in one step. */
function NewRepo({ machine, onClose, onAdded, demo }: Step & { demo: boolean }) {
  const { owner, services } = useAgents();
  const { busy, error, act } = useAct();
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [priv, setPriv] = useState(true);
  // The idempotency key of this form's create: a retry after a timeout answers the same.
  const [id, setId] = useState(commandId);
  const valid = name.trim().length > 0 && !/\s/.test(name.trim());
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
          onClose();
        });
      }}
    >
      <div className="flex items-center gap-1 font-mono text-[12px]">
        <span className="text-subtle">{owner}/</span>
        <input value={name} onChange={(e) => setName(e.target.value)} placeholder="repo-name" aria-label="Repo name" spellCheck={false} className={`${input} min-w-0 flex-1 py-1 font-mono text-[12px]`} />
      </div>
      <input value={description} onChange={(e) => setDescription(e.target.value)} placeholder="Description (optional)" aria-label="Description" className={`${input} w-full py-1 text-[12px]`} />
      <label className="flex items-center gap-1.5 text-[12px] text-muted">
        <input type="checkbox" checked={priv} onChange={(e) => setPriv(e.target.checked)} className="accent-[var(--accent)]" /> Private
      </label>
      <div className="flex items-center gap-1.5">
        <button type="submit" disabled={busy !== null || !valid} className={primary}>
          {busy ? <span className="shimmer">Creating…</span> : "Create and clone"}
        </button>
        <button type="button" onClick={onClose} className={btn}>
          Cancel
        </button>
      </div>
      {!demo && <p className="text-[11px] text-subtle">A roll code repo, cloned onto {machine.name}. Threads publish to it as changes.</p>}
      {error && <p className="text-[12px] text-danger">{error}</p>}
    </form>
  );
}
