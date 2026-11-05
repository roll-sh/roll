"use client";

/**
 * The transcript, as T3 Code's message timeline (`components/chat/MessagesTimeline.tsx`,
 * `TimelineWorkGroupRows.tsx`, `WorkLog.tsx`, `ProposedPlanCard.tsx`, `ChangedFilesCard.tsx`; MIT):
 * the user's turn in a bubble on the right, the agent's words bare on the left, and everything it
 * did between them in a calm work log: steps grouped into one sentence ("Ran 3 commands and read
 * 2 files"), a live group showing its latest step, a finished run folded into "Worked for 2m 13s"
 * above its answer. Plans are cards, task lists tick, subagents nest, requests are answered in
 * place (`request-card.tsx`). The grouping is `lib/agents-worklog.ts`.
 */

import {
  ArrowRight,
  Check,
  ChevronRight,
  CircleAlert,
  CircleCheck,
  CircleDashed,
  Copy,
  FileDiff as DiffIcon,
  ListChecks,
  ListTodo,
  Minimize2,
  RotateCcw,
  Split,
  X,
} from "lucide-react";
import { useState, type ReactNode } from "react";
import { useEscapeLayer } from "./escape-layers";
import type { Checkpoint, FileChange, Node, Request, RequestResponse, Run } from "@/lib/agents-api";
import { formatContextTokens } from "@/lib/agents-context";
import type { NodeTree } from "@/lib/agents-store";
import { ended, foldLabel, foldTurn, timelineItems, type TimelineItem } from "@/lib/agents-worklog";
import { Markdown } from "../code/markdown";
import { Collapse } from "./animated-height";
import { PlanCard } from "./plan";
import { RequestCard, toFileDiff } from "./request-card";
import { SubagentGroup, SubagentRow } from "./subagents";
import { StepRow, WorkGroup } from "./work-log";
import { elapsed, runStatusLabel, Stat, usageLine } from "./shared";

export interface TranscriptActions {
  /** Answer a request (unset when read-only: requests show what they asked). */
  respond?: (request: string, r: RequestResponse) => Promise<unknown>;
  /** Show this run's diff (or the whole thread's with `null`); unset when there's none to fetch. */
  showDiff?: (run: string | null) => void;
  rollback?: (checkpoint: Checkpoint) => Promise<unknown>;
  fork?: (run: Run) => Promise<unknown>;
  openThread: (thread: string) => void;
  /** Open a provider subagent's own transcript (unset: its work opens inline under its row). */
  openSubagent?: (node: string) => void;
  /** The composer below carries a ready plan's Implement and Refine (the transcript just says so). */
  hostsPlan?: boolean;
  /** Write a plan into the thread's workspace (T3's "Save to workspace"); answers the path written. */
  savePlan?: (path: string, text: string) => Promise<string>;
}

interface Ctx {
  requests: ReadonlyMap<string, Request>;
  actions: TranscriptActions;
  now: number;
}

