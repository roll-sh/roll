"use client";

import { HorizonMark } from "@roll/brand";
import { AtSign, Check, ChevronRight, CornerDownRight, Wand2 } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import type { Reviewer, ReviewComment, ReviewSource, Thread } from "@/lib/code-api";
import { applyState, reviewerBadges, reviewerName, stripSuggestion, suggestionDiff, type MiniLine } from "@/lib/review";
import { findingOf } from "@/lib/ai-review";
import { ago } from "@/lib/time";
import { Monogram } from "../repo/avatar";
import { btn, input } from "../ui";
import { Confidence, SeverityChip } from "./ai";
import { GitHubMark } from "./shared";
import { Markdown } from "./markdown";

/** What a thread can do on the dashboard. Without it (public pages) threads are read-only. */
export interface ThreadActions {
  writer: boolean;
  /** The change is open (not landed or closed). */
  open: boolean;
  /** A key of the action running right now, if any. */
  busy: string | null;
  /** Resolves to `undefined` when it failed. */
  reply: (thread: string, body: string) => Promise<unknown>;
  setResolved: (thread: string, resolved: boolean) => Promise<unknown>;
  apply: (thread: string, comment: string) => Promise<unknown>;
  /** The thread whose reply box should open (the `r` key). */
  replying?: string | null;
  onReplyDone?: () => void;
}

/** "3m ago", rendered on the server and the client alike (seconds apart, so hydration may differ). */
export function Ago({ ts, className = "text-[11px] text-subtle" }: { ts: number; className?: string }) {
  return (
    <time dateTime={new Date(ts * 1000).toISOString()} className={className} suppressHydrationWarning>
      {ago(ts, Date.now() / 1000)}
    </time>
  );
}

/** A reviewer's avatar: roll's reviewer is the Horizon mark; apps get a squared monogram. */
export function ReviewerAvatar({ reviewer, size = 20 }: { reviewer: Pick<Reviewer, "kind" | "name">; size?: number }) {
  if (reviewer.kind === "roll") {
    return (
      <span className="grid shrink-0 place-items-center rounded-full border border-line bg-panel-2" style={{ width: size, height: size }}>
        <HorizonMark size={Math.round(size * 0.7)} interactive={false} />
      </span>
    );
  }
  return <Monogram name={reviewerName(reviewer)} size={size} org={reviewer.kind === "app"} />;
}

/** `coderabbitai` · app · github: a name with quiet kind badges. */
export function ReviewerName({ reviewer, source }: { reviewer: Reviewer; source: ReviewSource }) {
  return (
    <span className="inline-flex min-w-0 items-center gap-1.5">
      <span className="truncate text-[13px] text-fg">{reviewer.kind === "roll" ? "roll" : reviewerName(reviewer)}</span>
      {reviewerBadges(reviewer, source).map((b) =>
        b === "github" ? (
          <span key={b} className="inline-flex items-center rounded border border-line px-1 py-px text-subtle" title="from GitHub">
            <GitHubMark className="size-2.5" />
          </span>
        ) : b === "roll" ? null : (
          <span key={b} className="rounded border border-line px-1 py-px font-mono text-[10px] text-subtle">
            {b}
          </span>
        ),
      )}
    </span>
  );
}

const miniCls: Record<MiniLine["kind"], string> = { ctx: "", del: "bg-danger/10", add: "bg-ok/10" };
const miniSign: Record<MiniLine["kind"], string> = { ctx: " ", del: "−", add: "+" };

/** A suggestion as a small diff of the lines it replaces. */
export function SuggestionView({ original, suggestion }: { original: string[] | null; suggestion: string }) {
  const rows = suggestionDiff(original, suggestion);
  return (
    <div className="overflow-hidden rounded-lg border border-line">
      <p className="flex items-center gap-1.5 border-b border-line bg-panel-2 px-3 py-1.5 text-[11px] text-subtle">
        <Wand2 className="size-3" /> Suggested change
      </p>
      {rows.length === 0 ? (
        <p className="px-3 py-2 text-[12px] text-subtle">Deletes the selected lines.</p>
      ) : (
        <pre className="overflow-x-auto font-mono text-[12px] leading-5">
          {rows.map((r, i) => (
            <div key={i} className={`px-3 whitespace-pre ${miniCls[r.kind]}`}>
              <span className={`mr-2 select-none ${r.kind === "add" ? "text-ok" : r.kind === "del" ? "text-danger" : "text-subtle"}`}>
                {miniSign[r.kind]}
              </span>
              {r.text}
            </div>
          ))}
        </pre>
      )}
    </div>
  );
}

