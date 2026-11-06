/**
 * Bulk actions over selected threads (docs/code/agents.md, "Working at scale"): what each action
 * sends to each thread, which threads it can't apply to (and why), running it, and the one-line
 * summary ("12 settled · 1 failed (It's still running)"). Every step is one existing daemon command
 * with its own command id (the daemon's idempotency key): retrying a step that never got an answer
 * reuses its id, so a daemon never applies it twice; a step the daemon refused gets a fresh id when
 * the person retries it. Threads span machines, so there is no batch command: steps go out a few at
 * a time per page. Pure apart from `runBulk`'s sends.
 */

import { commandId, isBusy, type Command, type Decision, type MachineId, type Reply, type Request, type Run, type Thread, type ThreadId } from "./agents-api";
import { lastRuns } from "./agents-collection";
import { cantPark } from "./agents-inbox";
import type { AgentsData } from "./agents-store";

export type BulkKind = "settle" | "archive" | "snooze" | "approve" | "deny" | "stop" | "retry" | "model" | "followup" | "publish";

/** What a bulk action needs beyond the threads. */
export type BulkSpec =
  | { kind: "settle" | "archive" | "approve" | "deny" | "stop" | "retry" | "publish" }
  | { kind: "snooze"; until: number }
  | { kind: "model"; provider: string; model: string; effort?: string }
  | { kind: "followup"; text: string };

/** One command to one thread (approving sends one per pending approval). */
export interface BulkStep {
  thread: ThreadId;
  machine: MachineId;
  command: Command;
  /** Puts this step back (Undo), where the single action has one. */
  undo?: Command;
}

export interface Skip {
  thread: ThreadId;
  why: string;
}

export interface BulkPlan {
  spec: BulkSpec;
  steps: BulkStep[];
  skipped: Skip[];
}

/** The verb a summary uses, past tense: "12 settled". */
export const bulkDone: Record<BulkKind, string> = {
  settle: "settled",
  archive: "archived",
  snooze: "snoozed",
  approve: "approved",
  deny: "declined",
  stop: "stopped",
  retry: "retried",
  model: "switched",
  followup: "sent",
  publish: "published",
};

/** The action's name in the bar and ⌘K. */
export const bulkLabel: Record<BulkKind, string> = {
  settle: "Settle",
  archive: "Archive",
  snooze: "Snooze",
  approve: "Approve",
  deny: "Decline",
  stop: "Stop",
  retry: "Retry",
  model: "Set model",
  followup: "Send a follow-up",
  publish: "Publish",
};

/** What the retry of a failed thread says to its agent. */
export const RETRY_TEXT = "Your last run failed. Try again from where you stopped.";

/** Every thread's pending requests, oldest first, in one pass. */
function pendingByThread(data: AgentsData): Map<ThreadId, Request[]> {
  const out = new Map<ThreadId, Request[]>();
  for (const r of data.requests.values()) {
    if (r.value.status !== "pending") continue;
    const list = out.get(r.value.thread);
    if (list) list.push(r.value);
    else out.set(r.value.thread, [r.value]);
  }
  for (const list of out.values()) list.sort((a, b) => a.created_at - b.created_at);
  return out;
}

/** Which decision an approval takes for approve / decline, if it offers one. */
function decisionFor(r: Request, kind: "approve" | "deny"): Decision | null {
  if (r.body.type !== "approval") return null;
  const want: Decision[] = kind === "approve" ? ["accept"] : ["decline", "cancel"];
  return want.find((d) => (r.body.type === "approval" ? r.body.decisions.includes(d) : false)) ?? null;
}

/**
 * The steps an action sends to these threads and the threads it leaves alone, each with the reason
 * the single action would give (it's disabled there for the same reason). A thread that's gone is
 * skipped too.
 */
