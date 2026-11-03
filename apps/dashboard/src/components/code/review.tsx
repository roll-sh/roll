"use client";

import { CircleCheck, CircleDashed, CircleX, Eye, MessageSquare, Pencil, Trash2, UserPlus } from "lucide-react";
import { forwardRef, useCallback, useEffect, useState } from "react";
import {
  codeApi,
  type Change,
  type Review,
  type ReviewRequest,
  type ReviewStatus,
  type ReviewSummary,
  type Thread,
  type Verdict,
} from "@/lib/code-api";
import {
  approvalNotes,
  canSubmit,
  codeownerRows,
  emptyPending,
  loadPending,
  reviewerName,
  reviewHeadline,
  savePending,
  selectionLabel,
  timeline,
  verdictLabel,
  type DraftComment,
  type LineSelection,
  type PendingReview,
  type Tone,
} from "@/lib/review";
import { stripAiHeader } from "@/lib/ai-review";
import { btn, input, primary } from "../ui";
import { AiReviewHeader } from "./ai";
import { Markdown } from "./markdown";
import { Ago, ReviewerAvatar, ReviewerName, SuggestionView } from "./thread";

const toneCls: Record<Tone, string> = { ok: "text-ok", danger: "text-danger", accent: "text-accent", muted: "text-muted" };

// ── data ──

/** A change's reviews, threads, review status and requests; `reload` after any change or event. */
export function useChangeReview(change: Pick<Change, "id" | "head" | "revision" | "updated_at"> | null) {
  const [reviews, setReviews] = useState<Review[] | null>(null);
  const [threads, setThreads] = useState<Thread[] | null>(null);
  const [status, setStatus] = useState<ReviewStatus | null>(null);
  const [requests, setRequests] = useState<ReviewRequest[]>([]);
  const [error, setError] = useState<string | null>(null);
  const id = change?.id;

  const reload = useCallback(async () => {
    if (!id) return;
    const [r, t, s, q] = await Promise.allSettled([
      codeApi.reviews(id),
      codeApi.threads(id),
      codeApi.reviewStatus(id),
      codeApi.reviewRequests(id),
    ]);
    if (r.status === "fulfilled") setReviews(r.value);
    if (t.status === "fulfilled") setThreads(t.value);
    if (s.status === "fulfilled") setStatus(s.value);
    if (q.status === "fulfilled") setRequests(q.value);
    const failed = [r, t].find((x) => x.status === "rejected") as PromiseRejectedResult | undefined;
    setError(failed ? (failed.reason instanceof Error ? failed.reason.message : String(failed.reason)) : null);
  }, [id]);

  useEffect(() => {
    reload();
  }, [reload, change?.head, change?.revision, change?.updated_at]);

  return { reviews, threads, status, requests, error, reload };
}

/**
 * The review being written, kept in local storage per change until it's submitted. Compare
 * `pending.revision` with the change's to tell whether its comments were written on an older one.
 */
export function usePendingReview(change: Pick<Change, "id" | "revision"> | null) {
  const [pending, setPendingRaw] = useState<PendingReview>(() => emptyPending(change?.id ?? "", change?.revision ?? 0));
  const id = change?.id;
  const revision = change?.revision ?? 0;

  useEffect(() => {
    if (!id) return;
    setPendingRaw(loadPending(localStorage, id, revision).pending);
    // Only when the change is first known: a new revision keeps what's being written.
  }, [id]); // eslint-disable-line react-hooks/exhaustive-deps

  const setPending = useCallback((next: PendingReview | ((p: PendingReview) => PendingReview)) => {
    setPendingRaw((p) => {
      const v = typeof next === "function" ? next(p) : next;
      savePending(localStorage, v);
      return v;
    });
  }, []);

  const discard = useCallback(() => {
    if (!id) return;
    const empty = emptyPending(id, revision);
    savePending(localStorage, empty);
    setPendingRaw(empty);
  }, [id, revision]);

  return { pending, setPending, discard };
}

// ── status ──