function CommentView({
  comment,
  thread,
  original,
  actions,
}: {
  comment: ReviewComment;
  thread: Thread;
  original: string[] | null;
  actions?: ThreadActions;
}) {
  const raw = comment.suggestion != null ? stripSuggestion(comment.body) : comment.body;
  // roll's findings carry `finding` (and lead with `**severity** · title` in the text).
  const finding = comment.author.kind === "roll" ? findingOf({ body: raw, finding: comment.finding }) : null;
  const body = finding?.severity ? finding.body : raw;
  const state = applyState({ thread, suggestion: comment.suggestion, writer: !!actions?.writer, open: !!actions?.open });
  const key = `apply-${comment.id}`;
  return (
    <div className="flex gap-2.5">
      <ReviewerAvatar reviewer={comment.author} />
      <div className="min-w-0 flex-1 space-y-2">
        <p className="flex items-center gap-2">
          <ReviewerName reviewer={comment.author} source={comment.source} />
          <Ago ts={comment.created_at} />
        </p>
        {finding?.severity && (
          <p className="flex flex-wrap items-center gap-2">
            <SeverityChip severity={finding.severity} />
            <span className="text-[13px] font-medium text-fg">{finding.title}</span>
            {finding.confidence != null && <Confidence value={finding.confidence} />}
          </p>
        )}
        <Markdown source={body} />
        {comment.suggestion != null && (
          <div className="space-y-2">
            <SuggestionView original={original} suggestion={comment.suggestion} />
            {actions && (state.can || state.reason) && (
              <div className="flex items-center gap-2">
                {state.can && (
                  <button onClick={() => actions.apply(thread.id, comment.id)} disabled={actions.busy !== null} className={btn}>
                    {actions.busy === key ? <span className="shimmer">Applying…</span> : "Apply suggestion"}
                  </button>
                )}
                {state.reason && <span className="text-[11px] text-subtle">{state.reason}</span>}
              </div>
            )}
          </div>
        )}
      </div>
    </div>
  );
}

/**
 * One thread: its comments, suggestions as mini-diffs, a reply box and resolve. Resolved threads
 * fold to one line. `original` is the text of the lines it's on (for suggestion diffs).
 */
