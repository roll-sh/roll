import { ArrowUpRight, ClipboardList, GitPullRequest, X } from "lucide-react";
import { useState } from "react";
import type { ChangeStatus, Landing, Machine, Published, Thread, ThreadChange, WorkRef } from "@/lib/agents-api";
import { parseWorkRef } from "@/lib/agents-services";
import { btn, input, primary } from "../ui";
import { useAgents } from "./use-agents";

export const changeStatusLabel: Record<ChangeStatus, string> = {
  checking: "checking",
  failing: "failing",
  blocked: "blocked",
  ready: "ready",
  queued: "queued",
  landed: "landed",
  conflict: "conflict",
  closed: "closed",
};

/** Accent while roll code is working on it, green when it's good, red when it needs a fix, quiet otherwise. */
export function changeTone(s: ChangeStatus): "busy" | "ok" | "bad" | "quiet" {
  if (s === "checking" || s === "queued") return "busy";
  if (s === "ready" || s === "landed") return "ok";
  if (s === "failing" || s === "conflict") return "bad";
  return "quiet";
}

const toneClass = {
  busy: "border-accent/40 text-accent",
  ok: "border-ok/40 text-ok",
  bad: "border-danger/40 text-danger",
  quiet: "border-line-2 text-muted",
};

/** The thread's roll code change: `#42 ready`, linking to the change page; `detail` in the title. */
export function ChangeChip({ change, className = "" }: { change: ThreadChange; className?: string }) {
  const tone = changeTone(change.status);
  return (
    <a
      data-nodrag
      href={change.url}
      onClick={(e) => e.stopPropagation()}
      title={`${change.repo}#${change.number}${change.detail ? ` · ${change.detail}` : ""}`}
      aria-label={`Change ${change.number}, ${changeStatusLabel[change.status]}`}
      className={`inline-flex min-w-0 shrink-0 items-center gap-1 rounded-full border px-1.5 py-px font-mono text-[10.5px] leading-[16px] hover:bg-panel-2 ${toneClass[tone]}`}
    >
      <GitPullRequest className="size-3 shrink-0" />#{change.number}
      <span className={tone === "busy" ? "shimmer" : ""}>{changeStatusLabel[change.status]}</span>
    </a>
  );
}

/** The work item the thread took: `#38 title`, with a release button when it can be let go. */
export function WorkChip({ work, onRelease, className = "" }: { work: WorkRef; onRelease?: () => void; className?: string }) {
  const [owner, repo] = work.repo.split("/");
  return (
    <span className={`inline-flex min-w-0 items-center gap-1 text-[11px] text-muted ${className}`} title={`${work.repo}#${work.number}: ${work.title}`}>
      <ClipboardList className="size-3 shrink-0 text-subtle" />
      <a data-nodrag href={`/${owner}/${repo}/work`} onClick={(e) => e.stopPropagation()} className="min-w-0 truncate hover:text-fg">
        <span className="font-mono text-subtle">#{work.number}</span> {work.title}
      </a>
      {onRelease && (
        <button data-nodrag onClick={onRelease} aria-label="Let go of the work item" title="Let go of it (the claim stays)" className="shrink-0 text-subtle hover:text-fg">
          <X className="size-3" />
        </button>
      )}
    </span>
  );
}

/** How a `land: true` went, said calmly: a blocked or conflicting land is a result, not an error. */
export function landingNote(l: Landing): { tone: "ok" | "busy" | "quiet" | "bad"; text: string } {
  switch (l.status) {
    case "landed":
      return { tone: "ok", text: "Landed." };
    case "queued":
      return { tone: "busy", text: "In the merge queue: it lands once its batch passes the checks." };
    case "blocked":
      return { tone: "quiet", text: "Not landed yet: the landing rules are waiting on something." };
    case "conflict":
      return { tone: "quiet", text: "Not landed: it conflicts with the default branch. Ask the agent to rebase, then publish again." };
    case "failed":
      return { tone: "bad", text: "Landing didn't go through." };
  }
}