export function planBulk(data: AgentsData, ids: Iterable<ThreadId>, spec: BulkSpec, now = Date.now()): BulkPlan {
  const steps: BulkStep[] = [];
  const skipped: Skip[] = [];
  const skip = (thread: ThreadId, why: string) => skipped.push({ thread, why });
  const pending = spec.kind === "approve" || spec.kind === "deny" ? pendingByThread(data) : null;
  const last = spec.kind === "publish" ? lastRuns(data) : null;
  for (const id of ids) {
    const rec = data.threads.get(id);
    if (!rec) {
      skip(id, "It's gone");
      continue;
    }
    const t = rec.value;
    const machine = rec.machine;
    const step = (command: Command, undo?: Command) => steps.push({ thread: id, machine, command, ...(undo ? { undo } : {}) });
    switch (spec.kind) {
      case "settle": {
        if (t.inbox.settle?.state === "settled") skip(id, "It's settled already");
        else if (cantPark(t, "settle")) skip(id, cantPark(t, "settle")!);
        else step({ type: "thread.settle", thread: id }, { type: "thread.unsettle", thread: id, restore: { settle: t.inbox.settle, snooze: t.inbox.snooze } });
        break;
      }
      case "snooze": {
        const why = cantPark(t, "snooze");
        if (why) skip(id, why);
        else if (spec.until <= now) skip(id, "That time has passed");
        else step({ type: "thread.snooze", thread: id, until: spec.until }, t.inbox.snooze ? { type: "thread.snooze", thread: id, until: t.inbox.snooze.until } : { type: "thread.unsnooze", thread: id });
        break;
      }
      case "archive": {
        if (t.archived_at !== null) skip(id, "It's archived already");
        else if (t.status === "running" || t.status === "starting") skip(id, "It's still running");
        else step({ type: "thread.archive", thread: id }, { type: "thread.unarchive", thread: id });
        break;
      }
      case "approve":
      case "deny": {
        const waiting = pending!.get(id) ?? [];
        if (!waiting.length) {
          skip(id, "Nothing is waiting for an answer");
          break;
        }
        let any = false;
        for (const r of waiting) {
          const decision = decisionFor(r, spec.kind);
          if (!decision) continue;
          any = true;
          step({ type: "request.respond", request: r.id, response: { type: "decision", decision } });
        }
        if (!any) skip(id, waiting.some((r) => r.body.type === "questions") ? "It asks a question: answer it in the conversation" : "Its request can't be answered that way");
        break;
      }
      case "stop": {
        if (!isBusy(t.status) || t.status === "waiting") skip(id, t.status === "waiting" ? "It's waiting for you, not running" : "It isn't running");
        else step({ type: "run.interrupt", thread: id });
        break;
      }
      case "retry": {
        if (t.status !== "error" && !t.error) skip(id, "It didn't fail");
        else if (isBusy(t.status)) skip(id, "It's running");
        else step({ type: "message.send", thread: id, text: RETRY_TEXT });
        break;
      }
      case "model": {
        if (t.provider !== spec.provider) skip(id, `It runs on another agent`);
        else if (t.model === spec.model && (spec.effort === undefined || t.effort === spec.effort)) skip(id, "It uses that model already");
        else step({ type: "thread.configure", thread: id, model: spec.model, ...(spec.effort ? { effort: spec.effort } : {}) }, t.model ? { type: "thread.configure", thread: id, model: t.model, ...(t.effort ? { effort: t.effort } : {}) } : undefined);
        break;
      }
      case "followup": {
        if (!spec.text.trim()) skip(id, "Nothing to send");
        else if (t.archived_at !== null) skip(id, "It's archived");
        else step({ type: "message.send", thread: id, text: spec.text.trim() });
        break;
      }
      case "publish": {
        const why = cantPublish(t, last!.get(id));
        if (why) skip(id, why);
        else {
          const repo = !!data.projects.get(t.project)?.value.repo;
          step({ type: "thread.publish", thread: id, message: t.title, land: t.change === null && repo });
        }
        break;
      }
    }
  }
  return { spec, steps, skipped };
}

/** Why a thread has nothing to publish (`null`: it has). */
export function cantPublish(t: Thread, last: Run | undefined): string | null {
  if (!t.worktree) return "It works outside a worktree";
  if (isBusy(t.status)) return "It's still running";
  if (t.change?.status === "landed") return "Its change landed";
  if (t.change && t.change.status !== "conflict" && t.change.status !== "failing") return "It's published already";
  if (!t.change && last?.stat && last.stat.additions + last.stat.deletions === 0) return "It has no changes";
  return null;
}

// ── running ──

export interface StepOutcome {
  step: BulkStep;
  /** The command id it went out with (kept for a retry that never got an answer). */
  id: string;
  ok: boolean;
  /** Why it failed: the daemon's refusal, or that no answer came. */
  why?: string;
  /** No answer came (a timeout, the network): retrying reuses `id`. */
  unanswered?: boolean;
}

export interface BulkResult {
  spec: BulkSpec;
  outcomes: StepOutcome[];
  skipped: Skip[];
}

/**
 * Steps at a time. Measured on the live relay: one command answers in ~10 ms, 60 at once in
 * ~140 ms, so 200 threads take well under a second here; a browser on HTTP/1.1 opens 6
 * connections per host and queues the rest, HTTP/2 takes them all.
 */
export const BULK_CONCURRENCY = 12;

/**
 * Send every step, a few at a time; never throws. `ids` gives a step its command id: a retry of
 * unanswered steps passes the ids they went out with. `onProgress` says how many have answered.
 */
