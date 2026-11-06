"use client";

/**
 * ⌘K for working at scale (docs/code/agents.md, "Working at scale"): the view as a canvas, list or
 * board, its grouping and order, "Next that needs me", and every bulk action on the selection.
 * The palette (`command-palette.tsx`) spreads these into its actions.
 */

import { AlarmClock, Archive, ArrowDownWideNarrow, ArrowUpNarrowWide, BellRing, Check, CircleStop, Columns3, Cpu, GitPullRequestArrow, Layers, ListOrdered, Network, RotateCcw, Rows3, ThumbsDown, ThumbsUp, X } from "lucide-react";
import { applicable, type BulkSpec } from "@/lib/agents-bulk";
import { firstDir, groupLabel, sortLabel } from "@/lib/agents-collection";
import { snoozePresets } from "@/lib/agents-inbox";
import { selection } from "@/lib/agents-selection";
import type { AgentsData } from "@/lib/agents-store";
import { setAgentsView } from "@/lib/agents-surface";
import { GROUP_BYS, groupByOf, layoutOf, SORT_KEYS, sortOf, subGroupByOf, withLayout, type Display, type Layout } from "@/lib/agents-views";
import type { PaletteItem } from "./command-palette.logic";

const ICON = "size-4 text-subtle";

export interface ScaleHost {
  display: Display;
  setDisplay: (d: Display) => void;
  nextNeedsMe: () => void;
  bulk: (spec: BulkSpec, ids: readonly string[]) => void;
}

const LAYOUT_ROWS: Record<Layout, { title: string; key: string; icon: React.ReactNode; terms: string[] }> = {
  canvas: { title: "Show as canvas", key: "⇧C", icon: <Network className={ICON} />, terms: ["layout", "tree", "graph", "view"] },
  list: { title: "Show as list", key: "⇧L", icon: <Rows3 className={ICON} />, terms: ["layout", "table", "rows", "view", "linear"] },
  board: { title: "Show as board", key: "⇧B", icon: <Columns3 className={ICON} />, terms: ["layout", "kanban", "columns", "view", "linear"] },
};

