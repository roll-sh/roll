import { Lock, Moon, MoreHorizontal, Plus, Search } from "lucide-react";
import { useNavigate } from "@tanstack/react-router";
import { useEffect, useMemo, useRef, useState } from "react";
import { formatUsd } from "@/lib/roll-api";
import { PROJECT_NAME, type ResourceKind, type WorkspaceProject } from "@/lib/workspace-api";
import {
  CREATABLE,
  freeSpot,
  KIND_BLURB,
  KIND_LABEL,
  missing,
  monthOf,
  projectMonth,
  resourceKey,
  suggestName,
  type ResourceView,
} from "@/lib/workspace-resources";
import { spendOf, useWorkspace, workspace, type WorkspaceState } from "@/lib/workspace-store";
import { AppShell } from "../app-shell";
import { Board, type Card } from "./board";
import { Inspector, ResourceSheet, StagedSheet } from "./inspector";
import { KindTile, MiniMap } from "./kind";

const btn =
  "flex h-7 items-center gap-1.5 rounded-md border border-line-2 bg-raised px-2.5 text-[12px] font-medium inset-shadow-highlight transition-colors hover:border-line-3";

/** `/dashboard/projects/$id`: the project's resources on a canvas. */
export function ProjectPage({ id }: { id: string }) {
  const ws = useWorkspace();
  const project = ws.projects.find((p) => p.id === id);
  if (!project)
    return (
      <AppShell crumb={ws.status === "loading" ? "" : "Project"} bleed>
        <div className="grid h-full place-items-center text-[13px] text-muted">
          {ws.status === "loading" ? "" : ws.status === "error" ? `Projects didn't load: ${ws.error}` : "There's no such project."}
        </div>
      </AppShell>
    );
  return <Project ws={ws} project={project} />;
}

function Project({ ws, project }: { ws: WorkspaceState; project: WorkspaceProject }) {
  const [selected, setSelected] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const staged = ws.staged[project.id] ?? [];
  const busy = staged.some((s) => s.phase === "creating");

  useEffect(() => setSelected(null), [project.id]);

  const cards: Card[] = useMemo(
    () => [
      ...project.resources.map((r): Card => {
        const key = resourceKey(r);
        return { type: "placed", key, x: r.x, y: r.y, view: ws.inventory.get(key) ?? missing(r) };
      }),
      ...staged.map((s): Card => ({ type: "staged", key: s.id, x: s.x, y: s.y, staged: s })),
    ],
    [project.resources, staged, ws.inventory, ws.status],
  );
  const current = cards.find((c) => c.key === selected) ?? null;

  const fail = (e: unknown) => setError(e instanceof Error ? e.message : String(e));
  const stage = (kind: ResourceKind) => {
    const taken = new Set([...ws.inventory.values()].map((v) => v.name).concat(staged.map((s) => s.name)));
    const s = workspace.stage(project.id, kind, suggestName(kind, project.name, taken));
    setAdding(false);
    setSelected(s.id);
  };
  const attach = (v: ResourceView) => {
    const spot = freeSpot(cards);
    setAdding(false);
    workspace.place(project.id, v.kind, v.ref, spot.x, spot.y).then(() => setSelected(resourceKey(v)), fail);
  };
  const deploy = () => {
    setSelected(null);
    void workspace.deploy(project.id);
  };

  const month = monthOf(new Date());
  const spend = spendOf(ws, project.resources);
  const asleep = cards.filter((c) => c.type === "placed" && c.view.state === "idle").length;

  return (
    <AppShell
      bleed
      crumb={<ProjectName project={project} staged={staged} />}
      actions={
        <>
          {staged.length > 0 && (
            <span className="flex items-center gap-2 border-r border-line pr-2 text-[12px] text-muted">
              <span className="rounded border border-accent/40 px-1.5 font-mono text-[10.5px] text-accent">{staged.length}</span>
              <span className="max-md:hidden">{staged.length === 1 ? "staged change" : "staged changes"}</span>
              <button disabled={busy} onClick={() => workspace.unstage(project.id)} className="rounded px-1.5 py-1 hover:bg-white/[0.05] hover:text-fg disabled:opacity-50">
                Discard
              </button>
              <button
                disabled={busy}
                onClick={deploy}
                className="h-7 rounded-md bg-fg px-2.5 text-[12px] font-medium text-bg transition-opacity hover:opacity-90 disabled:opacity-50"
              >
                {busy ? "Deploying…" : "Deploy"}
              </button>
            </span>
          )}
          <button className={btn} onClick={() => setAdding((a) => !a)} aria-expanded={adding}>
            <Plus className="size-3.5" />
            Add
          </button>
          <ProjectMenu project={project} />
        </>
      }
    >
      <div className="absolute inset-x-0 top-0 bottom-7">
        <Board
          cards={cards}
          selected={selected}
          onSelect={(k) => {
            setSelected(k);
            setAdding(false);
          }}
          onMove={(c, x, y) => {
            if (c.type === "staged") workspace.updateStaged(project.id, c.key, { x, y });
            else workspace.place(project.id, c.view.kind, c.view.ref, x, y).catch(fail);
          }}
        />
        {cards.length === 0 && ws.status === "ready" && <Empty name={project.name} onPick={stage} />}
        <AddMenu open={adding} ws={ws} onClose={() => setAdding(false)} onNew={stage} onAttach={attach} />
        {error && (
          <button
            onClick={() => setError(null)}
            className="rise absolute bottom-3 left-1/2 z-30 -translate-x-1/2 rounded-lg border border-danger/40 bg-sheet px-3 py-1.5 text-[12px] text-danger shadow-pop"
          >
            {error}
          </button>
        )}
        <Inspector open={!!current} onClose={() => setSelected(null)}>
          {current?.type === "placed" ? (
            <ResourceSheet
              key={current.key}
              view={current.view}
              spent={ws.spend.get(current.key)}
              onClose={() => setSelected(null)}
              onRemove={() => {
                setSelected(null);
                workspace.unplace(project.id, current.view.kind, current.view.ref).catch(fail);
              }}
            />
          ) : current?.type === "staged" ? (
            <StagedSheet
              key={current.key}
              staged={current.staged}
              onClose={() => setSelected(null)}
              onRename={(name) => workspace.updateStaged(project.id, current.key, { name })}
              onRemove={() => {
                setSelected(null);
                workspace.unstage(project.id, current.key);
              }}
              onDeploy={deploy}
            />
          ) : null}
        </Inspector>
      </div>
      <footer className="absolute inset-x-0 bottom-0 flex h-7 items-center gap-4 border-t border-line bg-canvas px-3 font-mono text-[10.5px] text-subtle">
        <span>
          {project.resources.length} {project.resources.length === 1 ? "resource" : "resources"}
        </span>
        {asleep > 0 && (
          <span className="flex items-center gap-1">
            <Moon className="size-3" />
            {asleep} scaled to zero
          </span>
        )}
        <span className="flex items-center gap-1 max-sm:hidden">
          <Lock className="size-3" />
          only you
        </span>
        <span className="ml-auto">
          {formatUsd(spend.usd)} this month · ≈ {formatUsd(projectMonth(spend.usd, month))} by {month.end}
        </span>
      </footer>
    </AppShell>
  );
}

