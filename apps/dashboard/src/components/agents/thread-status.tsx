/**
 * A thread's status as T3 Code shows it on its rows (T3's `Sidebar.logic.ts`
 * `resolveThreadStatusPill` and `ThreadStatusIndicators.tsx`, MIT). In roll's colours: what needs
 * you and what runs are accent, done is green, failed is red. A project's monogram and colour are
 * in `identity.tsx`.
 */

import { ChevronDown, Workflow } from "lucide-react";
import { isBusy, type Request, type Run, type Thread } from "@/lib/agents-api";
import { Menu, MenuGroup, MenuGroupLabel, MenuItem, MenuPopup, MenuTrigger } from "./ui/menu";

/**
 * What an orchestrator has spun up: the agents it started (delegated threads, oldest first), how
 * many still run and how many wait for you (not running: the header's count says the same).
 */
export function orchestration(thread: Thread, threads: Iterable<Thread>): { started: number; running: number; waiting: number; agents: Thread[] } {
  let running = 0;
  let waiting = 0;
  const agents: Thread[] = [];
  for (const t of threads) {
    if (t.lineage?.parent !== thread.id || t.lineage.relation !== "subagent") continue;
    agents.push(t);
    if (t.status === "waiting") waiting++;
    else if (isBusy(t.status)) running++;
  }
  agents.sort((a, b) => a.created_at - b.created_at || a.id.localeCompare(b.id));
  return { started: agents.length, running, waiting, agents };
}

/**
 * An orchestrator's mark (the palette's `Workflow` glyph), with the agents it started, how many
 * still run and how many wait. With `onOpen` it's a menu of those agents, each opening its
 * conversation: a finished subagent leaves the canvas, and this is the way back to it (T3's
 * subagent rows open their threads the same way).
 */
export function OrchestratorChip({
  started,
  running,
  waiting = 0,
  agents = [],
  onOpen,
  className = "",
}: {
  started: number;
  running: number;
  waiting?: number;
  agents?: readonly Thread[];
  onOpen?: (id: string) => void;
  className?: string;
}) {
  const label = `Orchestrator${started ? ` · ${started} agent${started === 1 ? "" : "s"}${running ? `, ${running} running` : ""}${waiting ? `, ${waiting} waiting for you` : ""}` : ""}`;
  const body = (
    <>
      <Workflow className={`size-3 shrink-0 ${running ? "text-accent" : ""}`} aria-hidden />
      <span aria-hidden>Orchestrator</span>
      {started > 0 && (
        <span aria-hidden className="font-mono tabular-nums text-subtle">
          {started}
          {running > 0 && <span className="text-accent"> · {running} running</span>}
          {waiting > 0 && <span className="text-accent"> · {waiting} waiting</span>}
        </span>
      )}
    </>
  );
  const chip = `inline-flex min-w-0 shrink-0 items-center gap-1 rounded-full border border-line-2 px-1.5 py-px text-[10.5px] leading-[16px] text-muted ${className}`;
  if (!onOpen || agents.length === 0)
    return (
      <span aria-label={label} title={label} className={chip}>
        {body}
      </span>
    );
  return (
    // The card under it opens its own thread on a click: picks here stay here (a portal still bubbles in React).
    <span className="inline-flex min-w-0 shrink-0" data-nodrag onClick={(e) => e.stopPropagation()} onKeyDown={(e) => e.stopPropagation()}>
      <Menu>
        <MenuTrigger
          aria-label={`${label}: open one`}
          title={label}
          className={`${chip} cursor-pointer transition-colors duration-150 hover:border-line-3 hover:text-fg data-popup-open:border-line-3 data-popup-open:text-fg`}
        >
          {body}
          <ChevronDown className="size-3 shrink-0" aria-hidden />
        </MenuTrigger>
        <MenuPopup aria-label="Agents it started" align="start" sideOffset={6} className="w-72">
          <MenuGroup>
            <MenuGroupLabel>Agents it started</MenuGroupLabel>
            {agents.map((t) => {
              const s = subagentState(t);
              return (
                <MenuItem key={t.id} onClick={() => onOpen(t.id)} aria-label={`${t.title}, ${s.label}`}>
                  <span aria-hidden className={`size-1.5 shrink-0 rounded-full ${toneDot[s.tone]} ${s.pulse ? "pulse-dot" : ""}`} />
                  <span className="min-w-0 flex-1 truncate">{t.title}</span>
                  <span className={`shrink-0 text-[11px] ${toneText[s.tone]}`}>{s.label}</span>
                </MenuItem>
              );
            })}
          </MenuGroup>
        </MenuPopup>
      </Menu>
    </span>
  );
}

