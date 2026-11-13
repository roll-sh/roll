import { Bot, Eye, KeyRound, Lock, Moon, Plus, Search, Settings } from "lucide-react";
import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { Link } from "@/components/link";
import type { ResourceKind, WorkspaceProject } from "@/lib/workspace-api";
import {
  CREATABLE,
  freeSpot,
  KIND_BLURB,
  KIND_LABEL,
  missing,
  monthOf,
  projectMonth,
  resourceKey,
  service as serviceView,
  suggestName,
  usd,
  type ResourceView,
} from "@/lib/workspace-resources";
import { spendOf, useWorkspace, workspace, type WorkspaceState } from "@/lib/workspace-store";
import { useProjectServices } from "../apps/live";
import { NewServiceSheet } from "../apps/new-service";
import { AppShell } from "../app-shell";
import { Board, type Card } from "./board";
import { Inspector, ResourceSheet, StagedSheet } from "./inspector";
import { ProjectAgents, threadsOf, useAgents } from "./agents";
import { KindTile } from "./kind";
import { ProjectIcon } from "./project-icon";
import { ProjectSettings } from "./project-settings";
import { PreviewsOverlay } from "./previews-overlay";

const btn =
  "flex h-7 items-center gap-1.5 rounded-md border border-line-2 bg-raised px-2.5 text-[12px] font-medium inset-shadow-highlight transition-colors hover:border-line-3";

export type ProjectTab = "canvas" | "previews";

/**
 * `/dashboard/projects/$id`: the project's resources on a canvas, or (`children`, the route's
 * outlet) another of its views under the same header.
 */
export function ProjectPage({ id, tab = "canvas", children }: { id: string; tab?: ProjectTab; children?: ReactNode }) {
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
  return (
    <Project key={project.id} ws={ws} project={project} tab={tab}>
      {children}
    </Project>
  );
}

