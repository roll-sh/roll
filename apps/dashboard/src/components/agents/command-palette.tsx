"use client";

import {
  AlarmClock,
  AlarmClockOff,
  Archive,
  ArchiveRestore,
  ArrowDown,
  ArrowDownFromLine,
  ArrowRightFromLine,
  ArrowUp,
  Bell,
  Inbox,
  Bookmark,
  Eye,
  EyeOff,
  Pencil,
  Rows3,
  SlidersHorizontal,
  Trash2,
  Bot,
  Check,
  ChevronLeft,
  ChevronRight,
  CircleCheck,
  Folder,
  FolderPlus,
  Focus,
  LayoutGrid,
  GitFork,
  Maximize,
  MessageSquare,
  Play,
  Plus,
  Search,
  Server,
  Settings2,
  Square,
  Workflow,
  Zap,
} from "lucide-react";
import { useEffect, useLayoutEffect, useMemo, useReducer, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { isBusy, reachable, type Machine, type Thread } from "@/lib/agents-api";
import type { Direction } from "@/lib/agents-layout";
import { ALL, HIDEABLE, type Display, type Hideable } from "@/lib/agents-views";
import { requestView } from "./views";
import { rolling } from "@/lib/agents-graph";
import { agentsPalette, closePalette, noteThreadOpen, threadOpens, usePalette, type PaletteIntent } from "@/lib/agents-palette";
import { setAgentsView, useAgentsSurface } from "@/lib/agents-surface";
import { splitSearchText, type SearchMatchHint } from "@/lib/palette-search";
import {
  buildRows,
  filterGroups,
  autoHighlight,
  findItem,
  nestedItems,
  placeholderFor,
  RECENT_THREAD_LIMIT,
  reduceStack,
  type PaletteAction,
  type PaletteGroup,
  type PaletteItem,
  type PaletteView,
} from "./command-palette.logic";
import { targetLabel, type Target as GoalTarget } from "./new-thread";
import { useGoalTargets, useLaunchGoal, type GoalKind } from "./orchestrate";
import { useAutomations } from "./automations/shared";
import { unseenNotices } from "@/lib/automations-store";
import { openProjectDialog } from "@/lib/agents-project-dialogs";
import { StatusDot } from "./shared";
import { MachineIcon, ProjectIcon } from "./identity";
import { useAgents } from "./use-agents";
import { useInboxActions } from "./threads-sidebar";
import { inbox, shelfOf, snoozePresets, type Shelf } from "@/lib/agents-inbox";
import { openShelf } from "@/lib/agents-shelf";
import { NO_CANVAS } from "./machine-projects";
import { projectActions } from "./project-actions";

const SHELF_NAME: Record<Exclude<Shelf, "active">, string> = { settled: "Settled", snoozed: "Snoozed", archived: "Archived" };

const ICON = "size-4 text-subtle";

const HIDE_NAMES: Record<Hideable, string> = { idle: "idle threads", terminals: "terminals" };

/** The Display popover's choices as rows, each saying what it does to the view now (Linear's ⌘K display). */
function displayItems(host: PaletteHost): PaletteItem[] {
  const d = host.display;
  const set = (patch: Partial<Display>) => () => {
    setAgentsView({ kind: "canvas" });
    host.setDisplay({ ...d, ...patch });
  };
  const hidden = d.hide ?? [];
  return [
    { kind: "action", value: "display:density", title: d.density === "compact" ? "Comfortable cards" : "Compact cards", icon: <Rows3 className={ICON} />, run: set({ density: d.density === "compact" ? undefined : "compact" }) },
    ...HIDEABLE.map(
      (h): PaletteItem => ({
        kind: "action",
        value: `display:hide:${h}`,
        title: hidden.includes(h) ? `Show ${HIDE_NAMES[h]}` : `Hide ${HIDE_NAMES[h]}`,
        icon: hidden.includes(h) ? <Eye className={ICON} /> : <EyeOff className={ICON} />,
        run: set({ hide: hidden.includes(h) ? hidden.filter((x) => x !== h) : [...hidden, h] }),
      }),
    ),
    { kind: "action", value: "display:group", title: d.group === "project" ? "Group by machine" : "Group by project", icon: <LayoutGrid className={ICON} />, run: set({ group: d.group === "project" ? undefined : "project" }) },
    { kind: "action", value: "display:projects", title: d.projects === "all" ? "Fold projects with nothing on the canvas" : "Show every project", icon: <Folder className={ICON} />, run: set({ projects: d.projects === "all" ? undefined : "all" }) },
  ];
}

/** What the palette can do that only the surface knows. */
export interface PaletteHost {
  addMachine: () => void;
  showArchived: boolean;
  toggleArchived: () => void;
  archived: number;
  fit: () => void;
  /** The canvas's views (All, then the saved ones) and the one showing. */
  views: readonly { id: string; name: string }[];
  view: string;
  selectView: (id: string) => void;
  /** Which way the shown view's tree grows, and switching it (saved with the view). */
  direction: Direction;
  setDirection: (d: Direction) => void;
  /** The shown view's display options, and changing them (saved with the view). */
  display: Display;
  setDisplay: (d: Display) => void;
  /** Delete a saved view (All shows then). */
  deleteView: (id: string) => void;
  /** The project filling the canvas, and focusing one (`null`: everything again). */
  focused: string | null;
  focusProject: (project: string | null) => void;
}

/**
 * The agents' command palette (⌘K in the agents surface; the search button in its header), T3
 * Code's CommandPalette: jump to any conversation, project or machine, run an action, or type
 * what should get done and hand it to agents ("Orchestrate: …" starts an orchestrator that plans
 * it and spins up subagents on the canvas; "New agent: …" starts one). Keyboard-first: ↑↓ move,
 * Enter picks, Backspace on an empty query goes back, Esc goes back or closes.
 */
export function CommandPalette({ host }: { host: PaletteHost }) {
  const { open, intent, session } = usePalette();
  if (!open || typeof document === "undefined") return null;
  return createPortal(<PaletteDialog key={session} session={session} intent={intent} host={host} />, document.body);
}

function PaletteDialog({ session, intent, host }: { session: number; intent: PaletteIntent; host: PaletteHost }) {
  const ctx = useAgents();
  const { client, openThread, focus, startNew } = ctx;
  const data = client.data;
  const { view: surface } = useAgentsSurface();
  const launch = useLaunchGoal();
  const { target, targets } = useGoalTargets();
  const machineCount = data.machines.size;
  const [now] = useState(() => Date.now());
  const auto = useAutomations();
  const inboxActions = useInboxActions();

  const [stack, dispatch] = useReducer(reduceStack, undefined, () => ({ query: intent.kind === "search" ? (intent.query ?? "") : "", views: [] }));
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const input = useRef<HTMLInputElement>(null);
  const list = useRef<HTMLDivElement>(null);
  const view = stack.views.at(-1)?.view ?? null;
  const query = stack.query;

  const machines = useMemo(() => [...data.machines.values()].map((r) => r.value).sort((a, b) => Number(b.online) - Number(a.online) || a.name.localeCompare(b.name)), [data.machines]);
  const projects = useMemo(() => [...data.projects.values()].map((r) => ({ ...r.value, machine: r.machine })).sort((a, b) => a.name.localeCompare(b.name)), [data.projects]);
  const machineName = (id: string) => data.machines.get(id)?.value.name ?? "";
  const openThreadId = surface.kind === "thread" ? surface.thread : null;
  const openRecord = openThreadId ? data.threads.get(openThreadId) : undefined;

  // ── goals ──

  const goalItem = (kind: GoalKind, goal: string, t: GoalTarget, title?: string): PaletteAction => ({
    kind: "action",
    value: `goal:${kind}:${t.project.id}`,
    title: title ?? (kind === "orchestrate" ? `Orchestrate: ${goal}` : `New agent: ${goal}`),
    description: kind === "orchestrate" ? `Plans it and spins up agents · ${targetLabel(t, machineCount)}` : `One agent with this prompt · ${targetLabel(t, machineCount)}`,
    icon: kind === "orchestrate" ? <Workflow className="size-4 text-accent" /> : <Bot className={ICON} />,
    run: () => launch(kind, goal, t),
  });

  /** The goal's view: where it goes, orchestrated first. */
  const goalView = (goal: string): PaletteView => ({
    label: "Orchestrate",
    placeholder: "What should the agents get done?",
    initialQuery: goal,
    enterLabel: "Start",
    build: (q) => {
      const g = q.trim();
      if (!g) return [];
      if (targets.length === 0) return [{ value: "none", label: "", items: [addProjectFirst] }];
      return [
        { value: "orchestrate", label: "Orchestrate in", items: targets.map((t) => goalItem("orchestrate", g, t, targetLabel(t, machineCount))) },
        { value: "agent", label: "Or one agent in", items: targets.map((t) => goalItem("agent", g, t, targetLabel(t, machineCount))) },
      ];
    },
  });

  // ── projects and machines ──

  // Adding a project is the one dialog (project-new.tsx); it picks the machine when there's a choice.
  const online = machines.filter((m) => reachable(m));
  const addProject: PaletteAction = {
    kind: "action",
    value: "action:add-project",
    title: "Add project…",
    searchTerms: ["folder", "clone", "repo", "new repository"],
    icon: <FolderPlus className={ICON} />,
    disabled: online.length === 0,
    run: () => openProjectDialog({ kind: "add", machine: online.length === 1 ? online[0].id : null }),
  };
  const addProjectFirst: PaletteSubmenuLike = { ...addProject, value: "goal:add-project", title: "Add a project first", description: "Agents work in a project on one of your machines" };

  const threadRow = (t: Thread, machine: string, query: string): PaletteAction => {
    const projectRecord = data.projects.get(t.project)?.value;
    const project = projectRecord?.name;
    const branch = t.worktree?.branch;
    // A conversation off the canvas says which shelf it's on (the toolbar's Shelf lists it).
    const shelf = shelfOf(t, now);
    const parts = [
      shelf === "active" ? null : SHELF_NAME[shelf],
      project,
      machineCount > 1 ? machineName(machine) : null,
      branch ? `#${branch.replace(/^roll\//, "")}` : null,
      t.id === openThreadId ? "Open" : null,
    ].filter(Boolean);
    const opens = threadOpens()[t.id] ?? null;
    return {
      kind: "action",
      value: `thread:${t.id}`,
      title: t.title,
      description: isBusy(t.status) && t.activity ? <span className={t.status === "waiting" ? "text-accent" : "shimmer"}>{t.activity}</span> : parts.join(" · "),
      // The project's logo or monogram, so a thread reads as its project's at a glance; an orchestrator keeps its mark.
      icon: t.orchestrator ? <Workflow className={ICON} /> : projectRecord ? <ProjectIcon project={projectRecord} className="size-4" /> : <MessageSquare className={ICON} />,
      leading: <StatusDot status={t.status} />,
      trailing: t.change ? <span className="shrink-0 font-mono text-[11px] text-subtle">#{t.change.number}</span> : undefined,
      timestamp: shortAgo(Math.max(t.updated_at, opens?.openedAt ?? 0), now),
      threadSearch: {
        title: t.title,
        branch,
        change: t.change ? { number: t.change.number, repo: t.change.repo } : null,
        projectTitle: project,
        machineLabel: machineCount > 1 ? machineName(machine) : null,
        activityAt: t.updated_at,
        opens,
      },
      secondary: t.id === openThreadId,
      run: () => {
        noteThreadOpen(t.id, query.trim() && !query.startsWith(">") ? query : undefined);
        openThread(t.id);
      },
    };
  };

  // ── the root ──

  const groups = useMemo((): { root: PaletteGroup[]; threads: PaletteItem[]; projects: PaletteItem[]; machines: PaletteItem[] } => {
    const { requests } = rolling(data);
    const live = [...data.threads.values()].filter((r) => r.value.archived_at === null);
    const shelved = inbox(data, now).filter((g) => g.settled.length + g.snoozed.length + g.archived.length > 0);
    const projectsView = (label: string, run: (p: (typeof projects)[number]) => void): PaletteView => ({
      label,
      placeholder: "Which project?",
      groups: [{ value: "projects", label: "Projects", items: projects.map((p) => ({ kind: "action" as const, value: `${label}:${p.id}`, title: p.name, description: machineCount > 1 ? machineName(p.machine) : p.path, icon: <ProjectIcon project={p} className="size-4" />, run: () => run(p) })) }],
    });
    const open = openRecord?.value ?? null;
    const at = openRecord?.machine ?? "";
    const currentView = host.views.find((x) => x.id === host.view)?.name ?? "All";
    const actions: (PaletteItem | false)[] = [
      { kind: "submenu", value: "action:orchestrate", title: "Orchestrate…", searchTerms: ["goal", "plan", "spin up", "agents", "delegate", "prompt"], description: "Say what to get done: an orchestrator plans it and spins up agents", icon: <Workflow className="size-4 text-accent" />, view: goalView("") },
      { kind: "action", value: "action:new", title: "New agent", searchTerms: ["new thread", "launch", "start", "conversation"], icon: <Plus className={ICON} />, run: () => startNew?.() },
      projects.length > 0 && { kind: "submenu", value: "action:new-in", title: "New agent in…", searchTerms: ["new thread in", "project"], icon: <Plus className={ICON} />, view: projectsView("New agent in", (p) => startNew?.(p.id)) },
      requests.length > 0 && { kind: "action", value: "action:waiting", title: `Answer what's waiting (${requests.length})`, searchTerms: ["approval", "question", "request", "waiting"], icon: <Bell className="size-4 text-accent" />, run: () => openThread(requests[0].thread) },
      !!open && isBusy(open.status) && { kind: "action", value: "action:stop", title: `Stop “${open.title}”`, searchTerms: ["interrupt", "cancel"], icon: <Square className={ICON} />, run: () => client.run(at, { type: "run.interrupt", thread: open.id }) },
      !!open && { kind: "action", value: "action:fork", title: `Fork “${open.title}”`, searchTerms: ["branch", "copy"], icon: <GitFork className={ICON} />, run: async () => openThread((await client.run<{ type: "created"; id: string }>(at, { type: "thread.fork", thread: open.id, worktree: !!open.worktree }, "created")).id) },
      !!open && {
        kind: "action",
        value: "action:settle",
        title: open.inbox.settle?.state === "settled" ? `Unsettle “${open.title}”` : `Settle “${open.title}”`,
        searchTerms: ["settle", "unsettle", "done", "inbox", "resolve", "restore"],
        icon: <CircleCheck className={ICON} />,
        // Through the sidebar's actions: T3's undo notice, and settle and advance.
        run: () => (open.inbox.settle?.state === "settled" ? inboxActions.restore(open) : inboxActions.settle(open)),
      },
      !!open && open.inbox.snooze === null && open.archived_at === null && {
        kind: "submenu",
        value: "action:snooze",
        title: `Snooze “${open.title}”…`,
        searchTerms: ["later", "remind", "hide", "inbox"],
        icon: <AlarmClock className={ICON} />,
        view: {
          label: "Snooze",
          placeholder: "Until when?",
          groups: [
            {
              value: "presets",
              label: "Back on the canvas",
              items: snoozePresets(Date.now()).map((p) => ({ kind: "action" as const, value: `snooze:${p.until}`, title: p.label, trailing: <span className="text-[11.5px] text-subtle">{p.when}</span>, icon: <AlarmClock className={ICON} />, run: () => inboxActions.snooze(open, p.until) })),
            },
          ],
        },
      },
      !!open && open.inbox.snooze !== null && { kind: "action", value: "action:wake", title: `Wake “${open.title}”`, searchTerms: ["snooze", "unsnooze", "restore"], icon: <AlarmClockOff className={ICON} />, run: () => inboxActions.restore(open) },
      !!open && {
        kind: "action",
        value: "action:archive",
        title: open.archived_at === null ? `Archive “${open.title}”` : `Unarchive “${open.title}”`,
        searchTerms: ["archive", "unarchive", "settle", "hide", "done", "restore"],
        icon: open.archived_at === null ? <Archive className={ICON} /> : <ArchiveRestore className={ICON} />,
        run: () => (open.archived_at === null ? inboxActions.archive(open) : inboxActions.restore(open)),
      },
      shelved.length > 0 && {
        kind: "submenu",
        value: "action:shelf",
        title: "Settled and archived in…",
        searchTerms: ["settled", "archived", "snoozed", "shelf", "restore", "unarchive", "unsettle", "old conversations"],
        icon: <Archive className={ICON} />,
        view: {
          label: "Settled and archived in",
          placeholder: "Which project?",
          groups: [
            {
              value: "projects",
              label: "Projects",
              items: shelved.map((g) => ({
                kind: "action" as const,
                value: `shelf:${g.project.id}`,
                title: g.project.name,
                description: (["settled", "snoozed", "archived"] as const).filter((s) => g[s].length).map((s) => `${SHELF_NAME[s]} ${g[s].length}`).join(" · "),
                icon: <ProjectIcon project={g.project} className="size-4" />,
                // The canvas, with the toolbar's shelf open on just this project.
                run: () => (setAgentsView({ kind: "canvas" }), openShelf(g.project.id)),
              })),
            },
          ],
        },
      },
      addProject,
      { kind: "action", value: "action:add-machine", title: "Add machine", searchTerms: ["connect", "computer", "sandbox", "rent"], icon: <Server className={ICON} />, run: host.addMachine },
      !!auto.store && { kind: "action", value: "action:automations", title: "Automations", searchTerms: ["workflow", "trigger", "schedule", "cron", "webhook", "zap", "rules"], icon: <Zap className={ICON} />, shortcut: "A", run: () => setAgentsView({ kind: "automations" }) },
      !!auto.store && {
        kind: "action",
        value: "action:notices",
        title: unseenNotices(auto) > 0 ? `Notices (${unseenNotices(auto)} new)` : "Notices",
        searchTerms: ["notifications", "inbox", "missed", "alerts", "notify"],
        icon: <Inbox className={ICON} />,
        run: () => setAgentsView({ kind: "automations", list: "notices" }),
      },
      !!auto.store && { kind: "action", value: "action:new-automation", title: "New automation", searchTerms: ["workflow", "trigger", "schedule", "template"], icon: <Plus className={ICON} />, run: () => setAgentsView({ kind: "automations", open: "new" }) },
      auto.automations.size > 0 && {
        kind: "submenu",
        value: "action:run-automation",
        title: "Run automation…",
        searchTerms: ["workflow", "trigger", "start"],
        icon: <Play className={ICON} />,
        view: {
          label: "Run automation",
          placeholder: "Which automation?",
          groups: [
            {
              value: "automations",
              label: "Automations",
              items: [...auto.automations.values()].map((x) => ({
                kind: "action" as const,
                value: `run-automation:${x.id}`,
                title: x.name,
                description: x.enabled ? x.description : "Paused",
                icon: <Zap className={ICON} />,
                run: async () => {
                  const s = auto.store!;
                  const r = await s.api.run(s.owner, x.id, {});
                  s.putRun(r);
                  setAgentsView({ kind: "automations", open: x.id, tab: "runs", run: r.id });
                },
              })),
            },
          ],
        },
      },
      projects.length > 1 && { kind: "submenu", value: "action:focus-project", title: "Focus a project…", searchTerms: ["zoom", "only", "isolate", "solo", "canvas"], icon: <Focus className={ICON} />, shortcut: "F", view: projectsView("Focus a project", (p) => host.focusProject(p.id)) },
      !!host.focused && { kind: "action", value: "action:unfocus", title: "Show every project", searchTerms: ["focus", "unfocus", "all", "everything"], icon: <Focus className={ICON} />, shortcut: "Esc", run: () => host.focusProject(null) },
      projects.length > 0 && { kind: "submenu", value: "action:project-settings", title: "Project settings", searchTerms: ["auto-pull", "defaults", "scripts", "configure"], icon: <Settings2 className={ICON} />, view: projectsView("Project settings", (p) => setAgentsView({ kind: "settings", target: `p:${p.id}` })) },
      machines.length > 0 && {
        kind: "submenu",
        value: "action:machine-settings",
        title: "Machine settings",
        searchTerms: ["providers", "configure", "computer"],
        icon: <Settings2 className={ICON} />,
        view: { label: "Machine settings", placeholder: "Which machine?", groups: [{ value: "machines", label: "Machines", items: machines.map((m) => ({ kind: "action" as const, value: `machine-settings:${m.id}`, title: m.name, icon: <MachineIcon machine={m} className={ICON} />, run: () => setAgentsView({ kind: "settings", target: `m:${m.id}` }) })) }] },
      },
      host.archived > 0 && { kind: "action", value: "action:archived", title: host.showArchived ? "Hide archived conversations" : `Show archived conversations (${host.archived})`, searchTerms: ["settled", "archive"], icon: <Archive className={ICON} />, run: host.toggleArchived },
      host.views.length > 1 && {
        kind: "submenu",
        value: "action:views",
        title: "Switch view",
        searchTerms: ["canvas", "filter", "layout"],
        icon: <LayoutGrid className={ICON} />,
        view: {
          label: "Switch view",
          placeholder: "Which view?",
          groups: [
            {
              value: "views",
              label: "Views",
              items: host.views.map((v) => ({
                kind: "action" as const,
                value: `view:${v.id}`,
                title: v.name,
                trailing: v.id === host.view ? <Check className="size-3.5 text-subtle" /> : undefined,
                icon: <LayoutGrid className={ICON} />,
                run: () => (setAgentsView({ kind: "canvas" }), host.selectView(v.id)),
              })),
            },
          ],
        },
      },
      { kind: "action", value: "action:new-view", title: "New view…", searchTerms: ["save", "filter", "bookmark", "canvas"], icon: <Bookmark className={ICON} />, shortcut: "⌥S", run: () => (setAgentsView({ kind: "canvas" }), requestView("new")) },
      host.view !== ALL && { kind: "action", value: "action:rename-view", title: `Rename view “${currentView}”…`, searchTerms: ["view", "name"], icon: <Pencil className={ICON} />, run: () => (setAgentsView({ kind: "canvas" }), requestView("rename")) },
      host.view !== ALL && {
        kind: "submenu",
        value: "action:delete-view",
        title: `Delete view “${currentView}”…`,
        searchTerms: ["view", "remove"],
        icon: <Trash2 className={ICON} />,
        view: { label: "Delete view", placeholder: "Delete it?", groups: [{ value: "confirm", label: "It can't be undone", items: [{ kind: "action" as const, value: "delete-view:yes", title: `Delete “${currentView}”`, icon: <Trash2 className={ICON} />, run: () => host.deleteView(host.view) }] }] },
      },
      {
        kind: "submenu",
        value: "action:display",
        title: "Display options…",
        searchTerms: ["density", "compact", "comfortable", "hide", "idle", "terminals", "group", "projects", "layout"],
        icon: <SlidersHorizontal className={ICON} />,
        view: { label: "Display options", placeholder: "Change how the canvas shows…", groups: [{ value: "display", label: currentView, items: displayItems(host) }] },
      },
      { kind: "action", value: "action:settings", title: "Agents settings", searchTerms: ["owner", "preferences", "configure", "providers", "endpoints", "gateway", "integrations"], icon: <Settings2 className={ICON} />, shortcut: "⌘,", run: () => setAgentsView({ kind: "settings", target: "owner" }) },
      { kind: "action", value: "action:fit", title: "Fit the canvas", searchTerms: ["zoom", "view", "overview", "all"], icon: <Maximize className={ICON} />, shortcut: "1", run: () => (setAgentsView({ kind: "canvas" }), host.fit()) },
      host.direction === "vertical"
        ? { kind: "action", value: "action:direction", title: "Lay out left to right", searchTerms: ["horizontal", "direction", "layout", "sideways", "tree"], icon: <ArrowRightFromLine className={ICON} />, shortcut: "⇧D", run: () => (setAgentsView({ kind: "canvas" }), host.setDirection("horizontal")) }
        : { kind: "action", value: "action:direction", title: "Lay out top-down", searchTerms: ["vertical", "direction", "layout", "downwards", "tree"], icon: <ArrowDownFromLine className={ICON} />, shortcut: "⇧D", run: () => (setAgentsView({ kind: "canvas" }), host.setDirection("vertical")) },
    ];
    const opens = threadOpens();
    const used = (r: (typeof live)[number]) => Math.max(r.value.updated_at, opens[r.value.id]?.openedAt ?? 0);
    // Recents: what was used last; a delegated task only while it works (it leaves the canvas when done).
    const recent = live
      .filter((r) => r.value.lineage?.relation !== "subagent" || isBusy(r.value.status))
      .sort((a, b) => used(b) - used(a))
      .slice(0, RECENT_THREAD_LIMIT)
      .map((r) => threadRow(r.value, r.machine, ""));
    const root: PaletteGroup[] = [{ value: "actions", label: "Actions", items: actions.filter((a): a is PaletteItem => !!a) }];
    if (recent.length) root.push({ value: "recent", label: "Recent", items: recent });
    return {
      root,
      // Archived ones too: searching is how an old conversation is found.
      threads: [...data.threads.values()].map((r) => threadRow(r.value, r.machine, query)),
      // "Project: roll": the canvas on it, then what can be done with it (its card's actions).
      projects: projects.map((p) => ({
        kind: "submenu" as const,
        value: `project:${p.id}`,
        title: `Project: ${p.name}`,
        searchTerms: [p.name, p.path, p.repo ? `${p.repo.owner}/${p.repo.repo}` : "", machineCount > 1 ? machineName(p.machine) : ""].filter(Boolean),
        description: [machineCount > 1 ? machineName(p.machine) : null, p.path].filter(Boolean).join(" · "),
        icon: <ProjectIcon project={p} className="size-4" />,
        view: {
          label: p.name,
          placeholder: `Do what with ${p.name}?`,
          groups: [
            {
              value: "project",
              label: p.name,
              items: [
                { kind: "action" as const, value: `project:${p.id}:show`, title: "Show on the canvas", icon: <Focus className={ICON} />, run: () => focus(`p:${p.id}`) },
                ...projectActions({ ...ctx, canvas: NO_CANVAS }, p).map((a) => ({
                  kind: "action" as const,
                  value: `project:${p.id}:${a.label}`,
                  title: a.label,
                  disabled: a.disabled,
                  icon: <span className="grid size-4 place-items-center text-subtle [&>svg]:size-4 [&>svg]:stroke-[1.75]">{a.icon}</span>,
                  run: a.onSelect,
                })),
              ],
            },
          ],
        },
      })),
      machines: machines.map((m) => ({
        kind: "action" as const,
        value: `machine:${m.id}`,
        title: m.name,
        searchTerms: [m.info?.hostname ?? "", m.sandbox ? "sandbox" : "computer"].filter(Boolean),
        description: m.online ? "Online" : m.sandbox ? "Asleep · wakes when an agent needs it" : "Offline",
        icon: <MachineIcon machine={m} className={ICON} />,
        leading: <StatusDot status={m.online ? "online" : "offline"} />,
        run: () => focus(`m:${m.id}`),
      })),
    };
    // Rebuilt when the records change; `query` only tags thread picks.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [data, projects, machines, openRecord, host, targets, query, auto.automations, auto.store, auto.notices, auto.noticesSeen, inboxActions, ctx]);

  const shown: PaletteGroup[] = useMemo(() => {
    if (view?.panel) return [];
    if (view?.build) return [...view.build(query)];
    const goal = query.trim();
    const goals: PaletteItem[] =
      !view && goal && !goal.startsWith(">")
        ? target
          ? [
              goalItem("orchestrate", goal, target),
              goalItem("agent", goal, target),
              ...(targets.length > 1 ? [{ kind: "submenu" as const, value: "goal:where", title: "Somewhere else…", description: "Pick the project", icon: <Folder className={ICON} />, view: goalView(goal) }] : []),
            ]
          : [addProjectFirst]
        : [];
    return filterGroups({
      groups: view?.groups ?? groups.root,
      query,
      inSubmenu: !!view,
      root: { groups: groups.root, nested: nestedItems(groups.root[0].items, query) },
      threads: groups.threads,
      projects: groups.projects,
      machines: groups.machines,
      goals,
      now,
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [view, query, groups, target, targets, now]);

  const { rows, values } = useMemo(() => buildRows(shown), [shown]);
  const [highlighted, setHighlighted] = useState<string | null>(null);
  // The first row on every new query or view (T3's autoHighlight); kept while the rows update live.
  const firstRow = autoHighlight(shown, values, view ? "" : query);
  useLayoutEffect(() => setHighlighted(firstRow), [query, view]); // eslint-disable-line react-hooks/exhaustive-deps
  useLayoutEffect(() => {
    if (highlighted === null ? firstRow !== null : !values.includes(highlighted)) setHighlighted(firstRow);
  }, [values, highlighted, firstRow]);
  useEffect(() => {
    if (!highlighted) return;
    [...(list.current?.querySelectorAll<HTMLElement>("[data-value]") ?? [])].find((el) => el.dataset.value === highlighted)?.scrollIntoView({ block: "nearest" });
  }, [highlighted]);

  // Straight into a view when opened that way.
  useEffect(() => {
    if (intent.kind === "orchestrate") dispatch({ type: "reset", view: goalView(intent.goal ?? "") });
    else if (intent.kind === "new-in") {
      const item = groups.root[0].items.find((i) => i.value === "action:new-in");
      if (item?.kind === "submenu") dispatch({ type: "reset", view: item.view });
    } else if (intent.kind === "add-project" && !addProject.disabled) {
      // The palette steps aside for the dialog.
      closePalette();
      void addProject.run();
    }
    // Once, where it opens.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  useEffect(() => input.current?.focus({ preventScroll: true }), [view]);

  const [leaving, setLeaving] = useState(false);
  const close = () => {
    if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) return closePalette();
    setLeaving(true);
    // Only this palette: ⌘K may have opened a new one meanwhile.
    setTimeout(() => agentsPalette.get().session === session && closePalette(), 140);
  };

  const execute = async (item: PaletteItem) => {
    if (item.disabled || busy) return;
    setError(null);
    if (item.kind === "submenu") return dispatch({ type: "push", view: item.view });
    try {
      const out = item.run();
      if (out instanceof Promise) {
        setBusy(true);
        await out;
      }
      if (!item.keepOpen) close();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const move = (by: number) => {
    if (values.length === 0) return;
    const at = highlighted ? values.indexOf(highlighted) : -1;
    setHighlighted(values[(at + by + values.length) % values.length]);
  };

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.nativeEvent.isComposing || e.keyCode === 229) return;
    if (e.key === "ArrowDown" || (e.ctrlKey && e.key === "n")) {
      e.preventDefault();
      move(1);
    } else if (e.key === "ArrowUp" || (e.ctrlKey && e.key === "p")) {
      e.preventDefault();
      move(-1);
    } else if (e.key === "Enter") {
      e.preventDefault();
      const item = findItem(shown, highlighted);
      if (item) void execute(item);
    } else if (e.key === "Backspace" && query === "" && view) {
      e.preventDefault();
      dispatch({ type: "back" });
    } else if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      if (view) dispatch({ type: "back" });
      else close();
    }
  };

  const picked = findItem(shown, highlighted);
  // A thread or a project opens; anything else runs.
  const enterLabel = view?.enterLabel ?? (picked?.kind === "submenu" || /^(thread|project):/.test(picked?.value ?? "") ? "Open" : "Run");
  const empty = view?.build ? (query.trim() ? "Add a project to give agents a goal." : `Say what should get done. The orchestrator plans it and spins up agents${target ? ` in ${targetLabel(target, machineCount)}` : ""}.`) : query.startsWith(">") ? "No matching actions." : "Nothing matches.";

  return (
    <div className="fixed inset-0 z-[60]" data-palette>
      <div className={`palette-backdrop absolute inset-0 transition-opacity duration-200 ${leaving ? "opacity-0" : "fade-in"}`} onPointerDown={close} aria-hidden />
      <div className="pointer-events-none absolute inset-0 flex flex-col items-center px-3 pt-[max(1rem,4vh)] sm:pt-[12vh]">
        <div
          role="dialog"
          aria-modal="true"
          aria-label="Command palette"
          className={`palette-glass pointer-events-auto flex max-h-[min(30rem,80vh)] min-h-0 w-full max-w-xl flex-col overflow-hidden rounded-2xl text-fg transition-[opacity,scale] duration-150 ease-in ${leaving ? "scale-[0.98] opacity-0" : "palette-in"}`}
        >
          <div className="flex items-center gap-2 px-3.5">
            {view ? (
              <button onClick={() => dispatch({ type: "back" })} className="-ml-1 flex shrink-0 items-center gap-1 rounded-md py-1 pr-1.5 pl-0.5 text-[12px] text-muted transition hover:bg-fg/[0.06] hover:text-fg" aria-label="Back" title="Back (Backspace)">
                <ChevronLeft className="size-3.5" />
                <span className="max-w-40 truncate">{view.label}</span>
              </button>
            ) : (
              <Search className="size-4 shrink-0 text-subtle" />
            )}
            {view?.panel ? (
              <div className="h-12 flex-1" />
            ) : (
              <input
                ref={input}
                value={query}
                onChange={(e) => {
                  setError(null);
                  dispatch({ type: "query", query: e.target.value });
                }}
                onKeyDown={onKeyDown}
                placeholder={placeholderFor(view)}
                aria-label="Search or say what to do"
                aria-controls="palette-list"
                aria-activedescendant={highlighted ? `palette-${highlighted}` : undefined}
                role="combobox"
                aria-expanded="true"
                autoComplete="off"
                spellCheck={false}
                className="h-12 min-w-0 flex-1 bg-transparent text-[14.5px] text-fg outline-none placeholder:text-subtle"
              />
            )}
            {busy && <span className="shimmer shrink-0 text-[12px] text-muted">Starting…</span>}
          </div>
          <div className="h-px shrink-0 bg-fg/[0.07]" />
          {view?.panel ? (
            <div key={view.label} className="palette-view min-h-0 overflow-y-auto" onKeyDown={(e) => e.key === "Escape" && (e.stopPropagation(), dispatch({ type: "back" }))}>
              {view.panel(close)}
            </div>
          ) : rows.length === 0 ? (
            <p className="px-6 py-9 text-center text-[13px] text-muted">{empty}</p>
          ) : (
            <div ref={list} id="palette-list" role="listbox" key={view?.label ?? "root"} className="palette-view min-h-0 overflow-y-auto overscroll-contain p-2">
              {rows.map((row, i) =>
                row.kind === "label" ? (
                  <p key={row.key} className={`px-2 pb-1 text-[11.5px] font-medium text-subtle ${i === 0 ? "pt-1" : "pt-2.5"}`}>
                    {row.label}
                  </p>
                ) : (
                  <Row key={row.key} item={row.item} active={row.item.value === highlighted} onHover={() => row.index !== null && setHighlighted(row.item.value)} onPick={() => void execute(row.item)} />
                ),
              )}
            </div>
          )}
          {error && <p className="border-t border-fg/[0.07] px-4 py-2 text-[12.5px] text-danger">{error}</p>}
          <div className="flex shrink-0 items-center gap-3 border-t border-fg/[0.07] bg-fg/[0.025] px-4 py-2.5 text-[12px] font-medium text-muted max-sm:hidden">
            <span className="flex items-center gap-1">
              <Kbd>
                <ArrowUp className="size-3" />
              </Kbd>
              <Kbd>
                <ArrowDown className="size-3" />
              </Kbd>
              Navigate
            </span>
            {!view?.panel && (
              <span className="flex items-center gap-1">
                <Kbd>Enter</Kbd>
                {enterLabel}
              </span>
            )}
            {view && (
              <span className="flex items-center gap-1">
                <Kbd>Backspace</Kbd>
                Back
              </span>
            )}
            <span className="flex items-center gap-1">
              <Kbd>Esc</Kbd>
              {view ? "Back" : "Close"}
            </span>
            {!view && <span className="ml-auto truncate text-subtle">Type a goal to hand it to agents</span>}
          </div>
        </div>
      </div>
    </div>
  );
}

type PaletteSubmenuLike = PaletteItem;

function Row({ item, active, onHover, onPick }: { item: PaletteItem; active: boolean; onHover: () => void; onPick: () => void }) {
  const second = item.matchHint ? <HintLine hint={item.matchHint} /> : item.description ? <span className="truncate text-[12px] text-subtle">{item.description}</span> : null;
  return (
    <div
      id={`palette-${item.value}`}
      data-value={item.value}
      role="option"
      aria-selected={active}
      aria-disabled={item.disabled || undefined}
      onMouseMove={item.disabled ? undefined : onHover}
      onMouseDown={(e) => e.preventDefault()}
      onClick={item.disabled ? undefined : onPick}
      className={`flex min-h-8 items-center gap-2.5 rounded-lg px-2 py-1.5 text-[13px] select-none ${item.disabled ? "opacity-60" : "cursor-pointer"} ${active ? "bg-fg/[0.08]" : ""}`}
    >
      <span className={`flex h-5 shrink-0 items-center ${second ? "self-start pt-0.5" : ""}`}>{item.icon}</span>
      <span className="flex min-w-0 flex-1 flex-col">
        <span className="flex min-w-0 items-center gap-1.5 text-fg">
          {item.leading}
          <span className="truncate">{item.title}</span>
        </span>
        {second}
      </span>
      {item.trailing}
      {item.timestamp && <span className="min-w-10 shrink-0 text-right text-[11.5px] text-subtle tabular-nums">{item.timestamp}</span>}
      {item.shortcut && <Kbd>{item.shortcut}</Kbd>}
      {item.kind === "submenu" && <ChevronRight className="-mr-0.5 size-4 shrink-0 text-subtle" />}
    </div>
  );
}

/** Why a row matched when its title doesn't show it: the field and its match (T3's `SearchMatchHintLine`). */
function HintLine({ hint }: { hint: SearchMatchHint }) {
  return (
    <span className="truncate text-[12px] text-subtle">
      <span className="opacity-80">{hint.label}:</span>{" "}
      {splitSearchText(hint.text, hint.ranges).map((p) =>
        p.highlighted ? (
          <mark key={p.start} className="bg-transparent font-semibold text-fg">
            {p.text}
          </mark>
        ) : (
          p.text
        ),
      )}
    </span>
  );
}

function Kbd({ children }: { children: ReactNode }) {
  return <kbd className="inline-flex h-5 min-w-5 items-center justify-center rounded bg-fg/[0.08] px-1 font-sans text-[11px] font-medium text-fg">{children}</kbd>;
}

/** "now", "5m", "3h", "2d", "Mar 4": T3's compact relative times. */
export function shortAgo(at: number, now = Date.now()): string {
  const s = Math.max(0, Math.round((now - at) / 1000));
  if (s < 60) return "now";
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h`;
  const d = Math.floor(h / 24);
  if (d < 7) return `${d}d`;
  return new Date(at).toLocaleDateString(undefined, { month: "short", day: "numeric" });
}