/** State, approvals, code owners and what's owed, for the dashboard card and the public aside. */
export function ReviewStatusView({
  summary,
  status,
  requests = [],
}: {
  summary: ReviewSummary | undefined;
  status: ReviewStatus | null;
  requests?: ReviewRequest[];
}) {
  const s = status ?? summary;
  if (!s) return <p className="text-[12px] text-subtle">No review information.</p>;
  const head = reviewHeadline({ state: s.state, approvals: status ? status.approvals.filter((a) => a.counted).length : (summary?.approvals ?? 0), required: s.required });
  const owners = status ? codeownerRows(status) : [];
  const pending = requests.filter((r) => r.current && r.done_at == null);
  return (
    <div className="space-y-3 text-[13px]">
      <div>
        <p className={`flex items-center gap-1.5 ${toneCls[head.tone]}`}>
          {head.tone === "ok" ? (
            <CircleCheck className="size-3.5" />
          ) : head.tone === "danger" ? (
            <CircleX className="size-3.5" />
          ) : (
            <CircleDashed className="size-3.5" />
          )}
          {head.label}
        </p>
        <p className="mt-0.5 text-[12px] text-subtle">{head.detail}</p>
        {s.required > 0 && (
          <div className="mt-2 flex gap-1" aria-hidden>
            {Array.from({ length: Math.min(s.required, 10) }, (_, i) => (
              <span
                key={i}
                className={`h-1 flex-1 rounded-full ${i < (status ? status.approvals.filter((a) => a.counted).length : (summary?.approvals ?? 0)) ? "bg-ok" : "bg-line"}`}
              />
            ))}
          </div>
        )}
      </div>
      {status?.auto_approved && (
        <p className="text-[12px] text-muted">
          Auto-approved by <span className="font-mono text-fg">{status.auto_approved.rule}</span>
        </p>
      )}
      {owners.length > 0 && (
        <div>
          <p className="text-[11px] text-subtle">Code owners</p>
          <ul className="mt-1 space-y-1">
            {owners.map((o) => (
              <li key={o.pattern} className="flex items-start gap-2 text-[12px]">
                {o.satisfied ? (
                  <CircleCheck className="mt-0.5 size-3 shrink-0 text-ok" />
                ) : (
                  <CircleDashed className="mt-0.5 size-3 shrink-0 text-subtle" />
                )}
                <span className="min-w-0">
                  <span className="font-mono">{o.pattern}</span>{" "}
                  <span className="text-subtle">
                    {o.owners} · {o.files} file{o.files === 1 ? "" : "s"}
                  </span>
                </span>
              </li>
            ))}
          </ul>
        </div>
      )}
      {status && status.changes_requested.length > 0 && (
        <p className="text-[12px] text-danger">
          Changes requested by {status.changes_requested.map((r) => reviewerName(r)).join(", ")}
        </p>
      )}
      {status && status.unresolved_threads > 0 && status.rules.require_resolved_threads && (
        <p className="text-[12px] text-muted">
          {status.unresolved_threads} unresolved thread{status.unresolved_threads === 1 ? "" : "s"}
        </p>
      )}
      {(pending.length > 0 || (status?.pending.length ?? 0) > 0) && (
        <div>
          <p className="text-[11px] text-subtle">Waiting on</p>
          <ul className="mt-1 space-y-1">
            {(pending.length ? pending.map((r) => ({ name: requestName(r.reviewer), required: r.required })) : status!.pending.map((r) => ({ name: reviewerName(r), required: true }))).map((r) => (
              <li key={r.name} className="flex items-center gap-2 text-[12px]">
                <CircleDashed className="size-3 shrink-0 animate-pulse text-accent motion-reduce:animate-none" />
                <span className="font-mono">{r.name}</span>
                {r.required && <span className="text-[10px] text-subtle">required</span>}
              </li>
            ))}
          </ul>
        </div>
      )}
    </div>
  );
}

const requestName = (r: ReviewRequest["reviewer"]) =>
  typeof r === "string" ? r : r.kind === "roll" ? "roll" : r.kind === "user" ? `@${reviewerName(r)}` : `${r.kind}:${reviewerName(r)}`;