/** One run: what the user sent, then the agent's work, then how it ended. */
export function RunView({
  run,
  tree,
  requests,
  checkpoint,
  now,
  actions,
  busyThread,
  last = false,
}: {
  run: Run;
  tree: NodeTree[];
  requests: ReadonlyMap<string, Request>;
  checkpoint: Checkpoint | undefined;
  now: number;
  actions: TranscriptActions;
  busyThread: boolean;
  /** The newest run: its actions show without hovering, as T3 does for a turn's final message. */
  last?: boolean;
}) {
  const live = !ended(run);
  const items = timelineItems(tree);
  const turn = foldTurn(items, run);
  const [folded, setFolded] = useState(true);
  const ctx: Ctx = { requests, actions, now };
  const files = changedFiles(tree);
  return (
    <section className={`group/run flex flex-col gap-3 ${run.status === "rolled_back" ? "opacity-45" : ""}`} aria-label={`Run ${run.ordinal}`}>
      <UserMessage run={run} />
      {turn.fold && (
        <div className="border-b border-line pt-1 pb-2">
          <button
            onClick={() => setFolded((f) => !f)}
            aria-expanded={!folded}
            className="flex items-center gap-1 rounded-md px-1 text-[13px] text-muted transition-colors hover:text-fg"
          >
            {foldLabel(run)}
            <ChevronRight className={`size-3.5 transition-transform duration-200 motion-reduce:transition-none ${folded ? "" : "rotate-90"}`} />
          </button>
          <Collapse open={!folded}>
            <div className="flex flex-col gap-3 pt-3">
              <Items items={turn.fold} ctx={ctx} live={false} />
            </div>
          </Collapse>
        </div>
      )}
      <Items items={turn.rest} ctx={ctx} live={live} />
      {live ? (
        <p className="flex h-6 items-center gap-2 px-1 text-[13px] text-muted tabular-nums transition-opacity duration-150 starting:opacity-0">
          <span className={`size-1.5 rounded-full bg-accent ${run.status === "waiting" ? "" : "pulse-dot"}`} aria-hidden />
          <span className={run.status === "waiting" ? "" : "shimmer"}>
            {run.status === "waiting" ? "Waiting for you" : "Working"}
            {run.started_at ? ` for ${elapsed(now - run.started_at)}` : "…"}
          </span>
        </p>
      ) : (
        <>
          {files.length > 0 && run.status !== "rolled_back" && <ChangedFiles files={files} onOpen={actions.showDiff && (() => actions.showDiff!(run.id))} />}
          <RunMeta run={run} tree={tree} checkpoint={checkpoint} actions={actions} busyThread={busyThread} last={last} />
        </>
      )}
      {run.error && <p className="rounded-xl border border-danger/40 bg-danger/10 px-3 py-2 text-[12.5px] text-danger">{run.error}</p>}
    </section>
  );
}

/** Under a finished run (T3's assistant meta row): how it ended, what it cost, copy, fork, roll back. */
function RunMeta({ run, tree, checkpoint, actions, busyThread, last }: { run: Run; tree: NodeTree[]; checkpoint: Checkpoint | undefined; actions: TranscriptActions; busyThread: boolean; last: boolean }) {
  const [confirmRollback, setConfirmRollback] = useState(false);
  useEscapeLayer(confirmRollback, () => setConfirmRollback(false));
  const answer = [...tree].reverse().find((t) => t.node.body.type === "message")?.node.body;
  const usage = usageLine(run.usage);
  return (
    <div
      className={`-mt-1 flex flex-wrap items-center gap-x-1 gap-y-1 text-[12px] text-subtle transition-opacity duration-200 motion-reduce:transition-none ${
        last || confirmRollback ? "" : "opacity-0 group-hover/run:opacity-100 focus-within:opacity-100 [@media(hover:none)]:opacity-100"
      }`}
    >
      {answer?.type === "message" && answer.text && <CopyButton text={answer.text} />}
      {actions.fork && run.status !== "rolled_back" && (
        <MetaButton onClick={() => void actions.fork!(run)} title="Start a new thread from this point">
          <Split className="size-3" /> Fork
        </MetaButton>
      )}
      {actions.rollback &&
        checkpoint &&
        run.status !== "rolled_back" &&
        !busyThread &&
        (confirmRollback ? (
          <span className="flex items-center gap-1.5 px-1">
            <span className="text-muted">Restore files and drop later runs?</span>
            <button
              onClick={() => {
                setConfirmRollback(false);
                void actions.rollback!(checkpoint);
              }}
              className="rounded-md px-1.5 py-0.5 text-danger hover:bg-danger/10"
            >
              Roll back
            </button>
            <button onClick={() => setConfirmRollback(false)} className="rounded-md p-0.5 hover:text-fg" aria-label="Keep">
              <X className="size-3" />
            </button>
          </span>
        ) : (
          <MetaButton onClick={() => setConfirmRollback(true)} title="Restore the files to how they were after this run">
            <RotateCcw className="size-3" /> Roll back to here
          </MetaButton>
        ))}
      <span className="ml-auto flex items-center gap-2 px-1 tabular-nums">
        <span className={run.status === "failed" ? "text-danger" : ""}>
          {runStatusLabel[run.status]}
          {run.started_at && run.completed_at ? ` in ${elapsed(run.completed_at - run.started_at)}` : ""}
        </span>
        {usage && <span className="font-mono text-[11px]">{usage}</span>}
        {run.completed_at && <time className="text-[11px]">{clock(run.completed_at)}</time>}
      </span>
    </div>
  );
}

