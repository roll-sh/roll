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
  Bot,
  Check,
  ChevronRight,
  CircleCheck,
  CircleDashed,
  Copy,
  FileDiff as DiffIcon,
  ListTodo,
  RotateCcw,
  Split,
  X,
} from "lucide-react";
import { useState, type ReactNode } from "react";
import type { Checkpoint, FileChange, Request, RequestResponse, Run } from "@/lib/agents-api";
import type { NodeTree } from "@/lib/agents-store";
import { ended, foldLabel, foldTurn, timelineItems, type TimelineItem } from "@/lib/agents-worklog";
import { Markdown } from "../code/markdown";
import { Collapse } from "./animated-height";
import { RequestCard, toFileDiff } from "./request-card";
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
}

interface Ctx {
  requests: ReadonlyMap<string, Request>;
  actions: TranscriptActions;
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
  const ctx: Ctx = { requests, actions };
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

function Items({ items, ctx, live }: { items: TimelineItem[]; ctx: Ctx; live: boolean }) {
  return (
    <>
      {items.map((i) => (i.kind === "work" ? <WorkGroup key={i.id} entries={i.entries} /> : <NodeView key={i.t.node.id} t={i.t} requests={ctx.requests} actions={ctx.actions} live={live} />))}
    </>
  );
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

export function NodeView({ t, requests, actions, live }: { t: NodeTree; requests: ReadonlyMap<string, Request>; actions: TranscriptActions; live: boolean }) {
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
      return <PlanCard text={b.text} running={running} />;
    case "todos": {
      const done = b.items.filter((i) => i.status === "completed").length;
      return (
        <div className="px-0.5">
          <p className="flex min-h-6 items-center gap-1.5 text-[13px] text-muted">
            <span className="grid w-6 place-items-center">
              <ListTodo className="size-4 opacity-70" strokeWidth={1.5} />
            </span>
            Tasks {done}/{b.items.length}
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
      return (
        <div className="px-0.5">
          <p className="flex min-h-6 items-center gap-1.5 text-[13px]">
            <span className="grid w-6 shrink-0 place-items-center">
              <Bot className={`size-4 ${running ? "text-accent" : n.status === "failed" ? "text-danger/70" : "text-muted opacity-70"}`} strokeWidth={1.5} />
            </span>
            <span className="shrink-0 font-medium">{b.agent}</span>
            <span className={`min-w-0 flex-1 truncate text-muted ${running ? "shimmer" : ""}`}>{b.prompt}</span>
            {b.child && (
              <button onClick={() => actions.openThread(b.child!)} className="shrink-0 rounded-md px-1.5 text-[12px] text-accent hover:underline">
                Open thread
              </button>
            )}
          </p>
          {t.children.length > 0 && (
            <div className="ms-3 mt-1 flex flex-col gap-1 border-l border-line pl-3.5">
              <Items items={timelineItems(t.children)} ctx={{ requests, actions }} live={live} />
            </div>
          )}
          {b.result && <p className="ms-7 mt-1 text-[12.5px] text-muted">{b.result}</p>}
        </div>
      );
    case "request": {
      const q = requests.get(b.request);
      if (!q) return null;
      // Questions are answered in the composer's panel below; approvals right here.
      return <RequestCard request={q} onRespond={actions.respond && ((r) => actions.respond!(q.id, r))} hostedQuestions={!!actions.respond} />;
    }
    case "system":
      return <p className="text-center text-[12px] text-subtle">{b.text}</p>;
  }
}

/** A plan (T3's proposed-plan card): a card with its badge; a long one folds until expanded. */
function PlanCard({ text, running }: { text: string; running: boolean }) {
  const [open, setOpen] = useState(false);
  const long = text.length > 1400 || text.split("\n").length > 24;
  return (
    <div className="rounded-2xl border border-line bg-raised p-4">
      <div className="mb-2 flex items-center gap-2">
        <span className="rounded-full border border-line-2 px-2 py-px text-[11px] font-medium text-muted">Plan</span>
        <span className="ml-auto text-subtle">
          <CopyButton text={text} label="Copy the plan" />
        </span>
      </div>
      <div className={long && !open ? "max-h-104 overflow-hidden [mask-image:linear-gradient(to_bottom,black_75%,transparent)]" : ""}>
        <Markdown source={text} className="markdown-sm" />
        {running && <span className="caret inline-block" aria-hidden />}
      </div>
      {long && (
        <button onClick={() => setOpen((o) => !o)} className="mt-2 text-[12.5px] text-muted hover:text-fg">
          {open ? "Collapse plan" : "Expand plan"}
        </button>
      )}
    </div>
  );
}

export function EmptyTranscript({ children }: { children: ReactNode }) {
  return <div className="grid h-full min-h-48 place-items-center text-[13.5px] text-muted">{children}</div>;
}
