"use client";

import { HorizonMark } from "@roll/brand";
import { ArrowLeft, Database, ExternalLink, GitMergeConflict, ListOrdered, MemoryStick, MessageSquare, Trash2, X } from "lucide-react";
import Link from "next/link";
import { useCallback, useEffect, useRef, useState } from "react";
import {
  codeApi,
  shortId,
  type Change,
  type ChangeState,
  type Compare,
  type Lane,
  type LandResult,
  type Repo,
  type Status,
  type Verdict,
} from "@/lib/code-api";
import { checkRows } from "@/lib/ci";
import { canWrite, isAdmin, landSummary, landVerb } from "@/lib/queue";
import { addDraft, draftFor, emptyPending, removeDraft, reviewHeadline, reviewPayload, updateDraft, type LineSelection } from "@/lib/review";
import { CodeShell } from "./shell";
import { ago, btn, Confirm, EmptyPanel, ErrorNote, Panel, panel, primary } from "../ui";
import { DiffViewer, type DiffReview } from "./diff";
import { AiProgress, RunAiReview, useAiJobs } from "./ai";
import { CodeSearch } from "./search";
import { DraftCard, LineComposer, RequestReview, ReviewComposer, ReviewStatusView, ReviewTimeline, useChangeReview, usePendingReview } from "./review";
import type { ThreadActions } from "./thread";
import { repoCrumb, RepoGate, useRepo } from "./frame";
import {
  BlockerList,
  ChangeStateBadge,
  ChangeStateIcon,
  CheckIcon,
  ChecksSummary,
  GitHubMark,
  POLL_MS,
  QueuePill,
  Segmented,
  SourceBadge,
  sortChanges,
  useAction,
  useRepoEvents,
} from "./shared";

type Filter = "active" | ChangeState | "all";

const FILTERS: { label: string; value: Filter }[] = [
  { label: "active", value: "active" },
  { label: "conflict", value: "conflict" },
  { label: "landed", value: "landed" },
  { label: "closed", value: "closed" },
  { label: "all", value: "all" },
];

