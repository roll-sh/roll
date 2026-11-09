/**
 * Working on many threads at once (docs/code/agents.md, "Working at scale"): the bulk bar, a strip
 * of the frame under the views bar while threads are selected (Linear's and T3's bulk actions,
 * never a floating bar), and what works everywhere threads show: x selects the focused thread,
 * Escape clears, N goes to the next thread that needs you, and ⌘/Shift-clicked canvas cards are
 * selected too. Each action is idempotent per thread (`lib/agents-bulk.ts`) and ends in one notice,
 * "12 settled · 1 failed (why)", with Undo where the single action has one; the threads that
 * failed stay selected, with Retry.
 */

import { AlarmClock, Archive, ArchiveRestore, Check, ChevronDown, CircleStop, Cpu, GitPullRequestArrow, MessageSquarePlus, RotateCcw, Send, ThumbsDown, ThumbsUp, X } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import type { MachineId, ProviderInfo, ThreadId } from "@/lib/agents-api";
import type { AgentsClient } from "@/lib/agents-client";
import { applicable, bulkLabel, planBulk, retryPlan, runBulk, skipSummary, summarize, undoPlan, byThread, type BulkPlan, type BulkResult, type BulkSpec, type BulkStep } from "@/lib/agents-bulk";
import { snoozePresets } from "@/lib/agents-inbox";
import { selection, useSelection } from "@/lib/agents-selection";
import type { AgentsData } from "@/lib/agents-store";
import { Collapse } from "./animated-height";
import { showInboxToast } from "./threads-sidebar";
import { Menu as MenuRoot, MenuGroup, MenuGroupLabel, MenuItem, MenuPopup, MenuTrigger } from "./ui/menu";

interface Running {
  label: string;
  done: number;
  total: number;
}

/** Runs bulk actions for the page: progress, the notice, Undo, and the failures kept selected for Retry. */
export function useBulk(client: AgentsClient, readOnly = false) {
  const [running, setRunning] = useState<Running | null>(null);
  const [failed, setFailed] = useState<{ result: BulkResult; why: string } | null>(null);
  const live = useRef(true);
  useEffect(() => {
    live.current = true;
    return () => {
      live.current = false;
    };
  }, []);
  const send = useCallback((m: MachineId, command: Parameters<typeof client.send>[1], id: string) => client.send(m, command, id), [client]);

  const go = useCallback(
    async (plan: BulkPlan, opts: { ids?: Map<BulkStep, string>; undo?: boolean } = {}) => {
      const label = opts.undo ? "Undoing" : `${bulkLabel[plan.spec.kind]}`;
      setRunning({ label, done: 0, total: plan.steps.length });
      const result = await runBulk(plan, send, { ids: opts.ids, onProgress: (done, total) => live.current && setRunning({ label, done, total }) });
      if (!live.current) return result;
      setRunning(null);
      const { failed: bad, done } = byThread(result);
      const text = opts.undo ? `Undone: ${done.length} put back${bad.length ? ` · ${bad.length} failed (${bad[0].why})` : ""}` : summarize(result);
      const undo = opts.undo ? null : undoPlan(result);
      showInboxToast({ text, error: bad.length > 0, undo: undo ? () => void go(undo, { undo: true }) : undefined });
      if (bad.length) {
        // The failures stay picked, so Retry (or another action) is one click away.
        selection.replace(bad.map((f) => f.thread));
        setFailed({ result, why: bad[0].why });
      } else {
        setFailed(null);
        if (!opts.undo) selection.clear();
      }
      return result;
    },
    [send],
  );

  const run = useCallback(
    (spec: BulkSpec, ids: readonly ThreadId[]) => {
      if (readOnly || running) return;
      const plan = planBulk(client.data, ids, spec);
      if (!plan.steps.length) {
        showInboxToast({ text: `Nothing to ${bulkLabel[spec.kind].toLowerCase()}: ${skipSummary(plan.skipped) ?? "no thread selected"}`, error: true });
        return;
      }
      setFailed(null);
      void go(plan);
    },
    [client, go, readOnly, running],
  );
  const retry = useCallback(() => {
    if (!failed || running) return;
    const r = retryPlan(failed.result);
    if (r) void go(r.plan, { ids: r.ids });
  }, [failed, go, running]);
  const dismissFailed = useCallback(() => setFailed(null), []);
  // Stable while nothing changes, so the memoised canvas view doesn't render again.
  return useMemo(() => ({ run, retry, running, failed, dismissFailed }), [run, retry, running, failed, dismissFailed]);
}