function MetaButton({ onClick, title, children }: { onClick: () => void; title: string; children: ReactNode }) {
  return (
    <button onClick={onClick} title={title} className="inline-flex h-6 items-center gap-1 rounded-md px-1.5 transition-colors hover:bg-fg/[0.06] hover:text-fg">
      {children}
    </button>
  );
}

/** T3's copy button: the icon turns into a check for a moment. */
function CopyButton({ text, label = "Copy" }: { text: string; label?: string }) {
  const [done, setDone] = useState(false);
  return (
    <button
      onClick={() => {
        void navigator.clipboard?.writeText(text);
        setDone(true);
        setTimeout(() => setDone(false), 1200);
      }}
      aria-label={label}
      title={label}
      className="inline-grid size-6 place-items-center rounded-md transition-colors hover:bg-fg/[0.06] hover:text-fg"
    >
      {done ? <Check className="size-3" /> : <Copy className="size-3" />}
    </button>
  );
}

const clock = (at: number) => new Date(at).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });

/** The user's turn (T3's user row): a bubble on the right; a long one folds; its time and copy on hover. */
function UserMessage({ run }: { run: Run }) {
  const [open, setOpen] = useState(false);
  const long = run.prompt.length > 600 || run.prompt.split("\n").length > 8;
  return (
    <div className="group/user flex flex-col items-end gap-1 pb-1">
      <div className="max-w-[80%] rounded-2xl bg-message px-3.5 py-2.5 text-fg">
        {run.attachments.length > 0 && (
          <div className="mb-2 flex flex-wrap justify-end gap-1.5">
            {run.attachments.map((a, i) => (
              // eslint-disable-next-line @next/next/no-img-element
              <img key={i} src={`data:${a.mime};base64,${a.data}`} alt={a.name} className="size-16 rounded-lg border border-line object-cover" />
            ))}
          </div>
        )}
        <p
          className={`text-[14px] leading-relaxed whitespace-pre-wrap ${long && !open ? "max-h-44 overflow-hidden [mask-image:linear-gradient(to_bottom,black_70%,transparent)]" : ""}`}
        >
          {run.prompt}
        </p>
        {long && (
          <button onClick={() => setOpen((o) => !o)} className="mt-1 text-[12px] text-muted hover:text-fg">
            {open ? "Show less" : "Show full message"}
          </button>
        )}
      </div>
      {run.steers.map((s, i) => (
        <div key={i} className="max-w-[80%] rounded-2xl rounded-tr-md bg-message px-3.5 py-2 text-[13.5px] whitespace-pre-wrap">
          <span className="mr-1.5 font-mono text-[10px] tracking-wide text-accent uppercase">steer</span>
          {s}
        </div>
      ))}
      <div className="flex items-center gap-1 text-[11px] text-subtle opacity-0 transition-opacity duration-200 group-hover/user:opacity-100 focus-within:opacity-100 motion-reduce:transition-none [@media(hover:none)]:opacity-100">
        <time className="tabular-nums">{clock(run.requested_at)}</time>
        <CopyButton text={run.prompt} label="Copy the message" />
      </div>
    </div>
  );
}

/** Consecutive subagent nodes go under one "N subagents" row (T3); everything else as it is. */
type Row = { kind: "item"; item: TimelineItem } | { kind: "subagents"; ts: NodeTree[] };

