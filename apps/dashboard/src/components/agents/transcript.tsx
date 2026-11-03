"use client";

import {
  Check,
  ChevronRight,
  CircleAlert,
  CircleDashed,
  FilePen,
  FileSearch,
  Globe,
  ListChecks,
  Plug,
  RotateCcw,
  Search,
  SquareTerminal,
  Split,
  Wrench,
  X,
} from "lucide-react";
import { useState, type ReactNode } from "react";
import type { Checkpoint, Node, Request, RequestResponse, Run, ToolKind } from "@/lib/agents-api";
import type { NodeTree } from "@/lib/agents-store";
import { FileDiffView } from "../code/diff";
import { Markdown } from "../code/markdown";
import { RequestCard, toFileDiff } from "./request-card";
import { elapsed, runStatusLabel, Stat, usageLine } from "./shared";

export interface TranscriptActions {
  respond: (request: string, r: RequestResponse) => Promise<unknown>;
  /** Show this run's diff (or the whole thread's with `null`). */
  showDiff: (run: string | null) => void;
  rollback?: (checkpoint: Checkpoint) => Promise<unknown>;
  fork?: (run: Run) => Promise<unknown>;
  openThread: (thread: string) => void;
}

/** One run: what the user sent, then the agent's work in order, then how it ended. */
export function RunView({
  run,
  tree,
  requests,
  checkpoint,
  now,
  actions,
  busyThread,
}: {
  run: Run;
  tree: NodeTree[];
  requests: ReadonlyMap<string, Request>;
  checkpoint: Checkpoint | undefined;
  now: number;
  actions: TranscriptActions;
  busyThread: boolean;
}) {
  const live = !["completed", "interrupted", "failed", "cancelled", "rolled_back"].includes(run.status);
  const [confirmRollback, setConfirmRollback] = useState(false);
  return (
    <section className={`space-y-3 ${run.status === "rolled_back" ? "opacity-45" : ""}`} aria-label={`Run ${run.ordinal}`}>
      <UserMessage run={run} />
      <Steps tree={tree} requests={requests} actions={actions} live={live} />
      {live ? (
        <p className="flex items-center gap-2 pl-0.5 text-[12px]">
          <span className="size-1.5 rounded-full bg-accent" aria-hidden />
          <span className="shimmer">
            {run.status === "waiting" ? "Waiting for you" : "Working"}
            {run.started_at ? ` for ${elapsed(now - run.started_at)}` : "…"}
          </span>
        </p>
      ) : (
        <div className="flex flex-wrap items-center gap-x-3 gap-y-1 border-t border-line pt-2 text-[11.5px] text-subtle">
          <span className={run.status === "failed" ? "text-danger" : run.status === "completed" ? "" : "text-muted"}>
            {runStatusLabel[run.status]}
            {run.started_at && run.completed_at ? ` in ${elapsed(run.completed_at - run.started_at)}` : ""}
          </span>
          {usageLine(run.usage) && <span className="font-mono">{usageLine(run.usage)}</span>}
          {run.stat && (run.stat.additions > 0 || run.stat.deletions > 0) && (
            <button onClick={() => actions.showDiff(run.id)} className="flex items-center gap-1 hover:text-fg" title="This run's diff">
              {run.stat.files} file{run.stat.files === 1 ? "" : "s"} <Stat stat={run.stat} />
            </button>
          )}
          <span className="ml-auto flex items-center gap-2">
            {actions.fork && run.status !== "rolled_back" && (
              <button onClick={() => void actions.fork!(run)} className="flex items-center gap-1 hover:text-fg" title="Start a new thread from this point">
                <Split className="size-3" /> Fork
              </button>
            )}
            {actions.rollback && checkpoint && run.status !== "rolled_back" && !busyThread && (
              confirmRollback ? (
                <span className="flex items-center gap-1.5">
                  <span className="text-muted">Restore files and drop later runs?</span>
                  <button
                    onClick={() => {
                      setConfirmRollback(false);
                      void actions.rollback!(checkpoint);
                    }}
                    className="text-danger hover:underline"
                  >
                    Roll back
                  </button>
                  <button onClick={() => setConfirmRollback(false)} className="hover:text-fg" aria-label="Keep">
                    <X className="size-3" />
                  </button>
                </span>
              ) : (
                <button onClick={() => setConfirmRollback(true)} className="flex items-center gap-1 hover:text-fg" title="Restore the files to how they were after this run">
                  <RotateCcw className="size-3" /> Roll back to here
                </button>
              )
            )}
          </span>
        </div>
      )}
      {run.error && <p className="rounded-lg border border-danger/40 bg-danger/10 px-3 py-2 text-[12px] text-danger">{run.error}</p>}
    </section>
  );
}

