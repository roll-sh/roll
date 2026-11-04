/**
 * Requests, answered in place as T3 Code does (MIT): an approval is a row in the work log with
 * Approve and Decline beside it and the rest in a menu (T3's pending-approval row in
 * `TimelineWorkGroupRows.tsx`); questions are T3's user-input panel
 * (`ComposerPendingUserInputPanel.tsx`): one question at a time, its options as radio (or check)
 * rows with 1–9 to pick, a single choice moving on by itself after 200 ms, a custom answer typed
 * in (in the conversation that's the composer, which hosts the panel; on the canvas the panel has
 * its own line), and Previous / Next question / Submit. A resolved request is one line.
 */

import { Check, ChevronDown, FileDiff as FileIcon, ListChecks, MessageCircleQuestion, Shield, ShieldQuestion, SquareTerminal, Wrench } from "lucide-react";
import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import type { Approval, ApprovalAction, Decision, FileChange, Question, Request, RequestResponse } from "@/lib/agents-api";
import type { FileDiff } from "@/lib/code-api";
import { FileDiffView } from "../code/diff";
import { input } from "../ui";
import { Menu, MenuItem, MenuPopup, MenuTrigger } from "./ui";

/**
 * The hunks of a unified diff, without git's header lines (`diff --git`, `index`, `new file mode`,
 * `---`/`+++`): the diff viewer reads a patch from its first `@@`. `null` when there's no hunk
 * (a mode change, a binary file).
 */
export function hunks(diff: string | null): string | null {
  if (diff === null) return null;
  if (diff.startsWith("@@")) return diff;
  const at = diff.indexOf("\n@@");
  return at < 0 ? null : diff.slice(at + 1);
}

/** An agent's file change in the shape the code diff component takes. */
export function toFileDiff(f: FileChange): FileDiff {
  let additions = 0;
  let deletions = 0;
  const patch = hunks(f.diff);
  for (const l of (patch ?? "").split("\n")) {
    if (l.startsWith("+")) additions++;
    else if (l.startsWith("-")) deletions++;
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
    patch,
  };
}

const actionIcon: Record<ApprovalAction, typeof SquareTerminal> = {
  command: SquareTerminal,
  file_change: FileIcon,
  permission: ShieldQuestion,
  tool: Wrench,
  plan: ListChecks,
};

/** The buttons' words; a plan's are T3's plan-ready actions. */
function decisionLabel(d: Decision, plan: boolean): string {
  switch (d) {
    case "accept":
      return plan ? "Implement" : "Approve";
    case "accept_for_session":
      return "Approve for session";
    case "decline":
      return plan ? "Refine" : "Decline";
    case "cancel":
      return "Stop run";
  }
}

const decidedLabel: Record<Decision, string> = {
  accept: "Approved",
  accept_for_session: "Approved for the session",
  decline: "Declined",
  cancel: "Stopped the run",
};

/** T3's message-action pill (the composer's send and "Implement"), and its quiet sibling. */
export const pill =
  "inline-flex h-7 shrink-0 items-center justify-center gap-1.5 rounded-full bg-accent px-3.5 text-[12.5px] font-medium whitespace-nowrap text-white shadow-xs transition hover:brightness-110 active:scale-[0.97] disabled:pointer-events-none disabled:opacity-50 motion-reduce:transition-none";
export const quiet =
  "inline-flex h-7 shrink-0 items-center justify-center gap-1.5 rounded-full px-3 text-[12.5px] whitespace-nowrap text-muted transition hover:bg-fg/[0.06] hover:text-fg active:scale-[0.97] disabled:pointer-events-none disabled:opacity-50 motion-reduce:transition-none";

/**
 * A request, answered right here: in the transcript, on its card on the canvas, at the bottom of
 * a phone's canvas. Questions answered in the conversation go through the composer's panel
 * instead (`hostedQuestions`): the transcript then just says they're waiting below.
 */