export function groupSubagents(items: readonly TimelineItem[]): Row[] {
  const rows: Row[] = [];
  for (const item of items) {
    const sub = item.kind === "node" && item.t.node.body.type === "subagent";
    const last = rows.at(-1);
    if (sub && last?.kind === "subagents") last.ts.push(item.t);
    else if (sub && last?.kind === "item" && last.item.kind === "node" && last.item.t.node.body.type === "subagent") rows[rows.length - 1] = { kind: "subagents", ts: [last.item.t, item.t] };
    else rows.push({ kind: "item", item });
  }
  return rows;
}

function Items({ items, ctx, live }: { items: TimelineItem[]; ctx: Ctx; live: boolean }) {
  return (
    <>
      {groupSubagents(items).map((row) =>
        row.kind === "subagents" ? (
          <SubagentGroup key={row.ts[0].node.id} nodes={row.ts.map((t) => t.node)} now={ctx.now} open={(n) => openSubagent(n, ctx.actions)} />
        ) : row.item.kind === "work" ? (
          <WorkGroup key={row.item.id} entries={row.item.entries} />
        ) : (
          <NodeView key={row.item.t.node.id} t={row.item.t} requests={ctx.requests} actions={ctx.actions} live={live} now={ctx.now} />
        ),
      )}
    </>
  );
}

/** A delegated thread opens as itself; a provider's subagent in the subagent view (when there is one). */
function openSubagent(n: Node, actions: TranscriptActions): (() => void) | undefined {
  if (n.body.type !== "subagent") return undefined;
  const child = n.body.child;
  if (child) return () => actions.openThread(child);
  return actions.openSubagent && (() => actions.openSubagent!(n.id));
}

/** Every file the run's steps changed, the last change of each. */
function changedFiles(tree: readonly NodeTree[]): FileChange[] {
  const by = new Map<string, FileChange>();
  const walk = (ts: readonly NodeTree[]) => {
    for (const t of ts) {
      if (t.node.body.type === "tool") for (const f of t.node.body.files) by.set(f.path, f);
      walk(t.children);
    }
  };
  walk(tree);
  return [...by.values()];
}

/** Under a finished run's answer (T3's changed-files card): what changed, and the way to the diff. */
function ChangedFiles({ files, onOpen }: { files: FileChange[]; onOpen?: () => void }) {
  const diffs = files.map(toFileDiff);
  const total = diffs.reduce((s, f) => ({ files: s.files + 1, additions: s.additions + f.additions, deletions: s.deletions + f.deletions }), { files: 0, additions: 0, deletions: 0 });
  return (
    <div className="rounded-xl border border-line bg-raised" aria-label="Changed files">
      <div className="flex items-center gap-2 px-3 pt-2 pb-1.5 text-[12.5px]">
        <span className="font-medium">
          {files.length} changed file{files.length === 1 ? "" : "s"}
        </span>
        <Stat stat={total} className="text-[11.5px]" />
        {onOpen && (
          <button onClick={onOpen} className="ml-auto inline-flex h-6 items-center gap-1 rounded-md px-1.5 text-[12px] text-muted transition-colors hover:bg-fg/[0.06] hover:text-fg">
            <DiffIcon className="size-3.5" /> Open diff
          </button>
        )}
      </div>
      <ul className="px-1.5 pb-1.5">
        {diffs.slice(0, 8).map((f) => (
          <li key={f.path}>
            <button
              onClick={onOpen}
              disabled={!onOpen}
              className="grid h-7 w-full grid-cols-[minmax(0,1fr)_auto] items-center gap-3 rounded-md px-1.5 text-left font-mono text-[12px] text-muted transition-colors enabled:hover:bg-fg/[0.05] enabled:hover:text-fg"
            >
              <span className="truncate">{f.path}</span>
              <Stat stat={{ files: 1, additions: f.additions, deletions: f.deletions }} className="text-[11px]" />
            </button>
          </li>
        ))}
        {diffs.length > 8 && <li className="px-1.5 py-1 text-[11.5px] text-subtle">and {diffs.length - 8} more</li>}
      </ul>
    </div>
  );
}