/** Ask someone (or roll, or an app, or an agent) for a review. Writers only. */
export function RequestReview({ change, busy, run }: { change: Change; busy: string | null; run: (key: string, fn: () => Promise<unknown>) => Promise<unknown> }) {
  const [who, setWho] = useState("");
  const [required, setRequired] = useState(false);
  const valid = /^(@[\w.-]+|agent:[\w.-]+|app:[\w.-]+|roll)$/.test(who.trim());
  return (
    <form
      className="space-y-2"
      onSubmit={async (e) => {
        e.preventDefault();
        if (!valid) return;
        const ok = await run("request-review", () => codeApi.requestReview(change.id, who.trim(), required));
        if (ok !== undefined) setWho("");
      }}
    >
      <div className="flex gap-2">
        <input
          value={who}
          onChange={(e) => setWho(e.target.value)}
          placeholder="@name, agent:x, app:x or roll"
          spellCheck={false}
          className={`${input} min-w-0 flex-1 font-mono text-[12px]`}
        />
        <button type="submit" disabled={!valid || busy !== null} className={btn} title="Request a review">
          <UserPlus className="size-3.5" />
        </button>
      </div>
      <label className="flex items-center gap-2 text-[11px] text-subtle">
        <input type="checkbox" checked={required} onChange={(e) => setRequired(e.target.checked)} className="accent-[var(--accent)]" />
        required — blocks landing until they review this head
      </label>
    </form>
  );
}

// ── timeline ──

const verdictIcon = (v: Verdict, dim: boolean) =>
  v === "approve" ? (
    <CircleCheck className={`size-3.5 ${dim ? "text-subtle" : "text-ok"}`} />
  ) : v === "request_changes" ? (
    <CircleX className={`size-3.5 ${dim ? "text-subtle" : "text-danger"}`} />
  ) : (
    <MessageSquare className="size-3.5 text-subtle" />
  );

/** Every review, oldest first: who, what verdict, stale or dismissed, and what they wrote. */
export function ReviewTimeline({
  reviews,
  status,
  threads = [],
  canDismiss,
  onDismiss,
  busy = null,
}: {
  reviews: Review[];
  status: ReviewStatus | null;
  threads?: Thread[];
  /** Whether the viewer may dismiss a review (writers). */
  canDismiss?: boolean;
  /** Resolves to `undefined` when it failed. */
  onDismiss?: (review: Review, reason: string) => Promise<unknown>;
  busy?: string | null;
}) {
  const [dismissing, setDismissing] = useState<string | null>(null);
  const [reason, setReason] = useState("");
  const notes = approvalNotes(status);
  const list = timeline(reviews);
  if (!list.length) return <p className="text-[12px] text-subtle">No reviews yet.</p>;
  return (
    <ol className="space-y-4">
      {list.map((r) => {
        const note = notes.get(r.id);
        const dim = r.dismissed || r.stale;
        const opened = threads.filter((t) => t.review === r.id).length;
        return (
          <li key={r.id} className={`rise flex gap-3 ${dim ? "opacity-70" : ""}`}>
            <ReviewerAvatar reviewer={r.reviewer} size={24} />
            <div className="min-w-0 flex-1 space-y-1.5">
              <p className="flex flex-wrap items-center gap-x-2 gap-y-1">
                <ReviewerName reviewer={r.reviewer} source={r.source} />
                <span className="inline-flex items-center gap-1 text-[12px] text-muted">
                  {verdictIcon(r.verdict, dim)} {verdictLabel(r.verdict)}
                </span>
                <span className="font-mono text-[11px] text-subtle">rev {r.revision}</span>
                {r.stale && <span className="rounded-full border border-line px-1.5 py-px text-[10px] text-subtle">stale</span>}
                {r.dismissed && (
                  <span className="rounded-full border border-line px-1.5 py-px text-[10px] text-subtle" title={r.dismiss_reason ?? undefined}>
                    dismissed{r.dismissed_by ? ` by ${r.dismissed_by}` : ""}
                  </span>
                )}
                <Ago ts={r.created_at} />
                {r.url && (
                  <a href={r.url} target="_blank" rel="noreferrer" className="text-[11px] text-subtle hover:text-fg">
                    on GitHub
                  </a>
                )}
              </p>
              {r.verdict === "approve" && note && !note.counted && !r.dismissed && (
                <p className="text-[11px] text-subtle">Doesn&apos;t count{note.reason ? `: ${note.reason}` : ""}</p>
              )}
              {r.ai && <AiReviewHeader ai={r.ai} />}
              {r.body.trim() && <Markdown source={r.ai ? stripAiHeader(r.body) : r.body} />}
              {opened > 0 && (
                <p className="text-[11px] text-subtle">
                  {opened} comment{opened === 1 ? "" : "s"} in the diff
                </p>
              )}
              {r.dismissed && r.dismiss_reason && <p className="text-[11px] text-subtle">Dismissed: {r.dismiss_reason}</p>}
              {canDismiss && onDismiss && !r.dismissed && r.verdict !== "comment" &&
                (dismissing === r.id ? (
                  <form
                    className="rise flex flex-wrap items-center gap-2"
                    onSubmit={async (e) => {
                      e.preventDefault();
                      if ((await onDismiss(r, reason.trim())) === undefined) return;
                      setDismissing(null);
                      setReason("");
                    }}
                  >
                    <input
                      autoFocus
                      value={reason}
                      onChange={(e) => setReason(e.target.value)}
                      placeholder="Why (optional)"
                      className={`${input} min-w-0 flex-1 text-[12px]`}
                    />
                    <button type="submit" disabled={busy !== null} className={btn}>
                      {busy === `dismiss-${r.id}` ? <span className="shimmer">Dismissing…</span> : "Dismiss"}
                    </button>
                    <button type="button" onClick={() => setDismissing(null)} className="text-[11px] text-subtle hover:text-fg">
                      Cancel
                    </button>
                  </form>
                ) : (
                  <button type="button" onClick={() => setDismissing(r.id)} disabled={busy !== null} className="text-[11px] text-subtle transition hover:text-fg">
                    Dismiss review
                  </button>
                ))}
            </div>
          </li>
        );
      })}
    </ol>
  );
}