function UserMessage({ run }: { run: Run }) {
  const [open, setOpen] = useState(false);
  const long = run.prompt.length > 600 || run.prompt.split("\n").length > 10;
  return (
    <div className="ml-auto max-w-[85%] space-y-1.5">
      <div className="rounded-xl border border-line bg-panel-2 px-3 py-2">
        {run.attachments.length > 0 && (
          <div className="mb-2 flex flex-wrap gap-1.5">
            {run.attachments.map((a, i) => (
              // eslint-disable-next-line @next/next/no-img-element
              <img key={i} src={`data:${a.mime};base64,${a.data}`} alt={a.name} className="size-16 rounded-md border border-line object-cover" />
            ))}
          </div>
        )}
        <p className={`text-[13px] leading-relaxed whitespace-pre-wrap ${long && !open ? "line-clamp-[10]" : ""}`}>{run.prompt}</p>
        {long && (
          <button onClick={() => setOpen((o) => !o)} className="mt-1 text-[11.5px] text-subtle hover:text-fg">
            {open ? "Show less" : "Show all"}
          </button>
        )}
      </div>
      {run.steers.map((s, i) => (
        <div key={i} className="rounded-xl border border-line px-3 py-1.5 text-[12.5px] whitespace-pre-wrap">
          <span className="mr-1.5 font-mono text-[10px] tracking-wide text-accent uppercase">steer</span>
          {s}
        </div>
      ))}
    </div>
  );
}

/** A run's nodes. In a finished run, consecutive tool calls fold into one "N steps" row. */
function Steps({ tree, requests, actions, live, nested = false }: { tree: NodeTree[]; requests: ReadonlyMap<string, Request>; actions: TranscriptActions; live: boolean; nested?: boolean }) {
  const groups: (NodeTree | NodeTree[])[] = [];
  for (const t of tree) {
    const last = groups[groups.length - 1];
    if (t.node.body.type === "tool" && !live && !nested) {
      if (Array.isArray(last)) last.push(t);
      else groups.push([t]);
    } else groups.push(t);
  }
  return (
    <div className={`space-y-2.5 ${nested ? "border-l border-line-2 pl-3" : ""}`}>
      {groups.map((g) =>
        Array.isArray(g) ? (
          g.length === 1 ? (
            <NodeView key={g[0].node.id} t={g[0]} requests={requests} actions={actions} live={live} />
          ) : (
            <ToolGroup key={g[0].node.id} items={g} requests={requests} actions={actions} />
          )
        ) : (
          <NodeView key={g.node.id} t={g} requests={requests} actions={actions} live={live} />
        ),
      )}
    </div>
  );
}

function ToolGroup({ items, requests, actions }: { items: NodeTree[]; requests: ReadonlyMap<string, Request>; actions: TranscriptActions }) {
  const [open, setOpen] = useState(false);
  const failed = items.some((t) => t.node.status === "failed");
  const files = new Set(items.flatMap((t) => (t.node.body.type === "tool" ? t.node.body.files.map((f) => f.path) : [])));
  return (
    <div>
      <button onClick={() => setOpen((o) => !o)} className="flex items-center gap-1.5 text-[12px] text-subtle hover:text-fg" aria-expanded={open}>
        <ChevronRight className={`size-3 transition-transform ${open ? "rotate-90" : ""}`} />
        {items.length} steps
        {files.size > 0 && ` · ${files.size} file${files.size === 1 ? "" : "s"} changed`}
        {failed && <CircleAlert className="size-3 text-danger" aria-label="a step failed" />}
      </button>
      {open && (
        <div className="mt-2 space-y-2 pl-4">
          {items.map((t) => (
            <NodeView key={t.node.id} t={t} requests={requests} actions={actions} live={false} />
          ))}
        </div>
      )}
    </div>
  );
}

const toolIcon: Record<ToolKind, typeof Wrench> = {
  command: SquareTerminal,
  edit: FilePen,
  read: FileSearch,
  search: Search,
  web: Globe,
  mcp: Plug,
  other: Wrench,
};