export function NodeView({ t, requests, actions, live, now }: { t: NodeTree; requests: ReadonlyMap<string, Request>; actions: TranscriptActions; live: boolean; now: number }) {
  const n = t.node;
  const b = n.body;
  const running = n.status === "running";
  switch (b.type) {
    case "message":
      return (
        <div className="px-1 py-0.5 text-[14px] leading-relaxed text-fg/90">
          {b.text ? <Markdown source={b.text} className="markdown-sm" /> : null}
          {running && <span className="caret inline-block" aria-hidden />}
        </div>
      );
    case "reasoning":
    case "tool":
      return <StepRow node={n} />;
    case "plan":
      return <PlanCard text={b.text} running={running} onSave={actions.savePlan} />;
    case "todos": {
      const done = b.items.filter((i) => i.status === "completed").length;
      const current = b.items.find((i) => i.status === "in_progress");
      return (
        <div className="px-0.5">
          <p className="flex min-h-6 min-w-0 items-center gap-1.5 text-[13px] text-muted">
            <span className="grid w-6 shrink-0 place-items-center">
              <ListTodo className="size-4 opacity-70" strokeWidth={1.5} />
            </span>
            {/* T3's "Tasks 2/3 · current". */}
            <span className="shrink-0 tabular-nums">
              Tasks {done}/{b.items.length}
            </span>
            {current && <span className="min-w-0 truncate text-subtle">· {current.text}</span>}
          </p>
          <ul className="ms-7 space-y-0.5 py-0.5">
            {b.items.map((i, k) => (
              <li key={k} className="flex items-center gap-2 text-[13px]">
                {i.status === "completed" ? (
                  <CircleCheck className="size-3.5 shrink-0 text-ok" strokeWidth={1.75} />
                ) : i.status === "in_progress" ? (
                  <span className="grid size-3.5 shrink-0 place-items-center rounded-full border border-accent">
                    <span className="size-1.5 rounded-full bg-accent" />
                  </span>
                ) : (
                  <CircleDashed className="size-3.5 shrink-0 text-subtle" strokeWidth={1.75} />
                )}
                <span className={i.status === "completed" ? "text-muted line-through decoration-line-2" : i.status === "in_progress" ? "shimmer" : "text-muted"}>{i.text}</span>
              </li>
            ))}
          </ul>
        </div>
      );
    }
    case "subagent":
      return <InlineSubagent t={t} requests={requests} actions={actions} live={live} now={now} />;
    case "compaction":
      return <CompactionRow node={n} />;
    case "request": {
      const q = requests.get(b.request);
      if (!q) return null;
      // A ready plan is carried out from the composer (T3's plan banner); questions are answered
      // in the composer's panel; other approvals right here.
      if (actions.hostsPlan && q.status === "pending" && q.body.type === "approval" && q.body.action === "plan") return <PlanReadyBelow />;
      return <RequestCard request={q} onRespond={actions.respond && ((r) => actions.respond!(q.id, r))} hostedQuestions={!!actions.respond} />;
    }
    case "system":
      return <p className="text-center text-[12px] text-subtle">{b.text}</p>;
  }
}

/**
 * A subagent's row. Where the conversation can open its transcript (or it's a delegated thread)
 * the row opens it; elsewhere (a pinned card) its work opens inline under the row.
 */
function InlineSubagent({ t, requests, actions, live, now }: { t: NodeTree; requests: ReadonlyMap<string, Request>; actions: TranscriptActions; live: boolean; now: number }) {
  const [open, setOpen] = useState(false);
  const opens = openSubagent(t.node, actions);
  const inline = !opens && t.children.length > 0;
  return (
    <div>
      <SubagentRow node={t.node} now={now} onOpen={opens ?? (inline ? () => setOpen((o) => !o) : undefined)} />
      {inline && (
        <Collapse open={open}>
          <div className="ms-3 mt-1 flex flex-col gap-1 border-l border-line pl-3.5">
            <Items items={timelineItems(t.children)} ctx={{ requests, actions, now }} live={live} />
          </div>
        </Collapse>
      )}
    </div>
  );
}