// ── writing ──

/** Write or preview markdown. */
function MarkdownField({
  value,
  onChange,
  placeholder,
  rows = 4,
  autoFocus,
  onSubmitKey,
  onEscape,
}: {
  value: string;
  onChange: (v: string) => void;
  placeholder: string;
  rows?: number;
  autoFocus?: boolean;
  onSubmitKey?: () => void;
  onEscape?: () => void;
}) {
  const [preview, setPreview] = useState(false);
  return (
    <div className="overflow-hidden rounded-lg border border-line bg-panel font-sans focus-within:border-line-2">
      <div className="flex items-center gap-1 border-b border-line px-1.5 py-1">
        {(["write", "preview"] as const).map((t) => (
          <button
            key={t}
            type="button"
            onClick={() => setPreview(t === "preview")}
            className={`inline-flex items-center gap-1 rounded-md px-2 py-0.5 text-[11px] transition ${
              (t === "preview") === preview ? "bg-panel-2 text-fg" : "text-subtle hover:text-fg"
            }`}
          >
            {t === "write" ? <Pencil className="size-3" /> : <Eye className="size-3" />}
            {t}
          </button>
        ))}
        <span className="ml-auto pr-1 text-[10px] text-subtle">markdown</span>
      </div>
      {preview ? (
        <div className="min-h-20 px-3 py-2">
          {value.trim() ? <Markdown source={value} /> : <p className="text-[12px] text-subtle">Nothing to preview.</p>}
        </div>
      ) : (
        <textarea
          value={value}
          onChange={(e) => onChange(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && (e.metaKey || e.ctrlKey) && onSubmitKey) {
              e.preventDefault();
              onSubmitKey();
            } else if (e.key === "Escape" && onEscape) onEscape();
          }}
          rows={rows}
          autoFocus={autoFocus}
          placeholder={placeholder}
          className="block w-full resize-y bg-transparent px-3 py-2 text-[13px] outline-none placeholder:text-subtle"
        />
      )}
    </div>
  );
}