/** One word for a delegated agent in its orchestrator's menu (its runs aren't loaded there). */
function subagentState(t: Thread): ThreadPill {
  if (t.status === "waiting" || t.pending_requests > 0) return { label: "Needs you", tone: "accent", pulse: false };
  if (isBusy(t.status)) return { label: "Working", tone: "accent", pulse: true };
  if (t.status === "error" || t.error) return { label: "Failed", tone: "danger", pulse: false };
  return { label: "Done", tone: "muted", pulse: false };
}

export type PillTone = "accent" | "ok" | "danger" | "muted";

export interface ThreadPill {
  label: string;
  tone: PillTone;
  /** Only running work moves. */
  pulse: boolean;
}

/** A run that has ended (it can't change any more). */
export function runEnded(run: Run | undefined): boolean {
  return !!run && ["completed", "interrupted", "failed", "cancelled", "rolled_back"].includes(run.status);
}

/**
 * The one status worth saying about a thread, T3's order: what waits on you first, then what's
 * working, then what failed, a plan ready to carry out, and a finished run you haven't looked at.
 * `null` when there's nothing to say (idle and seen). `planReady`: the newest run ended with a plan
 * in plan mode (`readyPlan` where the thread's nodes are loaded; `thread.plan_ready` elsewhere).
 */
export function threadPill(thread: Thread, pending: readonly Request[], lastRun: Run | undefined, planReady = false): ThreadPill | null {
  const waiting = pending.filter((r) => r.status === "pending" && r.thread === thread.id);
  const approval = waiting.find((r) => r.body.type === "approval");
  if (approval && approval.body.type === "approval" && approval.body.action === "plan") return { label: "Plan ready", tone: "accent", pulse: false };
  if (approval) return { label: "Pending approval", tone: "accent", pulse: false };
  if (waiting.length > 0) return { label: "Awaiting input", tone: "accent", pulse: false };
  // Waiting on you is a request here (`waitingOnYou`), never the record's `pending_requests` alone:
  // the header, the tab and the zoom bar count requests, and a card must agree with them.
  if (thread.status === "running" || thread.status === "waiting") return { label: "Working", tone: "accent", pulse: true };
  if (thread.status === "starting") return { label: "Connecting", tone: "accent", pulse: true };
  if (thread.status === "error" || lastRun?.status === "failed") return { label: "Failed", tone: "danger", pulse: false };
  if (planReady) return { label: "Plan ready", tone: "accent", pulse: false };
  if (lastRun?.status === "completed" && unseen(thread, lastRun)) return { label: "Completed", tone: "ok", pulse: false };
  return null;
}

/** Finished since you last opened it. */
function unseen(thread: Thread, run: Run): boolean {
  const seen = thread.inbox?.visited_at ?? null;
  return seen === null || (run.completed_at ?? 0) > seen;
}

/**
 * Done and not looked at yet: idle, its last run completed since you last opened it (T3's
 * completed tone; the card's border is green until you open it, as T3's row pill is).
 */
export function threadDone(thread: Thread, lastRun: Run | undefined): boolean {
  return thread.status === "idle" && !thread.error && lastRun?.status === "completed" && thread.archived_at === null && unseen(thread, lastRun);
}

// Done is said once, quietly: its dot is the green, its word the muted text (a pale green word failed
// contrast on white, and beside a green frame it said done twice).
const toneText: Record<PillTone, string> = { accent: "text-accent", ok: "text-muted", danger: "text-danger", muted: "text-subtle" };
const toneDot: Record<PillTone, string> = { accent: "bg-accent", ok: "bg-ok", danger: "bg-danger", muted: "bg-subtle" };

/** T3's `ThreadStatusLabel`: a dot and the label, in the pill's tone. */
export function StatusPill({ pill, className = "", line = false }: { pill: ThreadPill; className?: string; line?: boolean }) {
  return (
    <span role="img" aria-label={pill.label} title={pill.label} data-line={line || undefined} className={`inline-flex shrink-0 items-center gap-1 text-[10.5px] font-medium ${toneText[pill.tone]} ${className}`}>
      <span aria-hidden className={`size-1.5 rounded-full ${toneDot[pill.tone]} ${pill.pulse ? "pulse-dot" : ""}`} />
      <span aria-hidden>{pill.label}</span>
    </span>
  );
}