/**
 * The provider compacted the context (T3's `ContextCompactionTimelineRow`): a quiet separator,
 * "Compacting context" while it runs, then "Context compacted 182k → 41k tokens", or why it failed.
 */
export function CompactionRow({ node }: { node: Node }) {
  if (node.body.type !== "compaction") return null;
  const b = node.body;
  const failed = node.status === "failed";
  const running = node.status === "running";
  const label = failed ? "Context compaction failed" : node.status === "interrupted" ? "Context compaction stopped" : running ? "Compacting context" : "Context compacted";
  const counts = !running && !failed && b.before_tokens != null && b.after_tokens != null;
  return (
    <div role="separator" aria-label={label} className="flex items-center gap-3 py-1 text-[12px]">
      <span className="h-px flex-1 bg-line" aria-hidden />
      <span className={`flex max-w-[85%] min-w-0 items-center gap-1.5 ${failed ? "text-danger" : "text-subtle"}`}>
        {failed ? <CircleAlert className="size-3.5 shrink-0" /> : <Minimize2 className="size-3.5 shrink-0" />}
        <span className={running ? "shimmer" : ""}>{label}</span>
        {counts && (
          <span className="flex shrink-0 items-center gap-1 font-mono text-[11px] tabular-nums">
            {formatContextTokens(b.before_tokens!)} <ArrowRight className="size-3" aria-label="to" /> {formatContextTokens(b.after_tokens!)} tokens
          </span>
        )}
        {failed && b.error && <span className="min-w-0 truncate text-muted" title={b.error}>· {b.error}</span>}
      </span>
      <span className="h-px flex-1 bg-line" aria-hidden />
    </div>
  );
}

/** A plan waiting to be carried out, whose buttons are on the composer below. */
function PlanReadyBelow() {
  return (
    <p className="flex min-h-6 items-center gap-1.5 px-0.5 text-[13px] text-muted">
      <span className="grid w-6 place-items-center">
        <ListChecks className="size-4 text-accent" strokeWidth={1.5} />
      </span>
      Plan ready
      <span className="text-subtle">· implement or refine it below</span>
    </p>
  );
}

/**
 * A provider subagent's own transcript (T3 opens it as a thread of its own): what it was asked as
 * the user's turn, then its work and words, then its answer.
 */
export function SubagentTranscript({ t, requests, actions, now }: { t: NodeTree; requests: ReadonlyMap<string, Request>; actions: TranscriptActions; now: number }) {
  if (t.node.body.type !== "subagent") return null;
  const b = t.node.body;
  const live = t.node.status === "running" || t.node.status === "waiting";
  const items = timelineItems(t.children);
  const lastSaid = items.at(-1);
  const answered = lastSaid?.kind === "node" && lastSaid.t.node.body.type === "message" && lastSaid.t.node.body.text.trim() !== "";
  return (
    <section className="flex flex-col gap-3" aria-label={`Subagent ${b.agent}`}>
      <div className="flex flex-col items-end gap-1 pb-1">
        <div className="max-w-[80%] rounded-2xl bg-message px-3.5 py-2.5 text-[14px] leading-relaxed whitespace-pre-wrap text-fg">{b.prompt || b.agent}</div>
      </div>
      <Items items={items} ctx={{ requests, actions, now }} live={live} />
      {live ? (
        <p className="flex h-6 items-center gap-2 px-1 text-[13px] text-muted tabular-nums">
          <span className="pulse-dot size-1.5 rounded-full bg-accent" aria-hidden />
          <span className="shimmer">{b.progress?.trim() || "Working"}</span>
        </p>
      ) : (
        b.result &&
        !answered && (
          <div className="px-1 py-0.5 text-[14px] leading-relaxed text-fg/90">
            <Markdown source={b.result} className="markdown-sm" />
          </div>
        )
      )}
    </section>
  );
}

export function EmptyTranscript({ children }: { children: ReactNode }) {
  return <div className="grid h-full min-h-48 place-items-center text-[13.5px] text-muted">{children}</div>;
}