/**
 * The composer under a selected line or range: add the comment to the pending review, or post it
 * on its own right away. `Suggest` drops the selected lines into a ```suggestion block.
 */
export function LineComposer({
  selection,
  original,
  busy,
  hasPending,
  onAdd,
  onPostNow,
  onCancel,
}: {
  selection: LineSelection;
  original: string[] | null;
  busy: string | null;
  hasPending: boolean;
  onAdd: (body: string) => void;
  onPostNow: (body: string) => Promise<boolean>;
  onCancel: () => void;
}) {
  const [body, setBody] = useState("");
  const suggest = () => {
    if (!original) return;
    const block = `\`\`\`suggestion\n${original.join("\n")}\n\`\`\``;
    setBody((b) => (b.trim() ? `${b.replace(/\s+$/, "")}\n\n${block}` : block));
  };
  return (
    <div className="rise space-y-2 rounded-lg border border-line bg-panel p-3 font-sans">
      <p className="text-[11px] text-subtle">
        Comment on <span className="font-mono text-muted">{selection.path}</span> ·{" "}
        <span className="font-mono text-muted">
          {selectionLabel(selection)}
          {selection.side === "old" && " (removed)"}
        </span>
      </p>
      <MarkdownField
        value={body}
        onChange={setBody}
        placeholder="Leave a comment… (⌘↵ adds it to your review)"
        autoFocus
        onSubmitKey={() => body.trim() && onAdd(body)}
        onEscape={onCancel}
      />
      <div className="flex flex-wrap items-center gap-2">
        <button type="button" onClick={() => onAdd(body)} disabled={!body.trim()} className={primary}>
          {hasPending ? "Add to review" : "Start a review"}
        </button>
        <button
          type="button"
          onClick={async () => {
            if (await onPostNow(body)) setBody("");
          }}
          disabled={!body.trim() || busy !== null}
          className={btn}
        >
          {busy === "thread" ? <span className="shimmer">Posting…</span> : "Comment now"}
        </button>
        {original && selection.side === "new" && (
          <button type="button" onClick={suggest} className={btn} title="Propose replacement lines">
            Suggest
          </button>
        )}
        <button type="button" onClick={onCancel} className="ml-auto text-[12px] text-subtle hover:text-fg">
          Cancel
        </button>
      </div>
    </div>
  );
}

/** A comment waiting in the pending review: edit or remove it before submitting. */
export function DraftCard({
  draft,
  original,
  onEdit,
  onRemove,
}: {
  draft: DraftComment;
  original: string[] | null;
  onEdit: (body: string) => void;
  onRemove: () => void;
}) {
  const [editing, setEditing] = useState(false);
  const [body, setBody] = useState(draft.body);
  const suggestion = /```suggestion\n([\s\S]*?)\n?```/.exec(draft.body)?.[1] ?? null;
  return (
    <div className="space-y-2 rounded-lg border border-dashed border-line-2 bg-panel p-3 font-sans">
      <p className="flex items-center gap-2 text-[11px] text-subtle">
        <span className="rounded-full border border-accent/40 px-1.5 py-px text-accent">pending</span>
        <span className="font-mono">{selectionLabel({ start: draft.start_line ?? draft.line, end: draft.line })}</span>
        <span className="ml-auto flex items-center gap-1">
          <button type="button" onClick={() => setEditing((e) => !e)} className="p-1 hover:text-fg" aria-label="Edit pending comment">
            <Pencil className="size-3" />
          </button>
          <button type="button" onClick={onRemove} className="p-1 hover:text-fg" aria-label="Remove pending comment">
            <Trash2 className="size-3" />
          </button>
        </span>
      </p>
      {editing ? (
        <div className="space-y-2">
          <MarkdownField value={body} onChange={setBody} placeholder="Comment" onSubmitKey={() => (onEdit(body), setEditing(false))} />
          <button
            type="button"
            onClick={() => {
              onEdit(body);
              setEditing(false);
            }}
            disabled={!body.trim()}
            className={btn}
          >
            Save
          </button>
        </div>
      ) : (
        <>
          <Markdown source={suggestion != null ? draft.body.replace(/```suggestion\n[\s\S]*?\n?```/, "").trim() : draft.body} />
          {suggestion != null && <SuggestionView original={original} suggestion={suggestion} />}
        </>
      )}
    </div>
  );
}

