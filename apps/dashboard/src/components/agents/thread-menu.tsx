"use client";

/**
 * A thread's menu wherever the thread shows outside its conversation: its canvas card, the list's
 * rows and the board's cards, on a right-click or the ⋯ in their tools. T3 Code's thread context
 * menu (`components/threadActionMenu.logic.ts`, `hooks/useThreadActionMenu.ts`, MIT): open, pin,
 * fork, rename, settle, snooze, archive, publish, copy and delete.
 *
 * One menu for the page (`ThreadMenuHost`), opened at the pointer or under the button that asked:
 * a thousand cards don't each mount a menu, a rename dialog and a snooze dialog.
 */

import { AlarmClock, AlarmClockOff, Archive, ArchiveRestore, Check, Copy, GitFork, MoreHorizontal, Pencil, SquareArrowOutUpRight, Timer, Trash2, Undo2, Upload } from "lucide-react";
import { useEffect, useId, useRef, useState, useSyncExternalStore, type ReactNode } from "react";
import { cantPark, shelfOf, snoozePresets } from "@/lib/agents-inbox";
import { isBusy, reachable, type Thread } from "@/lib/agents-api";
import { publishLabel } from "./change";
import { requestPublish } from "./session";
import { deleteConfirm, showInboxToast, SnoozeDialog, useInboxActions } from "./threads-sidebar";
import type { MenuItem as MenuEntry } from "./menu";
import { Menu, MenuItem, MenuPopup, MenuRadioGroup, MenuRadioItem, MenuSeparator, MenuSub, MenuSubPopup, MenuSubTrigger } from "./ui";
import { Dialog, useKept } from "./ui/dialog";
import { Tip } from "./ui/tooltip";
import { useAgents, usePrefs } from "./use-agents";

type Anchor = Element | { x: number; y: number };
/** What opened the menu: the thread, where, and the place's own items (a canvas card's pins). */
interface Opened {
  thread: string;
  anchor: Anchor;
  extra?: () => MenuEntry[];
}

let opened: Opened | null = null;
/** Hosts mounted that can act (not a share's read-only page): only then is a right-click ours. */
let hosts = 0;
const subs = new Set<() => void>();
/** The last close, so the ⋯ whose press closed its menu doesn't open it again with the click. */
let closed: { anchor: Anchor; at: number } | null = null;
function set(next: Opened | null) {
  if (!next && opened) closed = { anchor: opened.anchor, at: performance.now() };
  opened = next;
  for (const f of subs) f();
}
const subscribe = (f: () => void) => {
  subs.add(f);
  return () => void subs.delete(f);
};

/** Opens `thread`'s menu at the pointer (a right-click) or at the element (the menu key, ⇧F10). */
export function onThreadContextMenu(e: React.MouseEvent, thread: string, extra?: () => MenuEntry[]) {
  if (!hosts || e.defaultPrevented) return;
  // A right-click in a field keeps the browser's menu (copy, paste, spelling).
  if ((e.target as Element).closest("input, textarea, [contenteditable=true]")) return;
  e.preventDefault();
  e.stopPropagation();
  const keyboard = e.button !== 2 && e.clientX === 0 && e.clientY === 0;
  set({ thread, anchor: keyboard ? e.currentTarget : { x: e.clientX, y: e.clientY }, extra });
}

/** Opens `thread`'s menu at `el` for an event the caller took (the list's menu key on its cursor row). */
export function openThreadMenu(thread: string, el: Element, e: React.SyntheticEvent) {
  if (!hosts) return;
  e.preventDefault();
  set({ thread, anchor: el });
}

/** Whether `thread`'s menu is open, from anywhere (a row keeps its hover look meanwhile). */
export function useThreadMenuOpen(thread: string): boolean {
  return useSyncExternalStore(subscribe, () => opened?.thread === thread, () => false);
}

/**
 * The ⋯ in a thread's tools: its menu, under the button. `className` replaces its size and hover
 * look (a sidebar row's is quieter).
 */
export function ThreadMenuButton({ thread, extra, className = "size-6 hover:bg-panel-3 hover:text-fg" }: { thread: string; extra?: () => MenuEntry[]; className?: string }) {
  const ref = useRef<HTMLButtonElement>(null);
  const open = useSyncExternalStore(subscribe, () => !!opened && opened.anchor === ref.current, () => false);
  return (
    <Tip label="Thread actions">
      <button
        ref={ref}
        type="button"
        aria-label="Thread actions"
        aria-haspopup="menu"
        aria-expanded={open}
        onClick={(e) => {
          e.stopPropagation();
          // The press that closed it (outside the menu, so outside press) was this button's: stays closed.
          if (closed && closed.anchor === e.currentTarget && performance.now() - closed.at < 400) return;
          set({ thread, anchor: e.currentTarget, extra });
        }}
        className={`grid place-items-center rounded-md transition-colors duration-150 [&>svg]:size-3.5 [&>svg]:stroke-[1.75] ${open ? "bg-fg/[0.08] text-fg" : ""} ${className}`}
      >
        <MoreHorizontal />
      </button>
    </Tip>
  );
}