export type Bulk = ReturnType<typeof useBulk>;

/**
 * The page's selection keys and the canvas's share of it: x toggles the focused thread (a list
 * row, a board card, a canvas card or a sidebar row), Escape clears (before Escape closes
 * anything), ⌘/Ctrl-click and Shift-click on a canvas card select it instead of opening it, and
 * selected cards are marked (one stylesheet, so no card renders again).
 */
export function SelectionHost({ data }: { data: AgentsData }) {
  const sel = useSelection();
  // Threads that are gone leave the selection.
  useEffect(() => {
    selection.prune((id) => data.threads.has(id));
  }, [data.threads]);
  useEffect(() => {
    const typing = (el: HTMLElement | null) => !!el && (el.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(el.tagName) || !!el.closest(".xterm"));
    const onKey = (e: KeyboardEvent) => {
      if (e.defaultPrevented || e.metaKey || e.ctrlKey || e.altKey) return;
      const el = e.target as HTMLElement | null;
      if (typing(el) || el?.closest?.("[role=dialog]:not([data-surface]), [role=menu], [role=listbox]")) return;
      if (e.key === "Escape" && selection.get().ids.size > 0) {
        e.preventDefault();
        e.stopPropagation();
        selection.clear();
        return;
      }
      if (e.key === "x" && !e.shiftKey) {
        const id = threadAt(el);
        if (!id) return;
        e.preventDefault();
        selection.toggle(id);
      }
    };
    // ⌘/Shift-click on a canvas card: selected, not opened (capture, before the card's own click).
    const onClick = (e: MouseEvent) => {
      if (!(e.metaKey || e.ctrlKey || e.shiftKey) || e.altKey || e.button !== 0) return;
      const target = e.target as HTMLElement | null;
      const card = target?.closest<HTMLElement>('[aria-label="Agents canvas"] [data-card][data-key^="t:"]');
      if (!card) return;
      // A card's own controls (its pin and fork buttons, links, menus) keep their clicks.
      const control = target!.closest("button, a, input, textarea, [role=menuitem]");
      if (control && card.contains(control) && !control.hasAttribute("data-drag")) return;
      e.preventDefault();
      e.stopPropagation();
      const id = card.dataset.key!.slice(2);
      if (e.shiftKey) selection.rangeTo(id, canvasOrder());
      else selection.toggle(id);
    };
    window.addEventListener("keydown", onKey, true);
    window.addEventListener("click", onClick, true);
    return () => {
      window.removeEventListener("keydown", onKey, true);
      window.removeEventListener("click", onClick, true);
    };
  }, []);
  // Selected canvas cards: an accent hairline and a faint fill (the coloured left edge stays).
  const css = useMemo(() => {
    if (!sel.ids.size) return "";
    const keys = [...sel.ids].map((id) => `[data-card][data-key="t:${cssEscape(id)}"] > [data-drag]`).join(",");
    return `${keys}{border-color:color-mix(in oklab,var(--accent) 55%,transparent)!important;background-color:color-mix(in oklab,var(--accent) 7%,var(--panel))!important}`;
  }, [sel.ids]);
  return css ? <style data-selection>{css}</style> : null;
}