export function ThreadCard({
  thread,
  original = null,
  actions,
  focused = false,
  location = false,
}: {
  thread: Thread;
  original?: string[] | null;
  actions?: ThreadActions;
  focused?: boolean;
  /** Show the path and lines (threads drawn below a file rather than at their line). */
  location?: boolean;
}) {
  const [expanded, setExpanded] = useState(!thread.resolved);
  const [replyOpen, setReplyOpen] = useState(false);
  const [reply, setReply] = useState("");
  const replyRef = useRef<HTMLTextAreaElement>(null);
  const ref = useRef<HTMLDivElement>(null);
  const first = thread.comments[0];
  const where = thread.start_line < thread.line ? `L${thread.start_line}–L${thread.line}` : `L${thread.line}`;

  useEffect(() => {
    if (focused) {
      setExpanded(true);
      ref.current?.scrollIntoView({ block: "center", behavior: window.matchMedia("(prefers-reduced-motion: reduce)").matches ? "auto" : "smooth" });
    }
  }, [focused]);

  // `r` opens the reply box and focuses it. The box may not be mounted yet (the thread was
  // collapsed or the box closed), so the focus waits for the commit that mounts it.
  const focusReply = useRef(false);
  useEffect(() => {
    if (actions?.replying === thread.id) {
      setExpanded(true);
      setReplyOpen(true);
      focusReply.current = true;
    }
  }, [actions?.replying, thread.id]);
  useEffect(() => {
    if (focusReply.current && replyRef.current) {
      focusReply.current = false;
      replyRef.current.focus();
    }
  });

  const send = async () => {
    if (!actions || !reply.trim()) return;
    // `undefined` means it failed (the page shows why); keep what was written.
    if ((await actions.reply(thread.id, reply.trim())) === undefined) return;
    setReply("");
    setReplyOpen(false);
    actions.onReplyDone?.();
  };

  return (
    <div
      ref={ref}
      id={`thread-${thread.id}`}
      className={`scroll-mt-24 rounded-lg border bg-panel font-sans transition-colors duration-200 ${
        focused ? "border-accent/60" : "border-line"
      }`}
    >
      {!expanded ? (
        <button
          type="button"
          onClick={() => setExpanded(true)}
          className="flex w-full items-center gap-2 px-3 py-2 text-left text-[12px] text-subtle hover:text-fg"
        >
          <ChevronRight className="size-3.5" />
          <Check className="size-3.5 text-ok" />
          <span className="truncate">
            Resolved{thread.resolved_by ? ` by ${thread.resolved_by}` : ""} · {first ? reviewerName(first.author) : ""}:{" "}
            {first ? stripSuggestion(first.body).split("\n")[0] : ""}
          </span>
          <span className="ml-auto shrink-0 font-mono text-[11px]">{thread.comments.length}</span>
        </button>
      ) : (
        <>
          {(location || thread.outdated || thread.resolved) && (
            <div className="flex items-center gap-2 border-b border-line px-3 py-1.5 text-[11px] text-subtle">
              {location && (
                <span className="truncate font-mono">
                  {thread.path} · {where}
                  {thread.side === "old" && " (removed)"}
                </span>
              )}
              {thread.outdated && <span className="rounded-full border border-line px-1.5 py-px">outdated</span>}
              {thread.resolved && (
                <button type="button" onClick={() => setExpanded(false)} className="ml-auto inline-flex items-center gap-1 hover:text-fg">
                  <Check className="size-3 text-ok" /> resolved
                </button>
              )}
            </div>
          )}
          <div className="space-y-4 px-3 py-3">
            {thread.comments.map((c) => (
              <CommentView key={c.id} comment={c} thread={thread} original={original} actions={actions} />
            ))}
          </div>
          {actions && (
            <div className="flex flex-wrap items-center gap-2 border-t border-line px-3 py-2">
              {replyOpen ? (
                <form
                  className="w-full space-y-2"
                  onSubmit={(e) => {
                    e.preventDefault();
                    send();
                  }}
                >
                  <textarea
                    ref={replyRef}
                    value={reply}
                    onChange={(e) => setReply(e.target.value)}
                    onKeyDown={(e) => {
                      if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
                        e.preventDefault();
                        send();
                      } else if (e.key === "Escape") {
                        setReplyOpen(false);
                        actions.onReplyDone?.();
                      }
                    }}
                    rows={3}
                    placeholder="Reply… (markdown; ⌘↵ to send)"
                    className={`${input} w-full resize-y font-sans`}
                  />
                  <div className="flex items-center gap-2">
                    <button type="submit" disabled={!reply.trim() || actions.busy !== null} className={btn}>
                      {actions.busy === `reply-${thread.id}` ? <span className="shimmer">Sending…</span> : "Reply"}
                    </button>
                    {actions.writer && !/(^|\s)@roll\b/.test(reply) && (
                      <button
                        type="button"
                        onClick={() => {
                          setReply((r) => (r.trim() ? `@roll ${r.replace(/^\s+/, "")}` : "@roll "));
                          requestAnimationFrame(() => {
                            const el = replyRef.current;
                            if (el) {
                              el.focus();
                              el.setSelectionRange(el.value.length, el.value.length);
                            }
                          });
                        }}
                        className={btn}
                        title="Ask roll about this thread (writers): a question, or explain, fix, resolve, don't flag …"
                      >
                        <AtSign className="size-3.5" /> Ask roll
                      </button>
                    )}
                    <button
                      type="button"
                      onClick={() => {
                        setReplyOpen(false);
                        actions.onReplyDone?.();
                      }}
                      className="text-[12px] text-subtle hover:text-fg"
                    >
                      Cancel
                    </button>
                  </div>
                </form>
              ) : (
                <button
                  type="button"
                  onClick={() => setReplyOpen(true)}
                  className="inline-flex flex-1 items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-left text-[12px] text-subtle transition hover:border-line-2 hover:text-fg"
                >
                  <CornerDownRight className="size-3.5" /> Reply…
                </button>
              )}
              {!replyOpen && (actions.writer || actions.open) && (
                <button
                  type="button"
                  onClick={() => actions.setResolved(thread.id, !thread.resolved)}
                  disabled={actions.busy !== null}
                  className={btn}
                >
                  {actions.busy === `resolve-${thread.id}` ? (
                    <span className="shimmer">Saving…</span>
                  ) : thread.resolved ? (
                    "Unresolve"
                  ) : (
                    <>
                      <Check className="size-3.5" /> Resolve
                    </>
                  )}
                </button>
              )}
            </div>
          )}
        </>
      )}
    </div>
  );
}