function Project({ ws, project, tab, children }: { ws: WorkspaceState; project: WorkspaceProject; tab: ProjectTab; children?: ReactNode }) {
  const [selected, setSelected] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [settings, setSettings] = useState(false);
  /** Where a new service's card goes while its sheet is open. */
  const [newService, setNewService] = useState<{ x: number; y: number } | null>(null);
  const live = useProjectServices(project.id, ws.kinds.has("service"));
  const staged = ws.staged[project.id] ?? [];
  const busy = staged.some((s) => s.phase === "creating");
  const repos = useMemo(() => new Set(project.resources.filter((r) => r.kind === "repo").map((r) => r.ref)), [project.resources]);
  const agents = useAgents();
  const working = agents ? threadsOf(agents, repos).filter((t) => t.status !== "idle").length : 0;
  // Open while the project has a repository its agents can work in; the toggle decides after that.
  const [agentsOpen, setAgentsOpen] = useState(repos.size > 0);
  // On a phone the panel covers the page instead of sitting beside it: closed until asked for.
  useEffect(() => {
    if (!window.matchMedia("(min-width: 768px)").matches) setAgentsOpen(false);
  }, []);
  const [overlay, setOverlay] = useState(false);
  const canvas = tab === "canvas";

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
  // A service and the databases it links, joined on the canvas (Railway's lines).
  const edges = useMemo(() => {
    const placed = new Set(cards.map((c) => c.key));
    return (live.services ?? []).flatMap((s) =>
      (s.links ?? []).map((l) => [`service:${s.id}`, `${l.kind}:${l.ref}`] as [string, string]).filter(([a, b]) => placed.has(a) && placed.has(b)),
    );
  }, [live.services, cards]);
  const current = cards.find((c) => c.key === selected) ?? null;

  const fail = (e: unknown) => setError(e instanceof Error ? e.message : String(e));
  const stage = (kind: ResourceKind) => {
    if (kind === "service") {
      // A service needs a source before it can be anything: its own sheet, made at once.
      setAdding(false);
      setSelected(null);
      setNewService(freeSpot([...cards, ...staged]));
      return;
    }
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
      crumb={
        <button onClick={() => setSettings(true)} className="flex min-w-0 items-center gap-2 rounded-md py-0.5 pr-1.5 pl-0.5 transition-colors hover:bg-white/[0.05]" title="Project settings">
          <ProjectIcon project={project} className="size-5" />
          <span className="truncate">{project.name}</span>
          {project.description && <span className="truncate font-normal text-subtle max-lg:hidden">{project.description}</span>}
        </button>
      }
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
          <nav aria-label="Project views" className="flex h-7 items-center rounded-md border border-line-2 bg-canvas p-0.5">
            {(["canvas", "previews"] as const).map((t) => (
              <Link
                key={t}
                href={`/dashboard/projects/${encodeURIComponent(project.id)}${t === "canvas" ? "" : "/previews"}`}
                aria-current={tab === t ? "page" : undefined}
                className={`flex h-full items-center rounded-[5px] px-2 text-[12px] font-medium transition-colors ${tab === t ? "bg-panel-3 text-fg" : "text-muted hover:text-fg"}`}
              >
                {t === "canvas" ? "Canvas" : "Previews"}
              </Link>
            ))}
          </nav>
          {canvas && (
            <button className={`${btn} ${overlay ? "border-line-3 bg-white/[0.06]" : ""}`} onClick={() => setOverlay((o) => !o)} aria-pressed={overlay} title="Each preview's branches beside their parents">
              <Eye className="size-3.5" />
              <span className="max-md:hidden">Previews</span>
            </button>
          )}
          <Link href={`/dashboard/projects/${project.id}/secrets`} className={btn} title="This project's secrets and environment variables">
            <KeyRound className="size-3.5" />
            <span className="max-md:hidden">Secrets</span>
          </Link>
          <button className={`${btn} ${agentsOpen ? "border-line-3 bg-white/[0.06]" : ""}`} onClick={() => setAgentsOpen((o) => !o)} aria-pressed={agentsOpen} title="This project's agents">
            <Bot className="size-3.5" />
            <span className="max-md:hidden">Agents</span>
            {working > 0 && <span className="pulse-dot size-1.5 rounded-full bg-accent" aria-label={`${working} working`} />}
          </button>
          {canvas && (
            <button className={btn} onClick={() => setAdding((a) => !a)} aria-expanded={adding}>
              <Plus className="size-3.5" />
              Add
            </button>
          )}
          <button onClick={() => setSettings(true)} aria-label="Project settings" title="Project settings" className="grid size-7 place-items-center rounded-md text-muted hover:bg-white/[0.05] hover:text-fg">
            <Settings className="size-4" />
          </button>
        </>
      }
    >
      <div className="absolute inset-x-0 top-0 bottom-7 overflow-clip">
        {!canvas && (
          <div className={`absolute inset-y-0 right-0 left-0 transition-[left] duration-300 ease-drawer motion-reduce:transition-none ${agentsOpen ? "md:left-[272px]" : ""}`}>
            {children}
          </div>
        )}
        {canvas && <Board
          shift={agentsOpen ? 272 : 0}
          cards={cards}
          edges={edges}
          selected={selected}
          onSelect={(k) => {
            setSelected(k);
            if (k) setNewService(null);
            setAdding(false);
          }}
          onMove={(c, x, y) => {
            if (c.type === "staged") workspace.updateStaged(project.id, c.key, { x, y });
            else workspace.place(project.id, c.view.kind, c.view.ref, x, y).catch(fail);
          }}
        >
          {overlay && <PreviewsOverlay projectId={project.id} placed={project.resources} />}
        </Board>}
        {canvas && cards.length === 0 && ws.status === "ready" && <Empty name={project.name} kinds={CREATABLE.filter((k) => ws.kinds.has(k))} onPick={stage} />}
        <ProjectAgents open={agentsOpen} repos={repos} onClose={() => setAgentsOpen(false)} />
        <AddMenu open={canvas && adding} ws={ws} onClose={() => setAdding(false)} onNew={stage} onAttach={attach} />
        {error && (
          <button
            onClick={() => setError(null)}
            className="rise absolute bottom-3 left-1/2 z-30 -translate-x-1/2 rounded-lg border border-danger/40 bg-sheet px-3 py-1.5 text-[12px] text-danger shadow-pop"
          >
            {error}
          </button>
        )}
        <Inspector
          open={canvas && (!!current || !!newService)}
          onClose={() => {
            setSelected(null);
            setNewService(null);
          }}
        >
          {newService && !current ? (
            <NewServiceSheet
              project={project}
              spot={newService}
              onClose={() => setNewService(null)}
              onCreated={(s) => {
                workspace.added(project.id, serviceView(s), newService.x, newService.y);
                setNewService(null);
                setSelected(resourceKey({ kind: "service", ref: s.id }));
                live.refresh();
              }}
            />
          ) : current?.type === "placed" ? (
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
          {usd(spend.usd)} this month · ≈ {usd(projectMonth(spend.usd, month))} by {month.end}
        </span>
      </footer>
      <ProjectSettings project={project} open={settings} onClose={() => setSettings(false)} />
    </AppShell>
  );
}

function Empty({ name, kinds, onPick }: { name: string; kinds: ResourceKind[]; onPick: (k: ResourceKind) => void }) {
  return (
    <div className="pointer-events-none absolute inset-0 grid place-items-center p-4">
      <div className="rise pointer-events-auto w-full max-w-[460px] text-center">
        <h2 className="text-[18px] font-semibold tracking-[-0.045em]">What should {name} run?</h2>
        <p className="mt-1 text-[12.5px] text-muted">Deploy your code, add a database or storage. Everything in a project scales to zero when it's idle.</p>
        <div className="mt-5 grid grid-cols-2 gap-2 text-left sm:grid-cols-3">
          {kinds.map((k) => (
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
  const fresh = CREATABLE.filter((k) => ws.kinds.has(k) && match(KIND_LABEL[k] + KIND_BLURB[k]));
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