/** The project's name in the top bar; click to rename. */
function ProjectName({ project, staged }: { project: WorkspaceProject; staged: { x: number; y: number }[] }) {
  const [editing, setEditing] = useState(false);
  const [name, setName] = useState(project.name);
  const [bad, setBad] = useState(false);
  useEffect(() => setName(project.name), [project.name]);
  const save = () => {
    setEditing(false);
    const n = name.trim();
    if (n === project.name) return;
    if (!PROJECT_NAME.test(n)) return setName(project.name);
    workspace.rename(project.id, n).catch(() => {
      setName(project.name);
      setBad(true);
      setTimeout(() => setBad(false), 1200);
    });
  };
  return (
    <span className="flex items-center gap-2">
      <MiniMap project={project} staged={staged} size={20} />
      {editing ? (
        <input
          autoFocus
          value={name}
          onChange={(e) => setName(e.target.value.toLowerCase())}
          onBlur={save}
          onKeyDown={(e) => {
            if (e.key === "Enter") save();
            if (e.key === "Escape") {
              setName(project.name);
              setEditing(false);
            }
          }}
          className="w-44 rounded border border-line-2 bg-raised px-1.5 py-0.5 font-medium outline-none"
        />
      ) : (
        <button onClick={() => setEditing(true)} title="Rename" className={`rounded px-1 py-0.5 hover:bg-white/[0.05] ${bad ? "text-danger" : ""}`}>
          {project.name}
        </button>
      )}
    </span>
  );
}

function ProjectMenu({ project }: { project: WorkspaceProject }) {
  const [open, setOpen] = useState<{ top: number; right: number } | null>(null);
  const [confirm, setConfirm] = useState(false);
  const navigate = useNavigate();
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const off = (e: MouseEvent) => !ref.current?.contains(e.target as Node) && (setOpen(null), setConfirm(false));
    window.addEventListener("mousedown", off);
    return () => window.removeEventListener("mousedown", off);
  }, [open]);
  return (
    <div ref={ref} className="relative">
      <button
        onClick={(e) => {
          // Fixed, so the top bar's sideways scroll doesn't clip it.
          const r = e.currentTarget.getBoundingClientRect();
          setOpen((o) => (o ? null : { top: r.bottom + 4, right: window.innerWidth - r.right }));
        }}
        aria-label="Project settings" className="grid size-7 place-items-center rounded-md text-muted hover:bg-white/[0.05] hover:text-fg">
        <MoreHorizontal className="size-4" />
      </button>
      {open && (
        <div style={open} className="rise fixed z-50 w-56 rounded-lg border border-line-2 bg-popover p-1 shadow-pop">
          <button
            onClick={() => {
              if (!confirm) return setConfirm(true);
              workspace.removeProject(project.id).then(() => navigate({ href: "/dashboard" }));
            }}
            className="w-full rounded-md px-2 py-1.5 text-left text-[12px] text-danger hover:bg-danger/10"
          >
            {confirm ? "Click again to delete" : "Delete project"}
            <span className="block text-[11px] text-subtle">Its resources stay; only the grouping goes.</span>
          </button>
        </div>
      )}
    </div>
  );
}