/** Publish the thread's work as a roll code change (and land it): the message, the land toggle, then what happened. */
export function PublishForm({ thread, machine, onDone }: { thread: Thread; machine: Machine; onDone: () => void }) {
  const { client, openThread } = useAgents();
  const [message, setMessage] = useState(thread.title);
  const [land, setLand] = useState(false);
  const [busy, setBusy] = useState(false);
  const [done, setDone] = useState<Published | null>(null);
  const [error, setError] = useState<string | null>(null);
  if (!thread.worktree)
    return (
      <div className="flex flex-wrap items-center gap-2 border-t border-line px-4 py-2.5 text-[12.5px]" data-nodrag role="group" aria-label="Publish">
        <p className="min-w-0 flex-1 basis-60 text-muted">A thread publishes from its own worktree. Fork this one into a worktree (its files come along), then publish the fork.</p>
        <button
          disabled={busy}
          className={primary}
          onClick={async () => {
            setBusy(true);
            setError(null);
            try {
              const r = await client.run<{ type: "created"; id: string }>(machine.id, { type: "thread.fork", thread: thread.id, worktree: true }, "created");
              onDone();
              openThread(r.id);
            } catch (err) {
              setError(err instanceof Error ? err.message : String(err));
            } finally {
              setBusy(false);
            }
          }}
        >
          {busy ? <span className="shimmer">Forking…</span> : "Fork into a worktree"}
        </button>
        <button onClick={onDone} className="text-subtle hover:text-fg" aria-label="Close">
          <X className="size-3.5" />
        </button>
        {error && <p className="w-full text-[12px] text-danger">{error}</p>}
      </div>
    );
  if (done) {
    const note = done.landing ? landingNote(done.landing) : null;
    return (
      <div className="space-y-1.5 border-t border-line px-4 py-2.5 text-[12.5px]" data-nodrag role="status" aria-label="Published">
        <p className="flex flex-wrap items-center gap-x-2 gap-y-1">
          Published <span className="font-mono text-muted">{done.branch}</span>
          <span className="font-mono text-[11px] text-subtle">{done.commit.slice(0, 8)}</span>
          {done.url && done.change !== null && (
            <a href={done.url} className="inline-flex items-center gap-0.5 text-accent hover:underline">
              change #{done.change} <ArrowUpRight className="size-3" />
            </a>
          )}
          <button onClick={onDone} className="ml-auto text-subtle hover:text-fg" aria-label="Close">
            <X className="size-3.5" />
          </button>
        </p>
        {done.landing && note && (
          <div className={`rounded-lg border px-2.5 py-1.5 text-[12px] ${note.tone === "ok" ? "border-ok/30" : note.tone === "busy" ? "border-accent/30" : note.tone === "bad" ? "border-danger/30" : "border-line"}`}>
            <p className={note.tone === "bad" ? "text-danger" : "text-fg"}>{note.text}</p>
            {done.landing.message && <p className="mt-0.5 text-muted">{done.landing.message}</p>}
            {done.landing.blockers.length > 0 && (
              <ul className="mt-1 list-disc space-y-0.5 pl-4 text-muted" aria-label="Blockers">
                {done.landing.blockers.map((b) => (
                  <li key={b}>{b}</li>
                ))}
              </ul>
            )}
          </div>
        )}
      </div>
    );
  }
  return (
    <form
      data-nodrag
      aria-label="Publish"
      className="flex flex-wrap items-center gap-2 border-t border-line px-4 py-2.5"
      onSubmit={async (e) => {
        e.preventDefault();
        setBusy(true);
        setError(null);
        try {
          setDone(await client.run<{ type: "published" } & Published>(machine.id, { type: "thread.publish", thread: thread.id, message: message.trim(), land }, "published"));
        } catch (err) {
          setError(err instanceof Error ? err.message : String(err));
        } finally {
          setBusy(false);
        }
      }}
    >
      <input value={message} onChange={(e) => setMessage(e.target.value)} aria-label="Commit message" className={`${input} min-w-0 flex-1 basis-48`} />
      <label className="flex items-center gap-1.5 text-[12px] text-muted" title="Through the landing rules (the merge queue on a queue repo)">
        <input type="checkbox" checked={land} onChange={(e) => setLand(e.target.checked)} className="accent-[var(--accent)]" /> and land
      </label>
      <button type="submit" disabled={busy || !message.trim()} className={primary}>
        {busy ? <span className="shimmer">Publishing…</span> : "Publish"}
      </button>
      {error && <p className="w-full text-[12px] text-danger">{error}</p>}
    </form>
  );
}

/** Give the thread a work item: the next open one of the project's repo, or one by `owner/repo#N` / `wi_…`. */
export function TakeWorkForm({ thread, repo, onDone }: { thread: Thread; repo: string | null; onDone: () => void }) {
  const { owner, services } = useAgents();
  const [ref, setRef] = useState("");
  const [busy, setBusy] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const take = async (key: string, what: Parameters<NonNullable<typeof services>["takeWork"]>[2]) => {
    if (!services) return;
    setBusy(key);
    setError(null);
    setNote(null);
    try {
      const item = await services.takeWork(owner, thread.id, what);
      if (item) onDone();
      else setNote("repo" in what ? `Nothing open to claim in ${what.repo}.` : "Nothing to take.");
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(null);
    }
  };
  const parsed = parseWorkRef(ref);
  return (
    <form
      data-nodrag
      aria-label="Take a work item"
      className="flex flex-wrap items-center gap-2 border-t border-line px-4 py-2.5"
      onSubmit={(e) => {
        e.preventDefault();
        if (parsed) void take("ref", parsed);
      }}
    >
      {repo && (
        <button type="button" disabled={busy !== null} onClick={() => void take("next", { repo })} className={primary}>
          {busy === "next" ? <span className="shimmer">Claiming…</span> : `Claim the next in ${repo}`}
        </button>
      )}
      <input value={ref} onChange={(e) => setRef(e.target.value)} placeholder={repo ? `or ${repo}#12` : "owner/repo#12, or owner/repo for the next"} aria-label="Work item" className={`${input} min-w-0 flex-1 basis-40 font-mono text-[12px]`} />
      <button type="submit" disabled={busy !== null || !parsed} className={btn}>
        {busy === "ref" ? <span className="shimmer">Taking…</span> : "Take"}
      </button>
      <button type="button" onClick={onDone} className="text-subtle hover:text-fg" aria-label="Close">
        <X className="size-3.5" />
      </button>
      {note && <p className="w-full text-[12px] text-muted">{note}</p>}
      {error && <p className="w-full text-[12px] text-danger">{error}</p>}
    </form>
  );
}