const cssEscape = (s: string) => (typeof CSS !== "undefined" && CSS.escape ? CSS.escape(s) : s.replace(/["\\]/g, "\\$&"));

/** The thread a focused element stands for: a list row, a board card, a canvas card or a sidebar row. */
function threadAt(el: HTMLElement | null): ThreadId | null {
  const own = el?.closest<HTMLElement>("[data-thread]")?.dataset.thread;
  if (own) return own;
  const key = el?.closest<HTMLElement>('[data-card][data-key^="t:"]')?.dataset.key;
  return key ? key.slice(2) : null;
}

/** Canvas cards in reading order (top to bottom, then left to right): a Shift-click's range. */
function canvasOrder(): string[] {
  const cards = [...document.querySelectorAll<HTMLElement>('[aria-label="Agents canvas"] [data-card][data-key^="t:"]:not([data-leaving])')];
  const at = cards.map((c) => [c, c.getBoundingClientRect()] as const);
  at.sort((a, b) => a[1].top - b[1].top || a[1].left - b[1].left);
  return at.map(([c]) => c.dataset.key!.slice(2));
}

// ── the bar ──

/**
 * The strip under the views bar while threads are selected: how many, the actions that apply to
 * them (each with how many it applies to when that's fewer), and while one runs, its progress.
 */
export function BulkBar({ data, bulk, phone, visible }: { data: AgentsData; bulk: Bulk; phone: boolean; visible: ReadonlySet<ThreadId> }) {
  const sel = useSelection();
  const ids = useMemo(() => [...sel.ids], [sel.ids]);
  const open = ids.length > 0 || !!bulk.running;
  return (
    <Collapse open={open}>
      <BulkStrip data={data} ids={ids} bulk={bulk} phone={phone} hidden={ids.filter((id) => !visible.has(id)).length} />
    </Collapse>
  );
}

function BulkStrip({ data, ids, bulk, phone, hidden }: { data: AgentsData; ids: ThreadId[]; bulk: Bulk; phone: boolean; hidden: number }) {
  const [now] = useState(() => Date.now());
  const can = useMemo(() => applicable(data, ids, now), [data, ids, now]);
  const [writing, setWriting] = useState(false);
  const run = (spec: BulkSpec) => bulk.run(spec, ids);
  const count = (n: number) => (n > 0 && n < ids.length ? n : null);
  const models = useMemo(() => modelsFor(data, ids), [data, ids]);
  // What Snooze leaves out and why ("2 · running"), said before picking a time.
  const snoozeSkips = useMemo(() => skipSummary(planBulk(data, ids, { kind: "snooze", until: now + 3_600_000 }, now).skipped), [data, ids, now]);
  const busy = !!bulk.running;
  if (writing) return <FollowUp n={can.followup} onCancel={() => setWriting(false)} onSend={(text) => (setWriting(false), run({ kind: "followup", text }))} />;
  const actions: { key: string; node: ReactNode; primary?: boolean }[] = [
    { key: "settle", primary: true, node: <Act icon={<Check />} label="Settle" n={count(can.settle)} disabled={busy || !can.settle} onClick={() => run({ kind: "settle" })} /> },
    {
      key: "snooze",
      node: (
        <Drop icon={<AlarmClock />} label="Snooze" n={count(can.snooze)} disabled={busy || !can.snooze}>
          <MenuGroup>
            <MenuGroupLabel>
              Snooze {can.snooze === ids.length ? can.snooze : `${can.snooze} of ${ids.length}`} until
              {snoozeSkips && <span className="mt-0.5 block font-normal text-subtle">Skips {snoozeSkips}</span>}
            </MenuGroupLabel>
            {snoozePresets(Date.now()).map((p) => (
              <MenuItem key={p.label} onClick={() => run({ kind: "snooze", until: p.until })}>
                <span className="min-w-0 flex-1">{p.label}</span>
                <span className="font-mono text-[11px] text-subtle">{p.when}</span>
              </MenuItem>
            ))}
          </MenuGroup>
        </Drop>
      ),
    },
    { key: "archive", primary: true, node: <Act icon={<Archive />} label="Archive" n={count(can.archive)} disabled={busy || !can.archive} onClick={() => run({ kind: "archive" })} /> },
    { key: "approve", primary: true, node: <Act icon={<ThumbsUp />} label="Approve" n={count(can.approve)} disabled={busy || !can.approve} onClick={() => run({ kind: "approve" })} /> },
    { key: "deny", node: <Act icon={<ThumbsDown />} label="Decline" n={count(can.deny)} disabled={busy || !can.deny} onClick={() => run({ kind: "deny" })} /> },
    { key: "stop", node: <Act icon={<CircleStop />} label="Stop" n={count(can.stop)} disabled={busy || !can.stop} onClick={() => run({ kind: "stop" })} /> },
    { key: "retry", node: <Act icon={<RotateCcw />} label="Retry" n={count(can.retry)} disabled={busy || !can.retry} onClick={() => run({ kind: "retry" })} title="Tell each failed agent to try again" /> },
    {
      key: "model",
      node: (
        <Drop icon={<Cpu />} label="Model" disabled={busy || !models.length}>
          {models.map((p) => (
            <MenuGroup key={p.id}>
              <MenuGroupLabel>
                {p.name} · {p.threads} thread{p.threads === 1 ? "" : "s"}
              </MenuGroupLabel>
              {p.models.map((m) => (
                <MenuItem key={m.id} onClick={() => run({ kind: "model", provider: p.id, model: m.id })}>
                  <span className="min-w-0 flex-1 truncate">{m.name}</span>
                </MenuItem>
              ))}
            </MenuGroup>
          ))}
        </Drop>
      ),
    },
    { key: "followup", node: <Act icon={<MessageSquarePlus />} label="Follow-up" disabled={busy || !can.followup} onClick={() => setWriting(true)} title="Send one message to every selected thread" /> },
    { key: "publish", node: <Act icon={<GitPullRequestArrow />} label="Publish" n={count(can.publish)} disabled={busy || !can.publish} onClick={() => run({ kind: "publish" })} title="Publish each thread's work as a change" /> },
  ];
  // Restore only when something selected is on a shelf (picked in the Shelf, or archived rows shown).
  if (can.restore)
    actions.unshift({
      key: "restore",
      primary: true,
      node: <Act icon={<ArchiveRestore />} label="Restore" n={count(can.restore)} disabled={busy} onClick={() => run({ kind: "restore" })} title="Take each off its shelf: un-settle, wake or unarchive" />,
    });
  return (
    <div role="toolbar" aria-label={`${ids.length} selected`} className="relative z-[3] flex h-10 items-center gap-1 border-b border-line bg-[color-mix(in_oklab,var(--accent)_5%,var(--canvas))] px-2 text-[12.5px]" data-nodrag>
      <span className="flex shrink-0 items-center gap-1.5 pr-1.5 pl-1 font-medium text-fg">
        <span className="grid h-[18px] min-w-[18px] place-items-center rounded-[5px] bg-accent px-1 font-mono text-[11px] text-white tabular-nums">{ids.length}</span>
        <span className="max-sm:sr-only">selected</span>
        {hidden > 0 && <span className="font-normal text-subtle max-sm:hidden">({hidden} not shown here)</span>}
      </span>
      <span className="mx-1 h-4 w-px shrink-0 bg-line-2" aria-hidden />
      {bulk.running ? (
        <span role="status" className="flex min-w-0 items-center gap-2 text-muted">
          <span className="pulse-dot size-1.5 shrink-0 rounded-full bg-accent" aria-hidden />
          <span className="truncate">
            {bulk.running.label}… <span className="font-mono tabular-nums">{bulk.running.done}/{bulk.running.total}</span>
          </span>
        </span>
      ) : (
        <div className="flex min-w-0 flex-1 items-center gap-0.5 overflow-x-auto [scrollbar-width:none]">
          {(phone ? actions.filter((a) => a.primary) : actions).map((a) => (
            <span key={a.key} className="contents">
              {a.node}
            </span>
          ))}
          {phone && (
            <Drop icon={<ChevronDown />} label="More" disabled={busy}>
              {actions
                .filter((a) => !a.primary && a.key !== "snooze" && a.key !== "model")
                .map((a) => (
                  <MenuItem key={a.key} onClick={() => (a.key === "followup" ? setWriting(true) : run({ kind: a.key as "deny" | "stop" | "retry" | "publish" }))}>
                    {bulkLabel[a.key as "deny"] ?? a.key}
                  </MenuItem>
                ))}
            </Drop>
          )}
        </div>
      )}
      {bulk.failed && !bulk.running && (
        <span className="flex shrink-0 items-center gap-1.5 text-[12px] text-danger max-sm:hidden" role="status">
          <span className="max-w-64 truncate" title={bulk.failed.why}>
            Failed: {bulk.failed.why}
          </span>
          <button type="button" onClick={bulk.retry} className="rounded-md px-1.5 py-0.5 font-medium text-fg hover:bg-fg/[0.06]">
            Retry
          </button>
        </span>
      )}
      <button
        type="button"
        onClick={() => {
          selection.clear();
          bulk.dismissFailed();
        }}
        title="Clear the selection (Esc)"
        aria-label="Clear the selection"
        className="ml-auto grid size-7 shrink-0 place-items-center rounded-md text-subtle transition-colors hover:bg-fg/[0.06] hover:text-fg"
      >
        <X className="size-3.5" />
      </button>
    </div>
  );
}

function Act({ icon, label, n, disabled, onClick, title }: { icon: ReactNode; label: string; n?: number | null; disabled?: boolean; onClick: () => void; title?: string }) {
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled}
      title={title ?? (n ? `${label} ${n} of the selected` : label)}
      className="flex h-7 shrink-0 items-center gap-1.5 rounded-md px-2 text-muted transition-colors duration-150 hover:bg-fg/[0.06] hover:text-fg disabled:pointer-events-none disabled:opacity-35 [&>svg]:size-3.5 [&>svg]:stroke-[1.75]"
    >
      {icon}
      <span className="max-sm:sr-only">{label}</span>
      {n != null && <span className="font-mono text-[10.5px] text-subtle tabular-nums">{n}</span>}
    </button>
  );
}

function Drop({ icon, label, n, disabled, children }: { icon: ReactNode; label: string; n?: number | null; disabled?: boolean; children: ReactNode }) {
  return (
    <MenuRoot>
      <MenuTrigger
        disabled={disabled}
        className="flex h-7 shrink-0 items-center gap-1.5 rounded-md px-2 text-muted transition-colors duration-150 hover:bg-fg/[0.06] hover:text-fg data-disabled:pointer-events-none data-disabled:opacity-35 data-popup-open:bg-fg/[0.06] data-popup-open:text-fg [&>svg]:size-3.5 [&>svg]:stroke-[1.75]"
      >
        {icon}
        <span className="max-sm:sr-only">{label}</span>
        {n != null && <span className="font-mono text-[10.5px] text-subtle tabular-nums">{n}</span>}
        <ChevronDown className="!size-3 text-subtle" />
      </MenuTrigger>
      <MenuPopup align="start" sideOffset={6} className="max-h-80 min-w-52 overflow-y-auto">
        {children}
      </MenuPopup>
    </MenuRoot>
  );
}

/** One message for every selected thread, typed in the bar (Enter sends, Escape goes back). */
function FollowUp({ n, onSend, onCancel }: { n: number; onSend: (text: string) => void; onCancel: () => void }) {
  const [text, setText] = useState("");
  const field = useRef<HTMLInputElement>(null);
  useEffect(() => field.current?.focus(), []);
  return (
    <form
      className="relative z-[3] flex h-10 items-center gap-2 border-b border-line bg-[color-mix(in_oklab,var(--accent)_5%,var(--canvas))] px-2"
      onSubmit={(e) => {
        e.preventDefault();
        if (text.trim()) onSend(text);
      }}
    >
      <MessageSquarePlus className="size-3.5 shrink-0 text-muted" strokeWidth={1.75} />
      <input
        ref={field}
        value={text}
        onChange={(e) => setText(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Escape") {
            e.preventDefault();
            e.stopPropagation();
            onCancel();
          }
        }}
        placeholder={`Send to ${n} thread${n === 1 ? "" : "s"}…`}
        aria-label="Follow-up for every selected thread"
        className="min-w-0 flex-1 bg-transparent text-[13px] text-fg outline-none placeholder:text-subtle"
      />
      <button type="submit" disabled={!text.trim()} className="flex h-7 items-center gap-1.5 rounded-md bg-fg px-2.5 text-[12px] font-medium text-bg transition hover:opacity-85 disabled:opacity-40">
        <Send className="size-3.5" />
        Send
      </button>
      <button type="button" onClick={onCancel} aria-label="Cancel" className="grid size-7 place-items-center rounded-md text-subtle hover:bg-fg/[0.06] hover:text-fg">
        <X className="size-3.5" />
      </button>
    </form>
  );
}

/** The models the selected threads could switch to: per provider in use, from a machine that has it. */
function modelsFor(data: AgentsData, ids: readonly ThreadId[]): { id: string; name: string; threads: number; models: ProviderInfo["models"] }[] {
  const used = new Map<string, { threads: number; machine: MachineId }>();
  for (const id of ids) {
    const r = data.threads.get(id);
    if (!r) continue;
    const u = used.get(r.value.provider);
    used.set(r.value.provider, { threads: (u?.threads ?? 0) + 1, machine: u?.machine ?? r.machine });
  }
  const out: { id: string; name: string; threads: number; models: ProviderInfo["models"] }[] = [];
  for (const [provider, u] of used) {
    const p = data.machines.get(u.machine)?.value.info?.providers.find((x) => x.id === provider);
    if (p?.models.length) out.push({ id: provider, name: p.name, threads: u.threads, models: p.models.filter((m) => !m.legacy) });
  }
  return out;
}
