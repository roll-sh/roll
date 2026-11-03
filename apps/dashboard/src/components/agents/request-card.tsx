"use client";

import { FileDiff as FileIcon, ListChecks, MessageCircleQuestion, ShieldQuestion, SquareTerminal, Wrench } from "lucide-react";
import { useState, type ReactNode } from "react";
import type { Approval, ApprovalAction, Decision, FileChange, Question, Request, RequestResponse } from "@/lib/agents-api";
import type { FileDiff } from "@/lib/code-api";
import { FileDiffView } from "../code/diff";
import { btn, input, primary } from "../ui";

/** An agent's file change in the shape the code diff component takes. */
export function toFileDiff(f: FileChange): FileDiff {
  let additions = 0;
  let deletions = 0;
  for (const l of (f.diff ?? "").split("\n")) {
    if (l.startsWith("+") && !l.startsWith("+++")) additions++;
    else if (l.startsWith("-") && !l.startsWith("---")) deletions++;
  }
  return {
    path: f.path,
    old_path: null,
    change: f.change === "add" ? "added" : f.change === "delete" ? "deleted" : "modified",
    mode: null,
    old_mode: null,
    binary: false,
    additions,
    deletions,
    patch: f.diff,
  };
}

const actionIcon: Record<ApprovalAction, typeof SquareTerminal> = {
  command: SquareTerminal,
  file_change: FileIcon,
  permission: ShieldQuestion,
  tool: Wrench,
  plan: ListChecks,
};

const decisionLabel: Record<Decision, string> = {
  accept: "Approve",
  accept_for_session: "Approve for session",
  decline: "Decline",
  cancel: "Stop run",
};

/**
 * A pending approval or question, answered in place: on the canvas next to its thread, inside the
 * open session, and in the header's "rolling" popover. Resolved requests render as a one-line
 * record of what was decided.
 */
export function RequestCard({
  request,
  onRespond,
  context,
  compact = false,
}: {
  request: Request;
  onRespond: (r: RequestResponse) => Promise<unknown>;
  /** A line above the title (the thread's name, in the popover). */
  context?: ReactNode;
  compact?: boolean;
}) {
  if (request.status !== "pending") return <Resolved request={request} />;
  return request.body.type === "approval" ? (
    <ApprovalForm approval={request.body} onRespond={onRespond} context={context} compact={compact} />
  ) : (
    <QuestionsForm questions={request.body.questions} onRespond={onRespond} context={context} />
  );
}

function useSend(onRespond: (r: RequestResponse) => Promise<unknown>) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const send = async (r: RequestResponse) => {
    setBusy(true);
    setError(null);
    try {
      await onRespond(r);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setBusy(false);
    }
  };
  return { busy, error, send };
}