function Empty({ name, onPick }: { name: string; onPick: (k: ResourceKind) => void }) {
  return (
    <div className="pointer-events-none absolute inset-0 grid place-items-center p-4">
      <div className="rise pointer-events-auto w-full max-w-[460px] text-center">
        <h2 className="text-[18px] font-semibold tracking-[-0.045em]">What should {name} run?</h2>
        <p className="mt-1 text-[12.5px] text-muted">Add a database or storage. Everything in a project scales to zero when it's idle.</p>
        <div className="mt-5 grid grid-cols-2 gap-2 text-left sm:grid-cols-3">
          {CREATABLE.map((k) => (
            <button
              key={k}
              onClick={() => onPick(k)}
              className="rounded-[10px] border border-line-2 bg-sheet p-2.5 text-left shadow-card inset-shadow-highlight transition-[border-color,box-shadow] duration-150 hover:border-line-3 hover:shadow-card-hover"
            >
              <KindTile kind={k} />
              <div className="mt-2 text-[12.5px] font-medium">{KIND_LABEL[k]}</div>
              <div className="text-[11px] text-subtle">{KIND_BLURB[k]}</div>
            </button>
          ))}
        </div>
      </div>
    </div>
  );
}

/** "Add": a new resource (staged) or one that's in no project yet. */
function AddMenu({
  open,
  ws,
  onClose,
  onNew,
  onAttach,
}: {
  open: boolean;
  ws: WorkspaceState;
  onClose: () => void;
  onNew: (k: ResourceKind) => void;
  onAttach: (v: ResourceView) => void;
}) {
  const [q, setQ] = useState("");
  const input = useRef<HTMLInputElement>(null);
  useEffect(() => {
    if (open) {
      setQ("");
      requestAnimationFrame(() => input.current?.focus());
    }
  }, [open]);
  const placed = new Set(ws.projects.flatMap((p) => p.resources.map(resourceKey)));
  const match = (s: string) => s.toLowerCase().includes(q.trim().toLowerCase());
  const fresh = CREATABLE.filter((k) => match(KIND_LABEL[k] + KIND_BLURB[k]));
  const loose = [...ws.inventory.values()].filter((v) => !placed.has(resourceKey(v)) && match(v.name + KIND_LABEL[v.kind]));

  return (
    <div
      className={`absolute top-2 right-2 z-30 w-[290px] origin-top-right rounded-xl border border-line-2 bg-popover shadow-pop inset-shadow-highlight transition-[opacity,transform] duration-150 ease-out-soft motion-reduce:transition-none ${
        open ? "scale-100 opacity-100" : "pointer-events-none scale-95 opacity-0"
      }`}
      onKeyDown={(e) => e.key === "Escape" && onClose()}
    >
      <div className="flex items-center gap-2 border-b border-line px-2.5 py-2 text-subtle">
        <Search className="size-3.5" />
        <input
          ref={input}
          value={q}
          onChange={(e) => setQ(e.target.value)}
          onKeyDown={(e) => {
            if (e.key !== "Enter") return;
            if (fresh[0]) onNew(fresh[0]);
            else if (loose[0]) onAttach(loose[0]);
          }}
          placeholder="Add to this project…"
          className="min-w-0 flex-1 bg-transparent text-[12.5px] text-fg outline-none placeholder:text-subtle"
        />
      </div>
      <div className="max-h-[360px] overflow-y-auto p-1 [scrollbar-width:none]">
        {fresh.length > 0 && <Group>New</Group>}
        {fresh.map((k) => (
          <Option key={k} kind={k} title={KIND_LABEL[k]} sub={KIND_BLURB[k]} right="$0 idle" onClick={() => onNew(k)} />
        ))}
        {loose.length > 0 && <Group>Not in a project</Group>}
        {loose.map((v) => (
          <Option key={resourceKey(v)} kind={v.kind} title={v.name} sub={`${KIND_LABEL[v.kind]} · ${v.sub}`} onClick={() => onAttach(v)} />
        ))}
        {!fresh.length && !loose.length && <p className="px-2 py-3 text-[12px] text-subtle">Nothing matches.</p>}
      </div>
    </div>
  );
}

const Group = ({ children }: { children: React.ReactNode }) => <div className="px-2 pt-1.5 pb-1 text-[10.5px] font-medium text-subtle">{children}</div>;

function Option({ kind, title, sub, right, onClick }: { kind: ResourceKind; title: string; sub: string; right?: string; onClick: () => void }) {
  return (
    <button onClick={onClick} className="flex w-full items-center gap-2.5 rounded-lg px-2 py-1.5 text-left transition-colors hover:bg-white/[0.05]">
      <KindTile kind={kind} />
      <span className="min-w-0 flex-1 leading-tight">
        <span className="block truncate text-[12.5px] font-medium">{title}</span>
        <span className="block truncate text-[11px] text-subtle">{sub}</span>
      </span>
      {right && <span className="font-mono text-[10.5px] text-subtle">{right}</span>}
    </button>
  );
}