const VERDICTS: { value: Verdict; label: string; writer: boolean }[] = [
  { value: "comment", label: "Comment", writer: false },
  { value: "approve", label: "Approve", writer: true },
  { value: "request_changes", label: "Request changes", writer: true },
];

/** The review being written: verdict, summary, the pending line comments, submit or discard. */
export const ReviewComposer = forwardRef<
  HTMLButtonElement,
  {
    change: Change;
    pending: PendingReview;
    stale: boolean;
    writer: boolean;
    verdict: Verdict;
    onVerdict: (v: Verdict) => void;
    onBody: (body: string) => void;
    onJump: (draft: DraftComment) => void;
    onSubmit: () => void;
    onDiscard: () => void;
    busy: string | null;
  }
>(function ReviewComposer({ change, pending, stale, writer, verdict, onVerdict, onBody, onJump, onSubmit, onDiscard, busy }, submitRef) {
  const n = pending.comments.length;
  const ok = canSubmit(verdict, pending.body, n) && (writer || verdict === "comment");
  const label = verdict === "approve" ? `Approve rev ${change.revision}` : verdict === "request_changes" ? "Request changes" : "Submit review";
  return (
    <div id="review-composer" className="scroll-mt-24 space-y-3">
      <div className="flex rounded-lg border border-line bg-panel p-0.5" role="radiogroup" aria-label="Verdict">
        {VERDICTS.map((v) => (
          <button
            key={v.value}
            type="button"
            role="radio"
            aria-checked={v.value === verdict}
            disabled={v.writer && !writer}
            onClick={() => onVerdict(v.value)}
            title={v.writer && !writer ? "Needs write access" : undefined}
            className={`flex-1 rounded-md px-2.5 py-1 text-[12px] transition disabled:opacity-40 ${
              v.value === verdict
                ? v.value === "approve"
                  ? "bg-panel-2 text-ok"
                  : v.value === "request_changes"
                    ? "bg-panel-2 text-danger"
                    : "bg-panel-2 text-fg"
                : "text-subtle hover:text-fg"
            }`}
          >
            {v.label}
          </button>
        ))}
      </div>
      <MarkdownField
        value={pending.body}
        onChange={onBody}
        placeholder={verdict === "approve" ? "Optional: why it's good to land" : "Summary of your review (markdown)"}
        rows={4}
        onSubmitKey={() => ok && onSubmit()}
      />
      {n > 0 && (
        <div>
          <p className="text-[11px] text-subtle">
            {n} pending comment{n === 1 ? "" : "s"}
            {stale && <span className="text-accent"> · written on rev {pending.revision}; lines may have moved</span>}
          </p>
          <ul className="mt-1 space-y-0.5">
            {pending.comments.map((d) => (
              <li key={d.id}>
                <button type="button" onClick={() => onJump(d)} className="flex w-full min-w-0 items-center gap-2 text-left text-[12px] text-muted hover:text-fg">
                  <span className="shrink-0 font-mono text-[11px] text-subtle">
                    {d.path}:{selectionLabel({ start: d.start_line ?? d.line, end: d.line })}
                  </span>
                  <span className="truncate">{d.body.split("\n")[0]}</span>
                </button>
              </li>
            ))}
          </ul>
        </div>
      )}
      <div className="flex flex-wrap items-center gap-2">
        <button ref={submitRef} type="button" onClick={onSubmit} disabled={!ok || busy !== null} className={primary}>
          {busy === "review" ? <span className="shimmer">Submitting…</span> : label}
        </button>
        {(n > 0 || pending.body.trim()) && (
          <button type="button" onClick={onDiscard} disabled={busy !== null} className="text-[12px] text-subtle transition hover:text-fg">
            Discard pending review
          </button>
        )}
        <span className="ml-auto text-[11px] text-subtle">a approve · n / p threads · r reply</span>
      </div>
    </div>
  );
});