function ApprovalForm({
  approval,
  onRespond,
  context,
  compact,
}: {
  approval: Approval;
  onRespond: (r: RequestResponse) => Promise<unknown>;
  context?: ReactNode;
  compact: boolean;
}) {
  const { busy, error, send } = useSend(onRespond);
  const [declining, setDeclining] = useState(false);
  const [note, setNote] = useState("");
  const [openFile, setOpenFile] = useState<string | null>(null);
  const Icon = actionIcon[approval.action];
  const has = (d: Decision) => approval.decisions.includes(d);
  const decide = (decision: Decision, message?: string) => send({ type: "decision", decision, ...(message ? { message } : {}) });

  return (
    <div className="space-y-3" role="group" aria-label={`Approval: ${approval.title}`}>
      <div className="flex items-start gap-2">
        <Icon className="mt-0.5 size-4 shrink-0 text-accent" />
        <div className="min-w-0 flex-1">
          {context && <div className="mb-0.5 truncate text-[11px] text-subtle">{context}</div>}
          <p className="text-[13px] leading-snug font-medium">{approval.title}</p>
          {approval.reason && <p className="mt-0.5 text-[12px] text-muted">{approval.reason}</p>}
        </div>
      </div>
      {approval.command && (
        <pre className="max-h-32 overflow-auto rounded-lg border border-line bg-bg px-2.5 py-2 font-mono text-[11.5px] leading-relaxed whitespace-pre-wrap" data-scroll>
          <span className="text-subtle select-none">$ </span>
          {approval.command}
        </pre>
      )}
      {approval.cwd && !compact && <p className="truncate font-mono text-[11px] text-subtle">in {approval.cwd}</p>}
      {approval.files.length > 0 && (
        <ul className="space-y-1">
          {approval.files.map((f) => (
            <li key={f.path}>
              <button
                type="button"
                onClick={() => setOpenFile((o) => (o === f.path ? null : f.path))}
                className="flex w-full items-center gap-2 truncate text-left font-mono text-[11.5px] text-muted hover:text-fg"
                disabled={!f.diff}
              >
                <span className="w-3 text-subtle">{f.change === "add" ? "A" : f.change === "delete" ? "D" : f.change === "rename" ? "R" : "M"}</span>
                <span className="truncate">{f.path}</span>
              </button>
              {openFile === f.path && f.diff && (
                <div className="mt-1.5 max-h-72 overflow-auto" data-scroll>
                  <FileDiffView file={toFileDiff(f)} />
                </div>
              )}
            </li>
          ))}
        </ul>
      )}
      {!approval.command && approval.tool && approval.input != null && !compact && (
        <pre className="max-h-32 overflow-auto rounded-lg border border-line bg-bg px-2.5 py-2 font-mono text-[11px] text-muted" data-scroll>
          {approval.tool} {JSON.stringify(approval.input, null, 2)}
        </pre>
      )}

      {declining ? (
        <form
          className="space-y-2"
          onSubmit={(e) => {
            e.preventDefault();
            void decide("decline", note.trim() || undefined);
          }}
        >
          <textarea
            autoFocus
            rows={2}
            value={note}
            onChange={(e) => setNote(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey) {
                e.preventDefault();
                void decide("decline", note.trim() || undefined);
              }
              if (e.key === "Escape") setDeclining(false);
            }}
            placeholder="Tell the agent what to do instead (optional)"
            aria-label="Note to the agent"
            className={`${input} w-full resize-none`}
          />
          <div className="flex gap-2">
            <button type="submit" disabled={busy} className={primary}>
              {busy ? <span className="shimmer">Sending…</span> : "Decline"}
            </button>
            <button type="button" onClick={() => setDeclining(false)} className={btn}>
              Back
            </button>
          </div>
        </form>
      ) : (
        <div className="flex flex-wrap items-center gap-1.5">
          {has("accept") && (
            <button type="button" disabled={busy} onClick={() => void decide("accept")} className={primary}>
              {busy ? <span className="shimmer">Sending…</span> : decisionLabel.accept}
            </button>
          )}
          {has("accept_for_session") && (
            <button type="button" disabled={busy} onClick={() => void decide("accept_for_session")} className={btn}>
              {decisionLabel.accept_for_session}
            </button>
          )}
          {has("decline") && (
            <button type="button" disabled={busy} onClick={() => setDeclining(true)} className={btn}>
              {decisionLabel.decline}
            </button>
          )}
          {has("cancel") && (
            <button type="button" disabled={busy} onClick={() => void decide("cancel")} className={`${btn} ml-auto`}>
              {decisionLabel.cancel}
            </button>
          )}
        </div>
      )}
      {error && (
        <p className="text-[12px] text-danger" role="alert">
          {error}
        </p>
      )}
    </div>
  );
}

const decidedLabel: Record<Decision, string> = {
  accept: "Approved",
  accept_for_session: "Approved for the session",
  decline: "Declined",
  cancel: "Stopped the run",
};

const OTHER = "\u0000other";