/** The page's one thread menu, and the rename and snooze dialogs it opens. */
export function ThreadMenuHost() {
  const { readOnly } = useAgents();
  const now = useSyncExternalStore(subscribe, () => opened, () => null);
  // Kept while the menu plays its way out.
  const last = useRef<Opened | null>(null);
  if (now) last.current = now;
  const [renaming, setRenaming] = useState<Thread | null>(null);
  const [snoozing, setSnoozing] = useState<Thread | null>(null);
  const rename = useKept(renaming);
  const snooze = useKept(snoozing);
  const actions = useInboxActions();
  useEffect(() => {
    if (readOnly) return;
    hosts++;
    return () => {
      hosts--;
      set(null);
    };
  }, [readOnly]);
  if (readOnly) return null;
  const shown = last.current;
  const anchor = shown?.anchor;
  const point = anchor && !(anchor instanceof Element) ? anchor : null;
  return (
    <>
      <Menu open={!!now} onOpenChange={(o) => o || set(null)}>
        <MenuPopup
          aria-label="Thread actions"
          anchor={point ? { getBoundingClientRect: () => DOMRect.fromRect({ x: point.x, y: point.y, width: 0, height: 0 }) } : (anchor as Element | undefined)}
          align={point ? "start" : "end"}
          sideOffset={point ? 2 : 6}
          className="w-60"
        >
          {shown && <Items key={shown.thread} open={shown} onRename={setRenaming} onSnooze={setSnoozing} />}
        </MenuPopup>
      </Menu>
      {rename.value && <RenameDialog key={rename.key} thread={rename.value} open={rename.open} onExited={rename.onExited} onClose={() => setRenaming(null)} />}
      {snooze.value && (
        <SnoozeDialog
          key={snooze.key}
          open={snooze.open}
          onExited={snooze.onExited}
          presets
          onClose={() => setSnoozing(null)}
          onSnooze={(until) => {
            const t = snooze.value;
            if (t) void actions.snooze(t, until);
          }}
        />
      )}
    </>
  );
}

const ICON = "size-3.5";