export function RequestCard({
  request,
  onRespond,
  context,
  compact = false,
  hostedQuestions = false,
}: {
  request: Request;
  /** Unset: read-only (a share's replay), the request shows what it asked and that it waits. */
  onRespond?: (r: RequestResponse) => Promise<unknown>;
  /** A line above the title (the thread's name, on the canvas). */
  context?: ReactNode;
  compact?: boolean;
  /** The composer below answers the questions. */
  hostedQuestions?: boolean;
}) {
  if (request.status !== "pending") return <Resolved request={request} />;
  if (!onRespond) return <Waiting request={request} />;
  if (request.body.type === "approval") return <ApprovalForm approval={request.body} thread={request.thread} onRespond={onRespond} context={context} compact={compact} />;
  if (hostedQuestions) return <AskedBelow count={request.body.questions.length} />;
  return <StandaloneQuestions questions={request.body.questions} thread={request.thread} onRespond={onRespond} context={context} />;
}

/** The questions are in the composer's panel: say so where they were asked. */
function AskedBelow({ count }: { count: number }) {
  return (
    <p className="flex min-h-6 items-center gap-1.5 px-0.5 text-[13px] text-muted">
      <span className="grid w-6 place-items-center">
        <MessageCircleQuestion className="size-4 text-accent" strokeWidth={1.5} />
      </span>
      {count === 1 ? "Asked a question" : `Asked ${count} questions`}
      <span className="text-subtle">· answer below</span>
    </p>
  );
}

/** A pending request nobody here can answer: what it asks, and that it waits. */
function Waiting({ request }: { request: Request }) {
  const b = request.body;
  const Icon = b.type === "approval" ? actionIcon[b.action] : MessageCircleQuestion;
  return (
    <div className="space-y-1.5 px-0.5" role="group" aria-label="Waiting for an answer">
      <p className="flex min-h-6 items-start gap-1.5 text-[13px]">
        <span className="grid h-6 w-6 shrink-0 place-items-center">
          <Icon className="size-4 text-accent" strokeWidth={1.5} />
        </span>
        <span className="min-w-0 flex-1 pt-0.5 leading-snug font-medium">{b.type === "approval" ? b.title : b.questions.map((q) => q.question).join(" · ")}</span>
      </p>
      {b.type === "approval" && b.command && <Command text={b.command} />}
      <p className="ms-7 text-[12px] text-subtle">
        <span className="shimmer">Waiting for an answer…</span>
      </p>
    </div>
  );
}

function Command({ text }: { text: string }) {
  return (
    <pre className="ms-7 max-h-40 overflow-auto rounded-lg border border-line bg-bg px-2.5 py-2 font-mono text-[11.5px] leading-relaxed whitespace-pre-wrap" data-scroll>
      <span className="text-subtle select-none">$ </span>
      {text}
    </pre>
  );
}

/**
 * Where the keyboard goes once an answered request's controls are gone (focus fell to the page):
 * in a conversation its composer (T3's), on the canvas the thread's own card.
 */
function afterAnswer(from: Element | null, thread: string): (() => HTMLElement | null) | null {
  const region = from?.closest('[role="region"][aria-label^="Thread "]');
  if (region) return () => region.querySelector<HTMLElement>('textarea[aria-label="Message"]');
  if (from?.closest("[data-card]")) return () => document.querySelector<HTMLElement>(`[data-card][data-key="t:${CSS.escape(thread)}"]`);
  return null;
}

function useSend(onRespond: (r: RequestResponse) => Promise<unknown>, thread: string) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const send = async (r: RequestResponse) => {
    setBusy(true);
    setError(null);
    const from = document.activeElement;
    const next = afterAnswer(from, thread);
    try {
      await onRespond(r);
      if (next)
        requestAnimationFrame(() => {
          const now = document.activeElement;
          if (!now || now === document.body || !from?.isConnected) next()?.focus({ preventScroll: true });
        });
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setBusy(false);
    }
  };
  return { busy, error, send };
}