function QuestionsForm({
  questions,
  onRespond,
  context,
}: {
  questions: Question[];
  onRespond: (r: RequestResponse) => Promise<unknown>;
  context?: ReactNode;
}) {
  const { busy, error, send } = useSend(onRespond);
  const [picked, setPicked] = useState<Record<string, string[]>>({});
  const [other, setOther] = useState<Record<string, string>>({});

  const answerOf = (q: Question): string | null => {
    const sel = picked[q.id] ?? [];
    const labels = sel.filter((s) => s !== OTHER);
    const custom = sel.includes(OTHER) || q.options.length === 0 ? (other[q.id] ?? "").trim() : "";
    const all = custom ? [...labels, custom] : labels;
    return all.length ? all.join(", ") : null;
  };
  const complete = questions.every((q) => answerOf(q) !== null);

  const toggle = (q: Question, label: string) =>
    setPicked((p) => {
      const cur = p[q.id] ?? [];
      if (!q.multi) return { ...p, [q.id]: [label] };
      return { ...p, [q.id]: cur.includes(label) ? cur.filter((x) => x !== label) : [...cur, label] };
    });

  return (
    <form
      className="space-y-3"
      aria-label="Questions from the agent"
      onSubmit={(e) => {
        e.preventDefault();
        if (!complete) return;
        const answers: Record<string, string> = {};
        for (const q of questions) answers[q.id] = answerOf(q)!;
        void send({ type: "answers", answers });
      }}
    >
      <div className="flex items-start gap-2">
        <MessageCircleQuestion className="mt-0.5 size-4 shrink-0 text-accent" />
        <div className="min-w-0">
          {context && <div className="mb-0.5 truncate text-[11px] text-subtle">{context}</div>}
          <p className="text-[13px] font-medium">
            {questions.length === 1 ? "The agent has a question" : `The agent has ${questions.length} questions`}
          </p>
        </div>
      </div>
      {questions.map((q) => {
        const sel = picked[q.id] ?? [];
        return (
          <fieldset key={q.id} className="space-y-1.5">
            <legend className="mb-1.5 flex items-baseline gap-2">
              <span className="rounded border border-line px-1.5 py-px font-mono text-[10px] tracking-wide text-subtle uppercase">{q.header}</span>
              <span className="text-[13px]">{q.question}</span>
            </legend>
            <div role={q.multi ? "group" : "radiogroup"} aria-label={q.header} className="space-y-1">
              {q.options.map((o) => {
                const on = sel.includes(o.label);
                return (
                  <button
                    key={o.label}
                    type="button"
                    role={q.multi ? "checkbox" : "radio"}
                    aria-checked={on}
                    onClick={() => toggle(q, o.label)}
                    className={`flex w-full items-start gap-2 rounded-lg border px-2.5 py-1.5 text-left text-[12.5px] transition ${
                      on ? "border-accent/60 bg-accent/10 text-fg" : "border-line text-muted hover:border-line-2 hover:text-fg"
                    }`}
                  >
                    <span
                      aria-hidden
                      className={`mt-[3px] grid size-3 shrink-0 place-items-center border ${q.multi ? "rounded-[3px]" : "rounded-full"} ${
                        on ? "border-accent bg-accent" : "border-line-2"
                      }`}
                    />
                    <span>
                      {o.label}
                      {o.description && <span className="block text-[11.5px] text-subtle">{o.description}</span>}
                    </span>
                  </button>
                );
              })}
              {q.options.length > 0 ? (
                <button
                  type="button"
                  role={q.multi ? "checkbox" : "radio"}
                  aria-checked={sel.includes(OTHER)}
                  onClick={() => toggle(q, OTHER)}
                  className={`w-full rounded-lg border px-2.5 py-1.5 text-left text-[12.5px] ${
                    sel.includes(OTHER) ? "border-accent/60 bg-accent/10" : "border-line text-subtle hover:text-fg"
                  }`}
                >
                  Something else…
                </button>
              ) : null}
              {(q.options.length === 0 || sel.includes(OTHER)) && (
                <input
                  autoFocus={q.options.length > 0}
                  value={other[q.id] ?? ""}
                  onChange={(e) => setOther((o) => ({ ...o, [q.id]: e.target.value }))}
                  placeholder="Your answer"
                  aria-label={`${q.header}: your answer`}
                  className={`${input} w-full`}
                />
              )}
            </div>
          </fieldset>
        );
      })}
      <button type="submit" disabled={busy || !complete} className={primary}>
        {busy ? <span className="shimmer">Sending…</span> : "Answer"}
      </button>
      {error && (
        <p className="text-[12px] text-danger" role="alert">
          {error}
        </p>
      )}
    </form>
  );
}

function Resolved({ request }: { request: Request }) {
  const b = request.body;
  const title = b.type === "approval" ? b.title : b.questions.map((q) => q.header).join(", ");
  let outcome = "Cancelled";
  const r = request.response;
  if (request.status === "resolved" && r) {
    outcome =
      r.type === "decision"
        ? `${decidedLabel[r.decision]}${r.message ? `: “${r.message}”` : ""}`
        : Object.values(r.answers).join(" · ");
  }
  return (
    <p className="flex items-baseline gap-2 text-[12px] text-muted">
      <span className="truncate">{title}</span>
      <span className="text-subtle">→</span>
      <span className="truncate text-fg">{outcome}</span>
      {request.resolved_by && <span className="shrink-0 text-subtle">by {request.resolved_by}</span>}
    </p>
  );
}