function Items({ open, onRename, onSnooze }: { open: Opened; onRename: (t: Thread) => void; onSnooze: (t: Thread) => void }) {
  const { client, openThread, phone = false } = useAgents();
  const prefs = usePrefs();
  const actions = useInboxActions();
  const [deleting, setDeleting] = useState(false);
  const rec = client.data.threads.get(open.thread);
  if (!rec) return <p className="px-2 py-1.5 text-[12.5px] text-subtle">This thread is gone.</p>;
  const thread = rec.value;
  const machine = client.data.machines.get(rec.machine)?.value;
  const project = client.data.projects.get(thread.project)?.value;
  const online = reachable(machine);
  const shelf = shelfOf(thread, Date.now());
  const archived = shelf === "archived";
  const forkable = !archived && online && machine?.info?.providers.find((p) => p.id === thread.provider)?.capabilities.fork !== false;
  const settleBlocked = cantPark(thread, "settle", machine);
  const snoozeBlocked = cantPark(thread, "snooze", machine);
  const close = () => set(null);
  const fork = () =>
    client
      .run<{ type: "created"; id: string }>(rec.machine, { type: "thread.fork", thread: thread.id }, "created")
      .then((r) => openThread(r.id))
      .catch((e: unknown) => showInboxToast({ text: `Couldn't fork: ${e instanceof Error ? e.message : String(e)}`, error: true }));
  const copy = (what: string, text: string) =>
    navigator.clipboard?.writeText(text).then(
      () => showInboxToast({ text: `Copied the ${what}` }),
      () => showInboxToast({ text: `Couldn't copy the ${what}`, error: true }),
    );
  const del = () => {
    if (prefs.confirm_delete && !deleting) return setDeleting(true);
    close();
    void actions.remove(thread);
  };
  const item = (label: string, icon: ReactNode, onClick: () => void, opts: { disabled?: boolean; title?: string } = {}) => (
    <MenuItem key={label} disabled={opts.disabled} title={opts.title} onClick={onClick}>
      {icon}
      {label}
    </MenuItem>
  );
  return (
    <>
      {item("Open", <SquareArrowOutUpRight className={ICON} />, () => openThread(thread.id))}
      {open.extra?.().map((x) => item(x.label, x.icon, x.onSelect, { disabled: x.disabled }))}
      {forkable && item("Fork", <GitFork className={ICON} />, () => void fork())}
      {!archived && item("Rename…", <Pencil className={ICON} />, () => onRename(thread), { disabled: !online })}
      <MenuSeparator />
      {!archived &&
        (shelf === "settled"
          ? item("Un-settle", <Undo2 className={ICON} />, () => void actions.unsettle(thread))
          : item("Settle", <Check className={ICON} />, () => void actions.settle(thread), { disabled: !!settleBlocked, title: settleBlocked ?? undefined }))}
      {!archived &&
        (shelf === "snoozed" ? (
          item("Wake now", <AlarmClockOff className={ICON} />, () => void actions.wake(thread))
        ) : snoozeBlocked || phone ? (
          // A phone has no room beside the menu: the presets in a dialog.
          item("Snooze…", <AlarmClock className={ICON} />, () => onSnooze(thread), { disabled: !!snoozeBlocked, title: snoozeBlocked ?? undefined })
        ) : (
          <MenuSub>
            <MenuSubTrigger>
              <AlarmClock className={ICON} />
              Snooze
            </MenuSubTrigger>
            <MenuSubPopup className="w-60">
              {snoozePresets(Date.now()).map((p) => (
                <MenuItem key={p.label} onClick={() => void actions.snooze(thread, p.until)}>
                  {p.label}
                  <span className="ms-auto text-[11px] text-subtle">{p.when}</span>
                </MenuItem>
              ))}
              <MenuSeparator />
              <MenuItem onClick={() => onSnooze(thread)}>Custom…</MenuItem>
            </MenuSubPopup>
          </MenuSub>
        ))}
      {archived
        ? item("Unarchive", <ArchiveRestore className={ICON} />, () => void actions.restore(thread))
        : item("Archive", <Archive className={ICON} />, () => void actions.archive(thread), { disabled: thread.status === "running" || thread.status === "starting" })}
      {!archived &&
        !!project &&
        (!!project.repo || !!project.git) &&
        item(
          // "…": it opens the publish form (not when there's nowhere to push: that label says why).
          `${publishLabel(thread, project)}${!project.repo && !project.git?.remote ? "" : "…"}`,
          <Upload className={ICON} />,
          () => {
            openThread(thread.id);
            requestPublish(thread.id);
          },
          { disabled: !online || isBusy(thread.status) || (!project.repo && !project.git?.remote) },
        )}
      <MenuSeparator />
      {!archived && (
        <MenuSub>
          <MenuSubTrigger>
            <Timer className={ICON} />
            Auto-settle behavior
          </MenuSubTrigger>
          <MenuSubPopup>
            <MenuRadioGroup value={thread.inbox.manual ? "disabled" : "enabled"} onValueChange={(v) => void actions.autoSettle(thread, v === "enabled")}>
              <MenuRadioItem value="enabled">Enabled</MenuRadioItem>
              <MenuRadioItem value="disabled">Disabled</MenuRadioItem>
            </MenuRadioGroup>
          </MenuSubPopup>
        </MenuSub>
      )}
      <MenuSub>
        <MenuSubTrigger>
          <Copy className={ICON} />
          Copy
        </MenuSubTrigger>
        <MenuSubPopup className="w-44">
          {item("Title", null, () => void copy("title", thread.title))}
          {thread.worktree && item("Branch", null, () => void copy("branch", thread.worktree!.branch))}
          {(thread.worktree || project) && item("Path", null, () => void copy("path", thread.worktree?.path ?? project!.path))}
          {item("Thread ID", null, () => void copy("thread ID", thread.id))}
        </MenuSubPopup>
      </MenuSub>
      <MenuItem closeOnClick={false} variant="destructive" disabled={!online} onClick={del}>
        <Trash2 className={ICON} />
        {deleting ? "Delete for good?" : "Delete…"}
      </MenuItem>
      {deleting && <p className="px-2 pb-1.5 text-[11.5px] leading-snug text-subtle">{deleteConfirm(thread)}</p>}
    </>
  );
}

function RenameDialog({ thread, open, onExited, onClose }: { thread: Thread; open: boolean; onExited: () => void; onClose: () => void }) {
  const { client } = useAgents();
  const [title, setTitle] = useState(thread.title);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const form = useId();
  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    const next = title.trim();
    const machine = client.data.threads.get(thread.id)?.machine;
    if (!next || next === thread.title || !machine) return onClose();
    setBusy(true);
    setError(null);
    try {
      await client.run(machine, { type: "thread.configure", thread: thread.id, title: next });
      onClose();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog
      open={open}
      onExited={onExited}
      onClose={onClose}
      busy={busy}
      title="Rename thread"
      className="w-[min(420px,100%)]"
      footer={
        <>
          <button type="button" onClick={onClose} className="h-8 rounded-md px-3 text-[12.5px] text-muted transition hover:bg-panel-2 hover:text-fg">
            Cancel
          </button>
          <button type="submit" form={form} disabled={busy || !title.trim()} className="h-8 rounded-md bg-fg px-3 text-[12.5px] font-medium text-bg transition hover:opacity-85 disabled:opacity-40">
            Rename
          </button>
        </>
      }
    >
      <form id={form} onSubmit={submit}>
        <input
          autoFocus
          aria-label="Title"
          value={title}
          onChange={(e) => setTitle(e.target.value)}
          onFocus={(e) => e.currentTarget.select()}
          className="h-9 w-full rounded-md border border-line bg-bg px-2.5 text-[13.5px] text-fg outline-none focus:border-accent/60"
        />
        {error && <p className="mt-2 text-[12px] text-danger">{error}</p>}
      </form>
    </Dialog>
  );
}