export async function runBulk(
  plan: Pick<BulkPlan, "spec" | "steps" | "skipped">,
  send: (machine: MachineId, command: Command, id: string) => Promise<Reply>,
  opts: { ids?: Map<BulkStep, string>; concurrency?: number; onProgress?: (done: number, total: number) => void } = {},
): Promise<BulkResult> {
  const outcomes: StepOutcome[] = new Array(plan.steps.length);
  let next = 0;
  let done = 0;
  const worker = async () => {
    while (next < plan.steps.length) {
      const i = next++;
      const step = plan.steps[i];
      const id = opts.ids?.get(step) ?? commandId();
      try {
        const reply = await send(step.machine, step.command, id);
        outcomes[i] = reply.type === "error" ? { step, id, ok: false, why: reply.message } : { step, id, ok: true };
      } catch (e) {
        outcomes[i] = { step, id, ok: false, why: e instanceof Error ? e.message : String(e), unanswered: true };
      }
      opts.onProgress?.(++done, plan.steps.length);
    }
  };
  await Promise.all(Array.from({ length: Math.min(opts.concurrency ?? BULK_CONCURRENCY, plan.steps.length) }, worker));
  return { spec: plan.spec, outcomes, skipped: plan.skipped };
}

/** Per thread: did every step to it go through (approving sends several to one thread). */
export function byThread(result: BulkResult): { done: ThreadId[]; failed: { thread: ThreadId; why: string }[] } {
  const failed = new Map<ThreadId, string>();
  const seen = new Set<ThreadId>();
  for (const o of result.outcomes) {
    seen.add(o.step.thread);
    if (!o.ok && !failed.has(o.step.thread)) failed.set(o.step.thread, o.why ?? "It failed");
  }
  return { done: [...seen].filter((t) => !failed.has(t)), failed: [...failed].map(([thread, why]) => ({ thread, why })) };
}

/**
 * The notice after a bulk action: "12 settled · 1 failed (It's still running) · 3 skipped". One
 * reason, the most common, stands for the failures; the skips are counted (their reasons are in
 * the bar's detail).
 */
export function summarize(result: BulkResult): string {
  const { done, failed } = byThread(result);
  const parts: string[] = [];
  const kind = result.spec.kind;
  if (done.length || (!failed.length && !result.skipped.length)) parts.push(`${done.length} ${bulkDone[kind]}`);
  if (failed.length) parts.push(`${failed.length} failed (${commonest(failed.map((f) => f.why))})`);
  if (result.skipped.length) parts.push(`${result.skipped.length} skipped`);
  return parts.join(" · ");
}

/** "3 skipped: 2 still running, 1 archived already", for the bar's detail line. */
export function skipSummary(skipped: readonly Skip[]): string | null {
  if (!skipped.length) return null;
  const counts = new Map<string, number>();
  for (const s of skipped) counts.set(s.why, (counts.get(s.why) ?? 0) + 1);
  return [...counts]
    .sort((a, b) => b[1] - a[1])
    .map(([why, n]) => `${n} · ${why}`)
    .join("; ");
}

function commonest(list: readonly string[]): string {
  const counts = new Map<string, number>();
  for (const s of list) counts.set(s, (counts.get(s) ?? 0) + 1);
  return [...counts].sort((a, b) => b[1] - a[1])[0][0];
}

/** The steps that put a result back (Undo): one per step that went through and has an undo. */
export function undoPlan(result: BulkResult): BulkPlan | null {
  const steps = result.outcomes.filter((o) => o.ok && o.step.undo).map((o) => ({ thread: o.step.thread, machine: o.step.machine, command: o.step.undo! }));
  return steps.length ? { spec: result.spec, steps, skipped: [] } : null;
}

/**
 * The steps to try again: those that failed. One that never got an answer goes out with the id it
 * had (the daemon may have run it: it answers with that receipt instead of running it twice); one
 * the daemon refused gets a fresh id, so it's tried for real.
 */
export function retryPlan(result: BulkResult): { plan: BulkPlan; ids: Map<BulkStep, string> } | null {
  const failed = result.outcomes.filter((o) => !o.ok);
  if (!failed.length) return null;
  const ids = new Map<BulkStep, string>();
  for (const o of failed) if (o.unanswered) ids.set(o.step, o.id);
  return { plan: { spec: result.spec, steps: failed.map((o) => o.step), skipped: [] }, ids };
}

/** What the actions can do with this selection: the bar offers only those (`n` threads each). */
export function applicable(data: AgentsData, ids: readonly ThreadId[], now = Date.now()): Record<Exclude<BulkKind, "model" | "followup" | "snooze">, number> & { snooze: number; followup: number } {
  const count = (spec: BulkSpec) => new Set(planBulk(data, ids, spec, now).steps.map((s) => s.thread)).size;
  return {
    settle: count({ kind: "settle" }),
    archive: count({ kind: "archive" }),
    snooze: count({ kind: "snooze", until: now + 3_600_000 }),
    approve: count({ kind: "approve" }),
    deny: count({ kind: "deny" }),
    stop: count({ kind: "stop" }),
    retry: count({ kind: "retry" }),
    publish: count({ kind: "publish" }),
    followup: count({ kind: "followup", text: "x" }),
  };
}