export function ChangesTab({ repo, tick }: { repo: Repo; tick: number }) {
  const [changes, setChanges] = useState<Change[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [filter, setFilter] = useState<Filter>("active");

  useEffect(() => {
    let alive = true;
    codeApi.changes(repo.full_name).then(
      (c) => alive && (setChanges(c), setError(null)),
      (e) => alive && setError(e instanceof Error ? e.message : String(e)),
    );
    return () => {
      alive = false;
    };
  }, [repo.full_name, tick]);

  const shown = sortChanges(
    (changes ?? []).filter((c) =>
      filter === "all" ? true : filter === "active" ? ["open", "queued", "conflict"].includes(c.state) : c.state === filter,
    ),
  );

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="w-full max-w-md">
          <Segmented value={filter} options={FILTERS} onChange={setFilter} />
        </div>
        <p className="text-[12px] text-subtle">
          Push a branch, open a PR on GitHub, or <span className="font-mono">POST …/changes</span>.
        </p>
      </div>
      {error && <p className="text-[13px] text-danger">{error}</p>}
      {changes === null ? (
        <p className="text-[13px] text-subtle">
          <span className="shimmer">Loading changes…</span>
        </p>
      ) : shown.length === 0 ? (
        <p className={`${panel} px-5 py-6 text-[13px] text-subtle`}>
          {filter === "active" ? "Nothing in flight — every change has landed or closed." : `No ${filter === "all" ? "" : `${filter} `}changes.`}
        </p>
      ) : (
        <ul className={panel}>
          {shown.map((c) => (
            <li key={c.id} className="rise border-b border-line last:border-0">
              <Link
                href={`/dashboard/code/${repo.full_name}/changes/${c.number}`}
                className="grid grid-cols-[auto_minmax(0,1fr)_auto] items-center gap-x-4 gap-y-1 px-5 py-3 transition hover:bg-panel-2/60 md:grid-cols-[auto_minmax(0,1fr)_auto_auto]"
              >
                <ChangeStateIcon state={c.state} />
                <div className="min-w-0">
                  <p className="flex items-center gap-2 truncate text-[13px]">
                    <span className="truncate">{c.title}</span>
                    {c.github_pr != null && <SourceBadge source={`github#${c.github_pr}`} url={null} />}
                  </p>
                  <p className="truncate font-mono text-[11px] text-subtle">
                    #{c.number} · {c.author} · {shortId(c.head)} · rev {c.revision}
                    {c.draft && " · draft"}
                    {c.state === "queued" && c.queue?.position != null && <span className="text-accent"> · queue #{c.queue.position}</span>}
                    {c.review && c.state !== "landed" && c.state !== "closed" && c.review.state !== "pending" && (
                      <span className={c.review.state === "changes_requested" ? "text-danger" : "text-ok"}> · {reviewHeadline(c.review).label.toLowerCase()}</span>
                    )}
                    {c.state === "open" && c.queue?.state === "ejected" && <span className="text-danger"> · ejected from the queue</span>}
                    {c.conflict && <span className="text-danger"> · {c.conflict.paths.length} conflicting</span>}
                  </p>
                </div>
                <span className="hidden md:block">
                  <ChecksSummary checks={c.checks} />
                </span>
                <span className="flex items-center gap-3 text-[11px] text-subtle">
                  {c.preview?.url && c.preview.state === "running" && (
                    <span className="inline-block size-1.5 rounded-full bg-ok" title="preview running" />
                  )}
                  {ago(c.updated_at)}
                </span>
              </Link>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

// --- Change detail -------------------------------------------------------------------------------

/** A change by per-repo number (or `chg_…` id, which landed events and work items carry). */
export function CodeChange({ fullName, id }: { fullName: string; id: string }) {
  const { repo, info, offline, missing, refresh: refreshRepo } = useRepo(fullName);
  const [change, setChange] = useState<Change | null>(null);
  const [notFound, setNotFound] = useState(false);
  const [diff, setDiff] = useState<Compare | null>(null);
  const [diffError, setDiffError] = useState<string | null>(null);
  const [landed, setLanded] = useState<LandResult | null>(null);
  const [bypassing, setBypassing] = useState(false);
  const review = useChangeReview(change);
  const ai = useAiJobs(change?.id ?? null, review.reload);
  const { pending, setPending, discard } = usePendingReview(change);
  const [selection, setSelection] = useState<LineSelection | null>(null);
  const [focused, setFocused] = useState<string | null>(null);
  const [replying, setReplying] = useState<string | null>(null);
  const [verdict, setVerdict] = useState<Verdict>("comment");
  const submitRef = useRef<HTMLButtonElement>(null);
  const writer = !!repo && canWrite(info, repo);
  const reviewable = !!change && (change.state === "open" || change.state === "queued" || change.state === "conflict");

  const load = useCallback(async () => {
    try {
      setChange(await codeApi.change(fullName, id));
      setNotFound(false);
    } catch {
      setNotFound(true);
    }
  }, [fullName, id]);

  useEffect(() => {
    let alive = true;
    let timer: ReturnType<typeof setTimeout>;
    const loop = async () => {
      await load();
      if (alive) timer = setTimeout(loop, POLL_MS);
    };
    loop();
    return () => {
      alive = false;
      clearTimeout(timer);
    };
  }, [load]);

  // Events for this repo (reviews and threads included) refetch right away instead of waiting for the poll.
  const reloadReview = review.reload;
  const reloadAi = ai.reload;
  const onEvent = useCallback(() => {
    load();
    reloadReview();
    reloadAi();
  }, [load, reloadReview, reloadAi]);
  useRepoEvents(repo?.full_name ?? null, { onOther: onEvent });

  const refresh = useCallback(async () => {
    await Promise.all([load(), refreshRepo(), reloadReview(), reloadAi()]);
  }, [load, refreshRepo, reloadReview, reloadAi]);
  const { busy, error, run } = useAction(refresh);

  /** `a` jumps to the composer with Approve chosen; the submit button is the confirmation. */
  const startApprove = useCallback(() => {
    setVerdict("approve");
    document.getElementById("review-composer")?.scrollIntoView({ block: "center" });
    requestAnimationFrame(() => submitRef.current?.focus());
  }, []);

  // `r` replies to the thread `n`/`p` landed on; `a` approves (writers, open changes).
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.metaKey || e.ctrlKey || e.altKey) return;
      const el = e.target as HTMLElement | null;
      if (el && (el.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(el.tagName))) return;
      if (e.key === "r" && focused) {
        e.preventDefault();
        setReplying(focused);
      } else if (e.key === "a" && writer && reviewable) {
        e.preventDefault();
        startApprove();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [focused, writer, reviewable, startApprove]);

  const base = change?.base;
  const head = change?.head;
  useEffect(() => {
    if (!repo || !base || !head) return;
    let alive = true;
    codeApi.compare(repo.full_name, base, head).then(
      (d) => alive && (setDiff(d), setDiffError(null)),
      (e) => alive && setDiffError(e instanceof Error ? e.message : String(e)),
    );
    return () => {
      alive = false;
    };
  }, [repo, base, head]);

  const crumb = repoCrumb(fullName, <span className="font-mono">{change ? `#${change.number}` : id}</span>);

  return (
    <RepoGate id={fullName} crumb={crumb} offline={offline} missing={missing} ready={!!repo && (!!change || notFound)}>
      {() => {
        const r = repo!;
        if (!change) {
          return (
            <CodeShell crumb={crumb}>
              <EmptyPanel>
                <HorizonMark state="sleep" size={40} />
                <h2 className="text-[15px] font-medium">No change {id}</h2>
                <p className="max-w-md text-[13px] text-muted">It may belong to another repository, or the link is wrong.</p>
                <Link href={`/dashboard/code/${r.full_name}#changes`} className={btn}>
                  <ArrowLeft className="size-3.5" /> All changes
                </Link>
              </EmptyPanel>
            </CodeShell>
          );
        }
        const c = change;
        const queuePolicy = r.land_policy !== "post_merge";
        const admin = isAdmin(info);
        const signedIn = !!info && (!info.auth.required || info.auth.caller.kind === "admin" || info.auth.caller.kind === "owner");
        const actions: ThreadActions = {
          writer,
          open: reviewable,
          busy,
          reply: (t, body) => run(`reply-${t}`, () => codeApi.reply(t, body)),
          setResolved: (t, resolved) => run(`resolve-${t}`, () => (resolved ? codeApi.resolveThread(t) : codeApi.unresolveThread(t))),
          apply: (t, comment) => run(`apply-${comment}`, () => codeApi.applySuggestion(t, comment)),
          replying,
          onReplyDone: () => setReplying(null),
        };
        const diffReview: DiffReview = {
          threads: review.threads ?? [],
          actions: signedIn ? actions : undefined,
          focused,
          onFocus: setFocused,
          compose:
            signedIn && reviewable
              ? {
                  selection,
                  onSelect: setSelection,
                  pending,
                  renderComposer: (sel, original) => (
                    <LineComposer
                      selection={sel}
                      original={original}
                      busy={busy}
                      hasPending={pending.comments.length > 0}
                      onCancel={() => setSelection(null)}
                      onAdd={(body) => {
                        setPending((p) => addDraft(p.comments.length || p.body.trim() ? p : emptyPending(c.id, c.revision), draftFor(sel, body)));
                        setSelection(null);
                      }}
                      onPostNow={async (body) => {
                        const out = await run("thread", () => codeApi.createThread(c.id, draftFor(sel, body)));
                        if (out === undefined) return false;
                        setSelection(null);
                        return true;
                      }}
                    />
                  ),
                  renderDraft: (d, original) => (
                    <DraftCard
                      draft={d}
                      original={original}
                      onEdit={(body) => setPending((p) => updateDraft(p, d.id, body))}
                      onRemove={() => setPending((p) => removeDraft(p, d.id))}
                    />
                  ),
                }
              : undefined,
        };
        const submitReview = async () => {
          const out = await run("review", () => codeApi.submitReview(c.id, reviewPayload(pending, verdict)));
          if (out !== undefined) {
            discard();
            setVerdict("comment");
          }
        };
        const blockers = c.blockers ?? [];
        const canLand = (c.state === "open" || c.state === "conflict") && !c.draft;
        const conflict = landed?.status === "conflict" ? landed : c.conflict;
        /** Land (post-merge), queue (merge queue) or bypass: one path, so every outcome reads the same. */
        const land = async (key: string, fn: () => Promise<LandResult>) => {
          setLanded(null);
          const out: { res: LandResult | null } = { res: null };
          await run(key, async () => {
            out.res = await fn();
            // A conflict or a blocker is an answer, not a failure, but the marks should still say it didn't land.
            if (out.res.status === "conflict" || out.res.status === "blocked") throw new Error(landSummary(out.res).text);
            return out.res;
          });
          setLanded(out.res);
          if (out.res?.status === "conflict" || out.res?.status === "blocked") load();
        };
        const enqueue = (lane?: Lane) => land(lane === "urgent" ? "enqueue-urgent" : "land", () => codeApi.enqueue(c.id, lane));

        return (
          <CodeShell
            crumb={crumb}
            actions={
              <span className="flex items-center gap-2">
                {signedIn && reviewable && (
                  <button
                    onClick={() => document.getElementById("review-composer")?.scrollIntoView({ block: "center", behavior: "smooth" })}
                    className={btn}
                    title="Go to your review"
                  >
                    <MessageSquare className="size-3.5" /> Review
                    {pending.comments.length > 0 && <span className="font-mono text-[11px] text-accent">{pending.comments.length}</span>}
                  </button>
                )}
                {c.state === "queued" && writer && (
                  <button onClick={() => run("dequeue", () => codeApi.dequeue(c.id))} disabled={busy !== null} className={btn}>
                    {busy === "dequeue" ? <span className="shimmer">Removing…</span> : "Remove from queue"}
                  </button>
                )}
                {canLand && queuePolicy && writer && (
                  <button
                    onClick={() => enqueue("urgent")}
                    disabled={busy !== null || blockers.length > 0}
                    className={btn}
                    title="Queue it ahead of the normal lane"
                  >
                    {busy === "enqueue-urgent" ? <span className="shimmer">Queueing…</span> : "Urgent"}
                  </button>
                )}
                {canLand && (
                  <button
                    onClick={() => (queuePolicy ? enqueue() : land("land", () => codeApi.landChange(c.id)))}
                    disabled={busy !== null || (queuePolicy && blockers.length > 0)}
                    className={primary}
                  >
                    {busy === "land" ? (
                      <span className="shimmer">{queuePolicy ? "Queueing…" : "Landing…"}</span>
                    ) : (
                      landVerb(r.land_policy)
                    )}
                  </button>
                )}
              </span>
            }
          >
            <div className="flex flex-wrap items-start justify-between gap-3">
              <div className="min-w-0">
                <h1 className="text-2xl font-semibold tracking-[-0.04em]">{c.title}</h1>
                <p className="mt-2 flex flex-wrap items-center gap-x-2 gap-y-1 text-[13px] text-muted">
                  <ChangeStateBadge state={c.state} />
                  {c.queue && (c.state === "queued" || c.queue.state === "ejected") && <QueuePill queue={c.queue} />}
                  {c.draft && (
                    <span className="rounded-full border border-line px-2 py-0.5 text-[12px] text-subtle">draft</span>
                  )}
                  <span className="font-mono text-[12px] text-subtle">#{c.number}</span>
                  <span className="text-subtle">·</span>
                  <span>{c.author}</span>
                  <span className="text-subtle">·</span>
                  <span className="font-mono text-[12px] text-subtle">
                    {shortId(c.base)} → {shortId(c.head)} · rev {c.revision}
                  </span>
                  {c.work_item && (
                    <>
                      <span className="text-subtle">·</span>
                      <span className="font-mono text-[12px] text-subtle">{c.work_item}</span>
                    </>
                  )}
                  {c.github_url && (
                    <a href={c.github_url} target="_blank" rel="noreferrer" className="inline-flex items-center gap-1 text-[12px] hover:text-fg">
                      <GitHubMark className="size-3" /> #{c.github_pr}
                    </a>
                  )}
                </p>
              </div>
              <p className="font-mono text-[11px] text-subtle">
                opened {ago(c.created_at)} · updated {ago(c.updated_at)}
              </p>
            </div>

            <ErrorNote error={error} />

            {c.state === "landed" && c.landed_as && (
              <p className="rise mt-4 rounded-lg border border-line bg-panel px-3 py-2 text-[13px] text-muted">
                Landed on <span className="font-mono text-fg">{r.default_branch}</span> as{" "}
                <span className="font-mono text-fg">{shortId(c.landed_as)}</span>.
              </p>
            )}
            {landed?.status === "queued" && c.state === "queued" && (
              <p className="rise mt-4 flex items-center gap-2 rounded-lg border border-line bg-panel px-3 py-2 text-[13px] text-muted">
                <ListOrdered className="size-4 text-accent" /> {landSummary(landed).text}. It lands once its batch passes the
                queue&apos;s checks.
              </p>
            )}
            {c.state === "open" && c.queue?.state === "ejected" && c.queue.error && <EjectedNote reason={c.queue.error} output={c.queue.output ?? null} />}
            {conflict && c.state !== "landed" && <ConflictNote paths={conflict.paths} head={conflict.head} />}

            <div className="mt-6 grid gap-4 lg:grid-cols-3">
              <div className="min-w-0 space-y-4 lg:col-span-2">
                {c.body.trim() && (
                  <Panel title="Description">
                    <p className="px-5 py-4 text-[13px] whitespace-pre-wrap text-muted">{c.body}</p>
                  </Panel>
                )}
                <Panel title="Files">
                  <div className="p-4">
                    {diffError ? (
                      <p className="text-[13px] text-danger">{diffError}</p>
                    ) : diff ? (
                      <DiffViewer files={diff.files} review={diffReview} />
                    ) : (
                      <p className="text-[13px] text-subtle">
                        <span className="shimmer">Loading diff…</span>
                      </p>
                    )}
                    {diff && diffReview.compose && (review.threads?.length ?? 0) === 0 && pending.comments.length === 0 && (
                      <p className="mt-3 text-[11px] text-subtle">Click a line number to comment; shift-click for a range.</p>
                    )}
                  </div>
                </Panel>
                <Panel
                  title="Reviews"
                  aside={review.error && <span className="text-[11px] text-danger">{review.error}</span>}
                >
                  <div className="space-y-6 px-5 py-4">
                    {review.reviews === null ? (
                      <p className="text-[12px] text-subtle">
                        <span className="shimmer">Loading reviews…</span>
                      </p>
                    ) : (
                      <ReviewTimeline
                        reviews={review.reviews}
                        status={review.status}
                        threads={review.threads ?? []}
                        canDismiss={writer}
                        busy={busy}
                        onDismiss={(rv, reason) => run(`dismiss-${rv.id}`, () => codeApi.dismissReview(c.id, rv.id, reason || undefined))}
                      />
                    )}
                    {signedIn && reviewable && (
                      <div className="border-t border-line pt-5">
                        <ReviewComposer
                          ref={submitRef}
                          change={c}
                          pending={pending}
                          stale={pending.comments.length > 0 && pending.revision !== c.revision}
                          writer={writer}
                          verdict={verdict}
                          onVerdict={setVerdict}
                          onBody={(body) => setPending((p) => ({ ...p, body }))}
                          onJump={(d) => document.getElementById(`diff-${d.path}`)?.scrollIntoView({ block: "start", behavior: "smooth" })}
                          onSubmit={submitReview}
                          onDiscard={discard}
                          busy={busy}
                        />
                      </div>
                    )}
                  </div>
                </Panel>
              </div>
              <div className="min-w-0 space-y-4">
                <Panel title="Review">
                  <div className="space-y-4 px-5 py-4">
                    <ReviewStatusView summary={c.review} status={review.status} requests={review.requests} />
                    {writer && reviewable && <RequestReview change={c} busy={busy} run={run} />}
                  </div>
                </Panel>
                {(ai.active || (writer && reviewable) || ai.failure) && (
                  <Panel title="roll reviewer" aside={writer && reviewable && <RunAiReview change={c} active={ai.active} busy={busy} run={run} />}>
                    <div className="space-y-2 px-5 py-4 text-[12px]">
                      {ai.active ? (
                        <AiProgress job={ai.active} />
                      ) : ai.failure ? (
                        <p className="text-muted">
                          roll couldn&apos;t finish its last review{ai.failure.error ? `: ${ai.failure.error}` : "."}
                        </p>
                      ) : (
                        <p className="text-subtle">
                          Ask roll for a review, or reply <span className="font-mono">@roll</span> in any thread.
                        </p>
                      )}
                    </div>
                  </Panel>
                )}
                {blockers.length > 0 && c.state !== "landed" && c.state !== "closed" && (
                  <Panel title={queuePolicy ? "Before it can queue" : "Before it can land"}>
                    <div className="px-5 py-4">
                      <BlockerList
                        blockers={blockers}
                        action={(kind) =>
                          !writer ? null : kind === "ready" ? (
                            <button
                              onClick={() => run("ready", () => codeApi.updateChange(c.id, { draft: false }))}
                              disabled={busy !== null}
                              className={btn}
                            >
                              {busy === "ready" ? <span className="shimmer">Saving…</span> : "Mark ready"}
                            </button>
                          ) : kind === "review" ? (
                            reviewable ? (
                              <button onClick={startApprove} className={btn}>
                                Review it
                              </button>
                            ) : null
                          ) : kind === "approve" ? (
                            <button onClick={() => run("approve", () => codeApi.approveRun(c.id))} disabled={busy !== null} className={btn}>
                              {busy === "approve" ? <span className="shimmer">Approving…</span> : `Approve running rev ${c.revision}`}
                            </button>
                          ) : null
                        }
                      />
                    </div>
                  </Panel>
                )}
                <ChecksPanel repo={r.full_name} change={c} policy={r.land_policy} required={r.required_checks} />
                {c.state !== "landed" && c.state !== "closed" && (
                  <Panel title="Search this change">
                    <div className="px-5 py-4">
                      <CodeSearch repo={r.full_name} rev={c.head} defaultBranch={r.default_branch} placeholder="Search this change…" />
                    </div>
                  </Panel>
                )}
                <PreviewPanel change={c} busy={busy} run={run} />
                {c.state !== "landed" && c.state !== "closed" && (
                  <div className="flex flex-wrap gap-2">
                    {writer && c.state === "open" && !c.draft && (
                      <button onClick={() => run("draft", () => codeApi.updateChange(c.id, { draft: true }))} disabled={busy !== null} className={btn}>
                        Mark as draft
                      </button>
                    )}
                    <button onClick={() => run("close", () => codeApi.closeChange(c.id))} disabled={busy !== null} className={btn}>
                      <X className="size-3.5" /> Close change
                    </button>
                  </div>
                )}
                {queuePolicy && admin && canLand && (
                  <div>
                    {bypassing ? (
                      <Confirm
                        word={`#${c.number}`}
                        action="Land now"
                        busy={busy === "bypass"}
                        onCancel={() => setBypassing(false)}
                        onConfirm={async () => {
                          await land("bypass", () => codeApi.landChange(c.id, { bypass: true }));
                          setBypassing(false);
                        }}
                      >
                        Lands this change on <span className="font-mono text-fg">{r.default_branch}</span> right away, without the
                        queue&apos;s checks. For emergencies; it&apos;s written to the audit log.
                      </Confirm>
                    ) : (
                      <button onClick={() => setBypassing(true)} className="text-[12px] text-subtle transition hover:text-fg">
                        Land past the queue…
                      </button>
                    )}
                  </div>
                )}
              </div>
            </div>
          </CodeShell>
        );
      }}
    </RepoGate>
  );
}

function ConflictNote({ paths, head }: { paths: string[]; head: string }) {
  return (
    <div className="rise mt-4 rounded-lg border border-danger/40 bg-danger/5 px-4 py-3 text-[13px]">
      <p className="flex items-center gap-2 text-danger">
        <GitMergeConflict className="size-4" /> Didn&apos;t land: {paths.length} file{paths.length === 1 ? "" : "s"} changed on main
        since this change&apos;s base and couldn&apos;t merge cleanly.
      </p>
      <ul className="mt-2 space-y-0.5 font-mono text-[12px] text-muted">
        {paths.map((p) => (
          <li key={p}>{p}</li>
        ))}
      </ul>
      <p className="mt-2 text-[12px] text-subtle">
        Revise it on top of <span className="font-mono">{shortId(head)}</span> and land again.
      </p>
    </div>
  );
}

/** Why the queue ejected it, then the failing checks' output. */
function EjectedNote({ reason, output }: { reason: string; output: string | null }) {
  const first = reason.trim();
  const tail = output?.trim();
  return (
    <div className="rise mt-4 rounded-lg border border-danger/40 bg-danger/5 px-4 py-3 text-[13px]">
      <p className="flex items-center gap-2 text-danger">
        <ListOrdered className="size-4" /> Ejected from the merge queue: {first}
      </p>
      {tail && (
        <pre className="mt-2 max-h-64 overflow-auto font-mono text-[11px] leading-5 whitespace-pre-wrap text-muted">{tail}</pre>
      )}
      <p className="mt-2 text-[12px] text-subtle">Revise it, then add it to the queue again.</p>
    </div>
  );
}

function ChecksPanel({ repo, change, policy, required }: { repo: string; change: Change; policy: Repo["land_policy"]; required: string[] }) {
  // Commit statuses posted for the head (external CI): they're checks already; this adds who posted them.
  const [statuses, setStatuses] = useState<Status[] | null>(null);
  // Refetch when a check moves (a status arrives as one), not on every poll.
  const checksKey = change.checks.map((c) => `${c.name}:${c.status}`).join(",");
  useEffect(() => {
    let alive = true;
    codeApi.statuses(repo, change.head).then(
      (c) => alive && setStatuses(c.statuses),
      () => alive && setStatuses(null),
    );
    return () => {
      alive = false;
    };
  }, [repo, change.head, checksKey]);
  const rows = checkRows(change.checks, statuses);
  const names = new Set(rows.map((c) => c.name));
  const gated = policy !== "post_merge";
  const missing = gated ? required.filter((r) => !names.has(r)) : [];
  return (
    <Panel title="Checks" aside={<ChecksSummary checks={change.checks} />}>
      {rows.length === 0 && missing.length === 0 ? (
        <p className="px-5 py-4 text-[12px] text-subtle">
          {policy === "post_merge"
            ? "Post-merge: checks run on main after landing."
            : "No checks reported yet. roll.toml checks run automatically; external CI posts commit statuses."}
        </p>
      ) : (
        <ul>
          {rows.map((ck) => (
            <li key={ck.name} className="flex items-center gap-2.5 border-b border-line px-5 py-2.5 last:border-0">
              <CheckIcon status={ck.status} />
              <div className="min-w-0 flex-1">
                <p className="truncate font-mono text-[12px]">
                  {ck.name}
                  {required.includes(ck.name) && gated && <span className="ml-1.5 text-[10px] text-subtle">required</span>}
                </p>
                {ck.summary && <p className={`truncate text-[11px] ${ck.errored ? "text-danger" : "text-subtle"}`}>{ck.summary}</p>}
              </div>
              <span className="shrink-0 text-right text-[11px] text-subtle" title={ck.updated ? `updated ${ago(ck.updated)}` : undefined}>
                {ck.source}
                {ck.creator && <span className="block font-mono text-[10px]">{ck.creator}</span>}
              </span>
              {ck.url && (
                <a href={ck.url} target="_blank" rel="noreferrer" className="text-subtle hover:text-fg" aria-label={`Open ${ck.name}`}>
                  <ExternalLink className="size-3.5" />
                </a>
              )}
            </li>
          ))}
          {missing.map((m) => (
            <li key={m} className="flex items-center gap-2.5 border-b border-line px-5 py-2.5 last:border-0">
              <CheckIcon status="queued" />
              <p className="flex-1 truncate font-mono text-[12px] text-subtle">
                {m} <span className="text-[10px]">required · not reported</span>
              </p>
            </li>
          ))}
        </ul>
      )}
    </Panel>
  );
}

function PreviewPanel({ change, busy, run }: { change: Change; busy: string | null; run: ReturnType<typeof useAction>["run"] }) {
  const p = change.preview;
  const open = change.state !== "landed" && change.state !== "closed";
  return (
    <Panel
      title="Preview"
      aside={
        p && (
          <span className="flex items-center gap-1.5 text-[12px] text-subtle">
            <span
              className={`inline-block size-1.5 rounded-full ${
                p.state === "running"
                  ? "bg-ok"
                  : p.state === "failed"
                    ? "bg-danger"
                    : p.state === "creating"
                      ? "bg-accent animate-pulse motion-reduce:animate-none"
                      : "border border-line-2"
              }`}
            />
            {p.state === "sleeping" ? "scaled to zero" : p.state}
          </span>
        )
      }
    >
      {p ? (
        <div className="space-y-3 px-5 py-4 text-[13px]">
          {p.url && (
            <a href={p.url} target="_blank" rel="noreferrer" className="flex items-center gap-1.5 truncate font-mono text-[12px] text-accent hover:underline">
              {p.url.replace(/^https?:\/\//, "")} <ExternalLink className="size-3 shrink-0" />
            </a>
          )}
          {p.state === "creating" && (
            <p className="text-[12px] text-subtle">
              <span className="shimmer">Branching data and starting the app…</span>
            </p>
          )}
          {p.error && <p className="text-[12px] text-danger">{p.error}</p>}
          {(p.postgres || p.redis) && (
            <ul className="space-y-1 text-[12px] text-muted">
              {p.postgres && (
                <li className="flex items-center gap-2">
                  <Database className="size-3.5 text-subtle" />
                  <Link href="/dashboard/databases" className="font-mono hover:text-fg">
                    {p.postgres.branch_id}
                  </Link>
                  <span className="text-subtle">Postgres branch</span>
                </li>
              )}
              {p.redis && (
                <li className="flex items-center gap-2">
                  <MemoryStick className="size-3.5 text-subtle" />
                  <Link href={`/dashboard/redis/${p.redis.db_id}`} className="font-mono hover:text-fg">
                    {p.redis.db_id}
                  </Link>
                  <span className="text-subtle">Redis branch</span>
                </li>
              )}
            </ul>
          )}
          <button
            onClick={() => run("preview-delete", () => codeApi.deletePreview(change.id))}
            disabled={busy !== null}
            className={btn}
          >
            <Trash2 className="size-3.5" /> Delete preview
          </button>
        </div>
      ) : (
        <div className="space-y-3 px-5 py-4">
          <p className="text-[12px] text-subtle">
            A preview runs this change from <span className="font-mono">roll.toml</span> with its own branch of the
            repo&apos;s Postgres and Redis, and scales to zero when idle.
          </p>
          {open && (
            <button onClick={() => run("preview", () => codeApi.createPreview(change.id))} disabled={busy !== null} className={btn}>
              {busy === "preview" ? <span className="shimmer">Creating…</span> : "Create preview"}
            </button>
          )}
        </div>
      )}
    </Panel>
  );
}