/** The palette's rows for layouts, grouping, ordering, "Next that needs me" and the selection. */
export function scaleItems(host: ScaleHost, data: AgentsData): PaletteItem[] {
  const d = host.display;
  const layout = layoutOf(d);
  const set = (patch: Partial<Display>) => () => {
    setAgentsView({ kind: "canvas" });
    host.setDisplay({ ...d, ...patch });
  };
  const items: PaletteItem[] = [
    { kind: "action", value: "action:next-needs-me", title: "Next that needs me", searchTerms: ["waiting", "approval", "question", "next", "inbox", "jump"], icon: <BellRing className="size-4 text-accent" />, shortcut: "N", run: host.nextNeedsMe },
  ];
  for (const l of ["canvas", "list", "board"] as const) {
    if (l === layout) continue;
    const r = LAYOUT_ROWS[l];
    items.push({ kind: "action", value: `layout:${l}`, title: r.title, searchTerms: r.terms, icon: r.icon, shortcut: r.key, run: () => (setAgentsView({ kind: "canvas" }), host.setDisplay(withLayout(d, l))) });
  }
  // Grouping and order: the list's and the board's (on the canvas they show it as a list first).
  const asCollection = (patch: Partial<Display>) => set({ ...(layout === "canvas" ? { layout: "list" as const } : {}), ...patch });
  const by = groupByOf(d);
  const sub = subGroupByOf(d);
  const sort = sortOf(d);
  items.push(
    {
      kind: "submenu",
      value: "action:group-by",
      title: "Group by…",
      searchTerms: ["grouping", "group", "columns", "status", "project", "machine", "agent", "pr"],
      icon: <Layers className={ICON} />,
      view: {
        label: "Group by",
        placeholder: "Group threads by…",
        groups: [{ value: "group", label: layout === "board" ? "Columns" : "Groups", items: GROUP_BYS.map((g) => ({ kind: "action" as const, value: `group:${g}`, title: groupLabel[g], trailing: g === by && layout !== "canvas" ? <Check className="size-3.5 text-subtle" /> : undefined, icon: <Layers className={ICON} />, run: asCollection({ groupBy: g === "status" ? undefined : g, ...(g === sub ? { subGroupBy: undefined } : {}) }) })) }],
      },
    },
    {
      kind: "submenu",
      value: "action:sub-group-by",
      title: "Sub-group by…",
      searchTerms: ["swimlanes", "sub-grouping", "nested"],
      icon: <Layers className={ICON} />,
      view: {
        label: "Sub-group by",
        placeholder: "Inside each group, by…",
        groups: [{ value: "sub", label: layout === "board" ? "Swimlanes" : "Sub-groups", items: GROUP_BYS.filter((g) => g !== by || g === "none").map((g) => ({ kind: "action" as const, value: `sub:${g}`, title: groupLabel[g], trailing: g === sub && layout !== "canvas" ? <Check className="size-3.5 text-subtle" /> : undefined, icon: <Layers className={ICON} />, run: asCollection({ subGroupBy: g === "none" ? undefined : g }) })) }],
      },
    },
    {
      kind: "submenu",
      value: "action:order-by",
      title: "Order by…",
      searchTerms: ["sort", "ordering", "activity", "cost", "title", "created", "changes"],
      icon: <ListOrdered className={ICON} />,
      view: {
        label: "Order by",
        placeholder: "Order threads by…",
        groups: [{ value: "sort", label: "Order", items: SORT_KEYS.map((k) => ({ kind: "action" as const, value: `sort:${k}`, title: sortLabel[k], trailing: k === sort.key && layout !== "canvas" ? <Check className="size-3.5 text-subtle" /> : undefined, icon: <ListOrdered className={ICON} />, run: asCollection({ sort: k === "activity" && firstDir(k) === "desc" ? undefined : { key: k, dir: firstDir(k) } }) })) }],
      },
    },
  );
  if (layout !== "canvas") {
    const dir = sort.dir === "asc" ? "desc" : "asc";
    items.push({ kind: "action", value: "action:reverse-order", title: `Order ${dir === "asc" ? "ascending" : "descending"}`, searchTerms: ["reverse", "sort", "direction"], icon: dir === "asc" ? <ArrowUpNarrowWide className={ICON} /> : <ArrowDownWideNarrow className={ICON} />, run: set({ sort: sort.key === "activity" && dir === "desc" ? undefined : { key: sort.key, dir } }) });
  }

  // The selection's actions.
  const ids = [...selection.get().ids];
  if (!ids.length) return items;
  const n = ids.length;
  const can = applicable(data, ids);
  const go = (spec: BulkSpec) => () => host.bulk(spec, ids);
  const of = (k: number) => (k < n ? `${k} of ${n}` : `${n}`);
  const terms = ["selected", "bulk", "all", "selection"];
  const bulk: (PaletteItem | false)[] = [
    can.settle > 0 && { kind: "action", value: "bulk:settle", title: `Settle ${of(can.settle)} selected`, searchTerms: terms, icon: <Check className={ICON} />, run: go({ kind: "settle" }) },
    can.archive > 0 && { kind: "action", value: "bulk:archive", title: `Archive ${of(can.archive)} selected`, searchTerms: terms, icon: <Archive className={ICON} />, run: go({ kind: "archive" }) },
    can.snooze > 0 && {
      kind: "submenu",
      value: "bulk:snooze",
      title: `Snooze ${of(can.snooze)} selected…`,
      searchTerms: terms,
      icon: <AlarmClock className={ICON} />,
      view: { label: "Snooze", placeholder: "Until…", groups: [{ value: "presets", label: "Until", items: snoozePresets(Date.now()).map((p) => ({ kind: "action" as const, value: `snooze:${p.label}`, title: p.label, description: p.when, icon: <AlarmClock className={ICON} />, run: go({ kind: "snooze", until: p.until }) })) }] },
    },
    can.approve > 0 && { kind: "action", value: "bulk:approve", title: `Approve ${of(can.approve)} selected`, searchTerms: [...terms, "accept", "yes"], icon: <ThumbsUp className={ICON} />, run: go({ kind: "approve" }) },
    can.deny > 0 && { kind: "action", value: "bulk:deny", title: `Decline ${of(can.deny)} selected`, searchTerms: [...terms, "deny", "reject", "no"], icon: <ThumbsDown className={ICON} />, run: go({ kind: "deny" }) },
    can.stop > 0 && { kind: "action", value: "bulk:stop", title: `Stop ${of(can.stop)} selected`, searchTerms: [...terms, "interrupt", "cancel"], icon: <CircleStop className={ICON} />, run: go({ kind: "stop" }) },
    can.retry > 0 && { kind: "action", value: "bulk:retry", title: `Retry ${of(can.retry)} failed`, searchTerms: [...terms, "restart", "again"], icon: <RotateCcw className={ICON} />, run: go({ kind: "retry" }) },
    can.publish > 0 && { kind: "action", value: "bulk:publish", title: `Publish ${of(can.publish)} selected`, searchTerms: [...terms, "change", "pr", "land"], icon: <GitPullRequestArrow className={ICON} />, run: go({ kind: "publish" }) },
    { kind: "action", value: "bulk:clear", title: `Clear the selection (${n})`, searchTerms: [...terms, "deselect", "none"], icon: <X className={ICON} />, shortcut: "Esc", run: () => selection.clear() },
  ];
  const models = modelRows(data, ids, go);
  if (models.length) bulk.splice(bulk.length - 1, 0, { kind: "submenu", value: "bulk:model", title: `Set the model of ${n} selected…`, searchTerms: [...terms, "model", "switch"], icon: <Cpu className={ICON} />, view: { label: "Set model", placeholder: "Which model?", groups: models } });
  return [...bulk.filter((b): b is PaletteItem => !!b), ...items];
}

function modelRows(data: AgentsData, ids: readonly string[], go: (spec: BulkSpec) => () => void) {
  const providers = new Map<string, string>();
  for (const id of ids) {
    const r = data.threads.get(id);
    if (r && !providers.has(r.value.provider)) providers.set(r.value.provider, r.machine);
  }
  return [...providers].flatMap(([provider, machine]) => {
    const p = data.machines.get(machine)?.value.info?.providers.find((x) => x.id === provider);
    if (!p?.models.length) return [];
    return [{ value: `models:${provider}`, label: p.name, items: p.models.filter((m) => !m.legacy).map((m) => ({ kind: "action" as const, value: `model:${provider}:${m.id}`, title: m.name, icon: <Cpu className={ICON} />, run: go({ kind: "model", provider, model: m.id }) })) }];
  });
}
