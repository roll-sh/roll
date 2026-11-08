"use client";

/**
 * The work log's rows (T3 Code's `components/chat/WorkLog.tsx` and `TimelineWorkGroupRows.tsx`,
 * MIT): a step is an icon, what it did and, on a click, its output, the files it changed or its
 * input; a group of steps is one sentence that opens to every step, or the step it's on while
 * live. The transcript and the requests on the canvas (the steps that led to them) show these.
 */

import { Brain, ChevronRight, Eye, Globe, Hammer, Plug, Search, SquarePen, SquareTerminal, Wrench } from "lucide-react";
import { useState } from "react";
import type { Node, ToolKind } from "@/lib/agents-api";
import type { NodeTree } from "@/lib/agents-store";
import { stepFailed, stepLabel, summarize, thoughtLine } from "@/lib/agents-worklog";
import { FileDiffView } from "../code/diff";
import { Markdown } from "../code/markdown";
import { Collapse } from "./animated-height";
import { SlidePages } from "./slide-pages";
import { toFileDiff } from "./request-card";
import { Stat } from "./shared";

export const toolIcon: Record<ToolKind, typeof Wrench> = {
  command: SquareTerminal,
  edit: SquarePen,
  read: Eye,
  search: Search,
  web: Globe,
  mcp: Plug,
  other: Wrench,
};

export function stepIcon(n: Node): typeof Wrench {
  if (n.body.type === "reasoning") return Brain;
  if (n.body.type === "tool") return toolIcon[n.body.kind];
  return Wrench;
}

export const stepRunning = (n: Node) => n.status === "running" || n.status === "waiting";

/**
 * A group of steps (T3's work group): one step is its own row; a live group shows the step it's
 * on; a finished one is a sentence that opens to every step. A lone step that gets company
 * crossfades into the group's row in place.
 */
export function WorkGroup({ entries }: { entries: NodeTree[] }) {
  const one = entries.length === 1;
  return (
    <div className="relative">
      <SlidePages page={one ? "step" : "group"} direction={0}>
        {one ? <StepRow node={entries[0].node} /> : <StepGroup entries={entries} />}
      </SlidePages>
    </div>
  );
}

function StepGroup({ entries }: { entries: NodeTree[] }) {
  const [open, setOpen] = useState(false);
  const current = [...entries].reverse().find((t) => stepRunning(t.node));
  const { summary, failed } = summarize(entries);
  const kinds = new Set(entries.map((t) => (t.node.body.type === "tool" ? t.node.body.kind : "reasoning")));
  const Icon = current ? stepIcon(current.node) : kinds.size === 1 ? stepIcon(entries[0].node) : Hammer;
  return (
    <div className="rounded-md">
      <button onClick={() => setOpen((o) => !o)} aria-expanded={open} className="group/step flex min-h-6 w-full min-w-0 items-center gap-1.5 rounded-md px-0.5 text-left text-[13px] leading-relaxed">
        <span className="grid w-6 shrink-0 place-items-center">
          <Icon className={`size-4 ${failed && !current ? "text-danger/70" : "text-muted opacity-70"}`} strokeWidth={1.5} />
        </span>
        <span className={`min-w-0 flex-1 truncate ${current ? "" : "text-muted group-hover/step:text-fg"}`}>
          {current ? <span className="shimmer">{stepLabel(current.node)}</span> : summary}
          {current && <span className="ml-1.5 text-subtle tabular-nums">· {entries.length} steps</span>}
        </span>
        <ChevronRight className={`size-3 shrink-0 text-subtle transition-transform duration-200 motion-reduce:transition-none ${open ? "rotate-90" : ""}`} />
      </button>
      <Collapse open={open}>
        <div className="flex flex-col pt-0.5">
          {entries.map((t) => (
            <StepRow key={t.node.id} node={t.node} grouped />
          ))}
        </div>
      </Collapse>
    </div>
  );
}

/** One step (T3's work row): icon, what it did, and its output, the files it changed or its input on a click. */
export function StepRow({ node, grouped = false }: { node: Node; grouped?: boolean }) {
  const [open, setOpen] = useState(false);
  const b = node.body;
  const running = stepRunning(node);
  const failed = stepFailed(node);
  const Icon = stepIcon(node);
  const tool = b.type === "tool" ? b : null;
  const thought = b.type === "reasoning" ? b.text : "";
  const hasBody = tool ? !!tool.output || tool.files.length > 0 || (tool.kind !== "command" && tool.input != null) : !!thought.trim();
  // A running command shows what it prints.
  const showOutput = open || (running && !!tool?.output);
  const label = stepLabel(node);
  const mono = tool?.kind === "command";
  const stat = tool && tool.files.length > 0 ? tool.files.map(toFileDiff).reduce((s, f) => ({ files: s.files + 1, additions: s.additions + f.additions, deletions: s.deletions + f.deletions }), { files: 0, additions: 0, deletions: 0 }) : null;
  return (
    <div className={`rounded-md px-0.5 ${grouped ? "" : "py-0.5"}`}>
      <button
        onClick={() => hasBody && setOpen((o) => !o)}
        aria-expanded={hasBody ? open : undefined}
        className={`group/step flex min-h-6 w-full min-w-0 items-center gap-1.5 text-left text-[13px] leading-relaxed ${hasBody ? "" : "cursor-default"}`}
      >
        <span className="grid w-6 shrink-0 place-items-center">
          <Icon className={`size-4 ${failed ? "text-danger/70" : "text-muted opacity-70"}`} strokeWidth={1.5} />
        </span>
        <span className={`min-w-0 flex-1 truncate ${mono ? "font-mono text-[12.5px]" : ""} ${running ? "" : "text-muted group-hover/step:text-fg"}`}>
          {running ? <span className="shimmer">{label}</span> : label}
          {running && b.type === "reasoning" && thoughtLine(thought) && <span className="ml-1.5 text-subtle">{thoughtLine(thought)}</span>}
        </span>
        {stat && <Stat stat={stat} className="shrink-0 text-[11px]" />}
        {failed && <span className="shrink-0 font-mono text-[11px] text-danger">{tool?.exit_code != null ? `exit ${tool.exit_code}` : "failed"}</span>}
        {hasBody && <ChevronRight className={`size-3 shrink-0 text-subtle transition-transform duration-200 motion-reduce:transition-none ${open ? "rotate-90" : ""}`} />}
      </button>
      <Collapse open={showOutput}>
        <div className="ms-7 max-h-96 space-y-2 overflow-auto px-0.5 py-1" data-scroll>
          {b.type === "reasoning" && <Markdown source={thought} className="markdown-sm text-muted" />}
          {tool?.output && (
            <pre className="max-h-64 overflow-auto rounded-lg border border-line bg-bg px-2.5 py-2 font-mono text-[11.5px] leading-relaxed whitespace-pre-wrap text-muted" data-scroll>
              {tool.output}
            </pre>
          )}
          {open && tool && tool.kind !== "command" && tool.kind !== "edit" && tool.input != null && (
            <pre className="max-h-40 overflow-auto rounded-lg border border-line bg-bg px-2.5 py-2 font-mono text-[11px] text-subtle" data-scroll>
              {JSON.stringify(tool.input, null, 2)}
            </pre>
          )}
          {open && tool?.files.map((f) => <FileDiffView key={f.path} file={toFileDiff(f)} />)}
        </div>
      </Collapse>
    </div>
  );
}