function ApprovalForm({
  approval,
  thread,
  onRespond,
  context,
  compact,
}: {
  approval: Approval;
  thread: string;
  onRespond: (r: RequestResponse) => Promise<unknown>;
  context?: ReactNode;
  compact: boolean;
}) {
  const { busy, error, send } = useSend(onRespond, thread);
  const declineButton = useRef<HTMLButtonElement>(null);
  // Back from the reason (Escape or Back): to the Decline that opened it, not the page.
  const stopDeclining = () => {
    setDeclining(false);
    requestAnimationFrame(() => declineButton.current?.focus({ preventScroll: true }));
  };
  const [declining, setDeclining] = useState(false);
  const [note, setNote] = useState("");
  const [openFile, setOpenFile] = useState<string | null>(null);
  const plan = approval.action === "plan";
  const Icon = actionIcon[approval.action];
  const has = (d: Decision) => approval.decisions.includes(d);
  const decide = (decision: Decision, message?: string) => send({ type: "decision", decision, ...(message ? { message } : {}) });
  const more = (["accept_for_session", "cancel"] as const).filter(has);

  return (
    <div className="space-y-2 px-0.5" role="group" aria-label={`Approval: ${approval.title}`}>
      {context && <div className="ms-7 min-w-0 truncate text-[11.5px] text-subtle">{context}</div>}
      <p className="flex min-h-6 items-start gap-1.5 text-[13px]">
        <span className="grid h-6 w-6 shrink-0 place-items-center">
          <Icon className="size-4 text-accent" strokeWidth={1.5} />
        </span>
        <span className="min-w-0 flex-1 pt-0.5 leading-snug font-medium">
          {/* What it asks is the request card's title: it reads, then is a bar, as a thread's does. */}
          <span data-title data-bar>
            {approval.title}
          </span>
        </span>
      </p>
      {approval.command && <Command text={approval.command} />}
      <div className="ms-7 space-y-2 text-[12px]">
        {approval.reason && <p className="text-muted">{approval.reason}</p>}
        {approval.cwd && !compact && <p className="truncate font-mono text-[11px] text-subtle">in {approval.cwd}</p>}
        {approval.files.length > 0 && (
          <ul className="space-y-0.5">
            {approval.files.map((f) => (
              <li key={f.path}>
                <button
                  type="button"
                  onClick={() => setOpenFile((o) => (o === f.path ? null : f.path))}
                  className="flex w-full items-center gap-2 truncate rounded-md px-1 py-0.5 text-left font-mono text-[11.5px] text-muted hover:bg-fg/[0.04] hover:text-fg"
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
        {/* On a canvas card its frame and its thread's pill say it waits. */}
        {!compact && (
          <p className="flex items-center gap-1 text-accent">
            <Shield className="size-3" /> {plan ? "Plan ready" : "Waiting for approval"}
          </p>
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
                e.stopPropagation();
                if (e.key === "Enter" && !e.shiftKey) {
                  e.preventDefault();
                  void decide("decline", note.trim() || undefined);
                }
                if (e.key === "Escape") stopDeclining();
              }}
              placeholder={plan ? "What should change in the plan?" : "Tell the agent what to do instead (optional)"}
              aria-label="Note to the agent"
              className={`${input} w-full resize-none text-[13px]`}
            />
            <div className="flex items-center gap-1.5">
              <button type="submit" disabled={busy} className={pill}>
                {busy ? <span className="shimmer">Sending…</span> : decisionLabel("decline", plan)}
              </button>
              <button type="button" onClick={stopDeclining} className={quiet}>
                Back
              </button>
            </div>
          </form>
        ) : (
          <div className="flex items-center gap-1">
            {has("accept") && (
              <button type="button" data-answer disabled={busy} onClick={() => void decide("accept")} className={pill}>
                {busy ? <span className="shimmer">Sending…</span> : decisionLabel("accept", plan)}
              </button>
            )}
            {has("decline") && (
              <button ref={declineButton} type="button" disabled={busy} onClick={() => setDeclining(true)} className={quiet}>
                {decisionLabel("decline", plan)}
              </button>
            )}
            {more.length > 0 && (
              <Menu>
                <MenuTrigger
                  render={
                    <button type="button" disabled={busy} aria-label="More options" title="More options" className={`${quiet} ml-auto px-2`}>
                      <ChevronDown className="size-3.5" />
                    </button>
                  }
                />
                <MenuPopup align="end" className="w-52">
                  {more.map((d) => (
                    <MenuItem key={d} onClick={() => void decide(d)} variant={d === "cancel" ? "destructive" : "default"}>
                      {decisionLabel(d, plan)}
                    </MenuItem>
                  ))}
                </MenuPopup>
              </Menu>
            )}
          </div>
        )}
        {error && (
          <p className="text-danger" role="alert">
            {error}
          </p>
        )}
      </div>
    </div>
  );
}

/**
 * Where a set of questions is (T3's pending user input progress): the one shown, what's picked
 * for each, what was typed for each.
 */
export interface QuestionFlow {
  questions: Question[];
  index: number;
  question: Question;
  last: boolean;
  picked: (q: Question) => string[];
  custom: (q: Question) => string;
  setCustom: (text: string) => void;
  /** Pick (or for a multi question, toggle) an option of the shown question; a single pick moves on. */
  pick: (label: string) => void;
  /** The shown question has an answer. */
  answered: boolean;
  complete: boolean;
  next: () => void;
  back: () => void;
  answers: () => Record<string, string>;
}

/** The answer to one question: the options picked, then (or, for one choice, instead) what was typed. */
function answerOf(q: Question, picked: string[], custom: string): string | null {
  const typed = custom.trim();
  if (!q.multi && typed) return typed;
  const all = typed ? [...picked, typed] : picked;
  return all.length ? all.join(", ") : null;
}

const NO_QUESTION: Question = { id: "", header: "", question: "", options: [], multi: false };

/**
 * The flow through a request's questions. `key` names the request: another one starts over. With
 * no questions it's inert (the composer holds one whether or not anything is asked).
 */
export function useQuestionFlow(questions: Question[], key = ""): QuestionFlow {
  const [index, setIndex] = useState(0);
  const [picked, setPicked] = useState<Record<string, string[]>>({});
  const [typed, setTyped] = useState<Record<string, string>>({});
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(() => () => void (timer.current && clearTimeout(timer.current)), []);
  useEffect(() => {
    setIndex(0);
    setPicked({});
    setTyped({});
  }, [key]);
  const i = Math.max(0, Math.min(index, questions.length - 1));
  const q = questions[i] ?? NO_QUESTION;
  const last = i === questions.length - 1;
  const of = (x: Question) => answerOf(x, picked[x.id] ?? [], typed[x.id] ?? "");
  return {
    questions,
    index: i,
    question: q,
    last,
    picked: (x) => picked[x.id] ?? [],
    custom: (x) => typed[x.id] ?? "",
    setCustom: (text) => setTyped((t) => ({ ...t, [q.id]: text })),
    pick: (label) => {
      if (q.multi) {
        setPicked((p) => {
          const cur = p[q.id] ?? [];
          return { ...p, [q.id]: cur.includes(label) ? cur.filter((x) => x !== label) : [...cur, label] };
        });
        return;
      }
      setPicked((p) => ({ ...p, [q.id]: [label] }));
      // One choice made and nothing typed: on to the next question, as T3 does.
      if (!last && !(typed[q.id] ?? "").trim()) {
        if (timer.current) clearTimeout(timer.current);
        timer.current = setTimeout(() => setIndex((n) => Math.min(n + 1, questions.length - 1)), 200);
      }
    },
    answered: of(q) !== null,
    complete: questions.every((x) => of(x) !== null),
    next: () => setIndex((n) => Math.min(n + 1, questions.length - 1)),
    back: () => setIndex((n) => Math.max(n - 1, 0)),
    answers: () => Object.fromEntries(questions.map((x) => [x.id, of(x) ?? ""])),
  };
}

/** Digits 1–9 pick the shown question's options (T3's shortcut), unless typing somewhere. */
export function pickByDigit(flow: QuestionFlow, e: React.KeyboardEvent, typing: boolean): boolean {
  if (typing || e.metaKey || e.ctrlKey || e.altKey || !/^[1-9]$/.test(e.key)) return false;
  const o = flow.question.options[Number(e.key) - 1];
  if (!o) return false;
  e.preventDefault();
  e.stopPropagation();
  flow.pick(o.label);
  return true;
}

/**
 * T3's user-input panel: the shown question with its header and where it is ("1/2"), its options
 * as rows with a radio (a check for several) and the digit that picks it. `children` go at the
 * bottom (the canvas's own answer line and buttons).
 */
export function QuestionsPanel({ flow, context, children }: { flow: QuestionFlow; context?: ReactNode; children?: ReactNode }) {
  const q = flow.question;
  const sel = flow.picked(q);
  // Moving on unmounts the option that had focus: give it to the new question's (keyboard users
  // keep going with arrows and Enter). Only when an option had it: never on load, and never when
  // focus went somewhere else on purpose (a blur with somewhere to go; an unmounted option's has none).
  const options = useRef<HTMLDivElement>(null);
  const hadFocus = useRef(false);
  useLayoutEffect(() => {
    const el = options.current;
    const active = document.activeElement;
    if (!el || !hadFocus.current || (active && active !== document.body)) return;
    (el.querySelector<HTMLElement>('[aria-checked="true"]') ?? el.querySelector<HTMLElement>("button"))?.focus();
  }, [q.id]);
  return (
    <div role="group" aria-label="Questions from the agent" className="min-w-0">
      {context && <div className="mb-1 min-w-0 truncate text-[11.5px] text-subtle">{context}</div>}
      <div className="flex items-center gap-2 text-[12px] text-muted">
        <MessageCircleQuestion className="size-3.5 shrink-0 text-accent" />
        <span className="min-w-0 flex-1 truncate">
          <span data-line>{q.header}</span>
        </span>
        {flow.questions.length > 1 && (
          <span className="shrink-0 font-mono text-[11px] text-subtle tabular-nums">
            {flow.index + 1}/{flow.questions.length}
          </span>
        )}
      </div>
      <p className="mt-1.5 text-[13.5px] leading-snug text-fg/85">
        <span data-title data-bar>
          {q.question}
        </span>
      </p>
      <div
        ref={options}
        role={q.multi ? "group" : "radiogroup"}
        aria-label={q.header}
        onFocus={() => (hadFocus.current = true)}
        onBlur={(e) => {
          if (e.relatedTarget && !e.currentTarget.contains(e.relatedTarget as Node)) hadFocus.current = false;
        }}
        className="mt-1.5 -mx-1.5 flex flex-col"
      >
        {q.options.map((o, k) => {
          const on = sel.includes(o.label);
          return (
            <button
              key={o.label}
              type="button"
              role={q.multi ? "checkbox" : "radio"}
              aria-checked={on}
              onClick={() => flow.pick(o.label)}
              className={`flex items-start gap-2.5 rounded-md py-1.5 ps-1.5 pe-1.5 text-left transition-colors outline-hidden focus-visible:outline-2 focus-visible:outline-accent/70 ${on ? "text-fg" : "text-fg/70 hover:bg-fg/[0.04] hover:text-fg"}`}
            >
              <span
                aria-hidden
                className={`mt-0.5 grid size-4 shrink-0 place-items-center border transition-colors ${q.multi ? "rounded-[4px]" : "rounded-full"} ${on ? "border-accent bg-accent" : "border-line-2"}`}
              >
                {on && (q.multi ? <Check className="size-3 text-white" strokeWidth={2.5} /> : <span className="size-1.5 rounded-full bg-white" />)}
              </span>
              <span className="min-w-0 flex-1">
                <span data-line className="block text-[13px] font-medium">
                  {o.label}
                </span>
                {o.description && <span className="block text-[11.5px] text-muted">{o.description}</span>}
              </span>
              {k < 9 && <kbd className="grid h-5 min-w-5 shrink-0 place-items-center rounded bg-fg/[0.07] px-1 font-mono text-[11px] text-muted">{k + 1}</kbd>}
            </button>
          );
        })}
      </div>
      {children}
    </div>
  );
}

/** Questions with nothing below to type in (the canvas): the panel with its own answer line and buttons. */
function StandaloneQuestions({
  questions,
  thread,
  onRespond,
  context,
}: {
  questions: Question[];
  thread: string;
  onRespond: (r: RequestResponse) => Promise<unknown>;
  context?: ReactNode;
}) {
  const flow = useQuestionFlow(questions);
  const { busy, error, send } = useSend(onRespond, thread);
  const q = flow.question;
  const submit = () => {
    if (!flow.last) return flow.answered && flow.next();
    if (flow.complete) void send({ type: "answers", answers: flow.answers() });
  };
  return (
    <form
      aria-label="Answer the agent"
      onSubmit={(e) => {
        e.preventDefault();
        submit();
      }}
      onKeyDown={(e) => pickByDigit(flow, e, (e.target as HTMLElement).tagName === "INPUT")}
    >
      <QuestionsPanel flow={flow} context={context}>
        <input
          key={q.id}
          value={flow.custom(q)}
          onChange={(e) => flow.setCustom(e.target.value)}
          onKeyDown={(e) => e.stopPropagation()}
          placeholder={q.options.length ? "Or type your own answer" : "Your answer"}
          aria-label={`${q.header}: your answer`}
          className={`${input} mt-1.5 w-full py-1 text-[13px]`}
        />
        <div className="mt-2.5 flex items-center gap-1.5">
          {flow.index > 0 && (
            <button type="button" onClick={flow.back} className={quiet}>
              Previous
            </button>
          )}
          <button type="submit" disabled={busy || (flow.last ? !flow.complete : !flow.answered)} className={`${pill} ml-auto`}>
            {busy ? <span className="shimmer">Sending…</span> : flow.last ? (questions.length === 1 ? "Submit answer" : "Submit answers") : "Next question"}
          </button>
        </div>
        {error && (
          <p className="mt-2 text-[12px] text-danger" role="alert">
            {error}
          </p>
        )}
      </QuestionsPanel>
    </form>
  );
}

function Resolved({ request }: { request: Request }) {
  const b = request.body;
  const title = b.type === "approval" ? b.title : b.questions.map((q) => q.header).join(", ");
  let outcome = "Cancelled";
  const r = request.response;
  if (request.status === "resolved" && r) {
    outcome = r.type === "decision" ? `${decidedLabel[r.decision]}${r.message ? `: “${r.message}”` : ""}` : Object.values(r.answers).join(" · ");
  }
  const Icon = b.type === "approval" ? actionIcon[b.action] : MessageCircleQuestion;
  return (
    <p className="flex min-h-6 min-w-0 items-center gap-1.5 px-0.5 text-[13px] text-muted">
      <span className="grid w-6 shrink-0 place-items-center">
        <Icon className="size-4 opacity-70" strokeWidth={1.5} />
      </span>
      <span className="min-w-0 truncate">{title}</span>
      <span className="text-subtle">→</span>
      <span className="min-w-0 truncate text-fg">{outcome}</span>
      {request.resolved_by && <span className="shrink-0 text-[12px] text-subtle">by {request.resolved_by}</span>}
    </p>
  );
}