export function NodeView({ t, requests, actions, live }: { t: NodeTree; requests: ReadonlyMap<string, Request>; actions: TranscriptActions; live: boolean }) {
  const n = t.node;
  const b = n.body;
  const running = n.status === "running";
  switch (b.type) {
    case "message":
      return (
        <div className="text-[13.5px]">
          {b.text ? <Markdown source={b.text} className="markdown-sm" /> : null}
          {running && <span className="caret inline-block" aria-hidden />}
        </div>
      );
    case "reasoning":
      return <Reasoning text={b.text} running={running} />;
    case "plan":
      return (
        <div className="rounded-xl border border-line bg-panel-2/50 p-3">
          <p className="mb-2 flex items-center gap-1.5 text-[11px] tracking-wide text-subtle uppercase">
            <ListChecks className="size-3.5" /> Plan
          </p>
          <Markdown source={b.text} className="markdown-sm" />
          {running && <span className="caret inline-block" aria-hidden />}
        </div>
      );
    case "todos": {
      const done = b.items.filter((i) => i.status === "completed").length;
      return (
        <div className="rounded-xl border border-line px-3 py-2">
          <p className="mb-1.5 text-[11.5px] text-subtle">
            Tasks {done}/{b.items.length}
          </p>
          <ul className="space-y-1">
            {b.items.map((i, k) => (
              <li key={k} className="flex items-center gap-2 text-[12.5px]">
                {i.status === "completed" ? (
                  <Check className="size-3.5 text-ok" />
                ) : i.status === "in_progress" ? (
                  <span className="grid size-3.5 place-items-center">
                    <span className="size-1.5 rounded-full bg-accent" />
                  </span>
                ) : (
                  <CircleDashed className="size-3.5 text-subtle" />
                )}
                <span className={i.status === "completed" ? "text-muted line-through decoration-line-2" : i.status === "in_progress" ? "shimmer" : "text-muted"}>{i.text}</span>
              </li>
            ))}
          </ul>
        </div>
      );
    }
    case "tool":
      return <ToolView node={n} />;
    case "subagent":
      return (
        <div className="rounded-xl border border-line p-3">
          <p className="flex items-center gap-2 text-[12.5px]">
            <span className={`size-1.5 rounded-full ${running ? "bg-accent" : n.status === "failed" ? "bg-danger" : "bg-subtle"}`} aria-hidden />
            <span className="font-medium">{b.agent}</span>
            <span className="truncate text-muted">{b.prompt}</span>
            {b.child && (
              <button onClick={() => actions.openThread(b.child!)} className="ml-auto shrink-0 text-[11.5px] text-accent hover:underline">
                Open thread
              </button>
            )}
          </p>
          {t.children.length > 0 && (
            <div className="mt-2.5">
              <Steps tree={t.children} requests={requests} actions={actions} live={live} nested />
            </div>
          )}
          {b.result && <p className="mt-2 text-[12px] text-muted">{b.result}</p>}
        </div>
      );
    case "request": {
      const q = requests.get(b.request);
      if (!q) return null;
      return (
        <div className={q.status === "pending" ? "rounded-xl border border-accent/45 p-3" : ""}>
          <RequestCard request={q} onRespond={(r) => actions.respond(q.id, r)} />
        </div>
      );
    }
    case "system":
      return <p className="text-center text-[11.5px] text-subtle">{b.text}</p>;
  }
}

function Reasoning({ text, running }: { text: string; running: boolean }) {
  const [open, setOpen] = useState(false);
  return (
    <div>
      <button onClick={() => setOpen((o) => !o)} className="flex items-center gap-1.5 text-[12px] text-subtle hover:text-fg" aria-expanded={open}>
        <ChevronRight className={`size-3 transition-transform ${open ? "rotate-90" : ""}`} />
        <span className={running ? "shimmer" : ""}>{running ? "Thinking" : "Thought"}</span>
      </button>
      {open && <p className="mt-1.5 border-l border-line-2 pl-3 text-[12.5px] leading-relaxed whitespace-pre-wrap text-muted">{text}</p>}
    </div>
  );
}

function ToolView({ node }: { node: Node }) {
  const b = node.body as Extract<Node["body"], { type: "tool" }>;
  const running = node.status === "running" || node.status === "waiting";
  const [open, setOpen] = useState(false);
  const Icon = toolIcon[b.kind];
  const failed = node.status === "failed" || (b.exit_code !== null && b.exit_code !== 0);
  const hasBody = !!b.output || b.files.length > 0 || (b.kind !== "command" && b.input != null);
  const showOutput = open || (running && !!b.output);
  return (
    <div>
      <button
        onClick={() => hasBody && setOpen((o) => !o)}
        className="flex w-full min-w-0 items-center gap-2 text-left text-[12.5px] text-muted hover:text-fg"
        aria-expanded={hasBody ? open : undefined}
      >
        <Icon className="size-3.5 shrink-0 text-subtle" />
        <span className={`truncate font-mono text-[12px] ${running ? "shimmer" : ""}`}>{b.title}</span>
        {b.files.length > 0 && (
          <Stat stat={b.files.map(toFileDiff).reduce((s, f) => ({ files: s.files + 1, additions: s.additions + f.additions, deletions: s.deletions + f.deletions }), { files: 0, additions: 0, deletions: 0 })} className="shrink-0 text-[11px]" />
        )}
        <span className="ml-auto shrink-0">
          {running ? null : failed ? (
            <span className="font-mono text-[11px] text-danger">{b.exit_code !== null ? `exit ${b.exit_code}` : "failed"}</span>
          ) : (
            <Check className="size-3.5 text-subtle" />
          )}
        </span>
      </button>
      {(showOutput || open) && (
        <div className="mt-1.5 space-y-2 pl-5">
          {b.output && (
            <pre className="max-h-64 overflow-auto rounded-lg border border-line bg-bg px-2.5 py-2 font-mono text-[11.5px] leading-relaxed whitespace-pre-wrap text-muted" data-scroll>
              {b.output}
            </pre>
          )}
          {open && b.kind !== "command" && b.kind !== "edit" && b.input != null && (
            <pre className="max-h-40 overflow-auto rounded-lg border border-line bg-bg px-2.5 py-2 font-mono text-[11px] text-subtle" data-scroll>
              {JSON.stringify(b.input, null, 2)}
            </pre>
          )}
          {open && b.files.map((f) => <FileDiffView key={f.path} file={toFileDiff(f)} />)}
        </div>
      )}
    </div>
  );
}

export function EmptyTranscript({ children }: { children: ReactNode }) {
  return <div className="grid h-full place-items-center text-[13px] text-subtle">{children}</div>;
}
