"use client";

import { Switch } from "./ui";
import { ArrowUpRight, ClipboardList, GitPullRequest, X } from "lucide-react";
import { useRef, useState } from "react";
import type { ChangeStatus, Landing, Machine, Project, Published, Thread, ThreadChange, WorkRef } from "@/lib/agents-api";
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

/**
 * Whether publishing the thread is its repo's first commit: it works in the folder of a checkout
 * with no commits yet (a worktree needs one), so there's nothing to branch a change from.
 */
export function firstCommit(thread: Thread, project: Project | undefined): boolean {
  return !thread.worktree && !!project?.git && !project.git.head && thread.cwd === project.path;
}

/** What More's publish item says: the first commit, a new change, or more work on its open change. */
export function publishLabel(thread: Thread, project: Project | undefined): string {
  if (firstCommit(thread, project)) return "Publish the first commit";
  const open = thread.change && thread.change.status !== "landed" && thread.change.status !== "closed";
  return open ? `Publish again (#${thread.change!.number})` : "Publish as a change";
}

/**
 * A change's page in the dashboard (`owner/repo`, number), signed in like the rest of it. The
 * record's `url` is the public roll.sh page, which reads anonymously: a private repo's answered
 * 404 there, even to its owner.
 */
export function changePageHref(repo: string, number: number): string {
  return `/dashboard/code/${repo.split("/").map(encodeURIComponent).join("/")}/changes/${number}`;
}

/** The thread's roll code change: `#42 ready`, linking to the change page (a tab of its own, so the agents stay open); `detail` in the title. */
export function ChangeChip({ change, className = "" }: { change: ThreadChange; className?: string }) {
  const tone = changeTone(change.status);
  return (
    <a
      data-nodrag
      href={changePageHref(change.repo, change.number)}
      target="_blank"
      rel="noreferrer"
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

/**
 * What "Ask to rebase" tells the agent about its conflicting change. It names the remote's default
 * branch: the checkout's own copy can be behind (auto-pull refused), and rebasing onto it changed
 * nothing.
 */
export function rebasePrompt(change: Pick<ThreadChange, "number" | "detail">): string {
  return `Change #${change.number} conflicts with the default branch${change.detail ? ` (in ${change.detail})` : ""}. Fetch from origin and rebase this branch onto the default branch as it is on origin (the local copy may be behind), resolve the conflicts, run the checks, and tell me when it's ready to publish again.`;
}

/**
 * Ask the agent to rebase its conflicting change: a message into its conversation (queued behind
 * a running turn), where a conflict was only a red chip and a note saying to ask.
 */
export function AskToRebase({ thread, machine, change }: { thread: Thread; machine: Machine | undefined; change: Pick<ThreadChange, "number" | "detail"> }) {
  const { client, readOnly } = useAgents();
  const [state, setState] = useState<"idle" | "busy" | "sent">("idle");
  const [error, setError] = useState<string | null>(null);
  if (readOnly || !machine) return null;
  const ask = async () => {
    setState("busy");
    setError(null);
    try {
      await client.run(machine.id, { type: "message.send", thread: thread.id, text: rebasePrompt(change) });
      setState("sent");
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setState("idle");
    }
  };
  return (
    <span className="inline-flex shrink-0 items-center gap-1.5" data-nodrag>
      <button
        type="button"
        onClick={() => void ask()}
        disabled={state !== "idle"}
        title={rebasePrompt(change)}
        className="inline-flex h-6 items-center rounded-md border border-line-2 px-2 text-[11.5px] text-soft transition-colors hover:border-line-3 hover:bg-fg/[0.04] hover:text-fg disabled:opacity-60"
      >
        {state === "sent" ? "Asked to rebase" : state === "busy" ? <span className="shimmer">Asking…</span> : "Ask to rebase"}
      </button>
      {error && <span role="alert" className="text-[11.5px] text-danger">{error}</span>}
    </span>
  );
}

/** The work item the thread took: `#38 title`, with a release button when it can be let go. */
export function WorkChip({ work, onRelease, className = "" }: { work: WorkRef; onRelease?: () => void; className?: string }) {
  const [owner, repo] = work.repo.split("/");
  return (
    <span className={`inline-flex min-w-0 items-center gap-1 text-[11px] text-muted ${className}`} title={`${work.repo}#${work.number}: ${work.title}`}>
      <ClipboardList className="size-3 shrink-0 text-subtle" />
      <a data-nodrag href={`/dashboard/code/${encodeURIComponent(owner)}/${encodeURIComponent(repo)}#work`} target="_blank" rel="noreferrer" onClick={(e) => e.stopPropagation()} className="min-w-0 truncate hover:text-fg">
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
/** Whether the landing's own message only says again what its note says ("Queued: it lands once…"). */
function repeats(message: string, note: string): boolean {
  const plain = (t: string) => t.toLowerCase().replace(/^[^:]{1,24}:\s*/, "").replace(/[.\s]+$/, "").trim();
  const [a, b] = [plain(message), plain(note)];
  return !!a && !!b && (a.includes(b) || b.includes(a));
}

export function landingNote(l: Landing): { tone: "ok" | "busy" | "quiet" | "bad"; text: string } {
  switch (l.status) {
    case "landed":
      return { tone: "ok", text: "Landed." };
    case "queued":
      return { tone: "busy", text: "In the merge queue: it lands once its batch passes the checks." };
    case "blocked":
      return { tone: "quiet", text: "Not landed yet: the landing rules are waiting on something." };
    case "conflict":
      return { tone: "quiet", text: "Not landed: it conflicts with the default branch." };
    case "failed":
      return { tone: "bad", text: "Landing didn't go through." };
  }
}

/**
 * Publish the thread's work as a roll code change (and land it): the message, the land toggle, then
 * what happened. A thread in the folder of a repo with no commits yet publishes its first commit
 * instead (onto the default branch, through the landing rules), so there's no toggle.
 */
export function PublishForm({ thread, project, machine, onDone }: { thread: Thread; project?: Project; machine: Machine; onDone: () => void }) {
  const { client, openThread } = useAgents();
  const [message, setMessage] = useState(thread.title);
  const [land, setLand] = useState(false);
  const [busy, setBusy] = useState(false);
  const [done, setDone] = useState<Published | null>(null);
  const [error, setError] = useState<string | null>(null);
  // The commit refreshes the project (it has a head now) before or after the reply: what this
  // publish is stays as it was when it was sent.
  const sent = useRef<boolean | null>(null);
  const first = sent.current ?? firstCommit(thread, project);
  if (!thread.worktree && !first)
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
          {done.change !== null && (done.url || project?.repo) && (
            <a href={project?.repo ? changePageHref(`${project.repo.owner}/${project.repo.repo}`, done.change) : done.url!} target="_blank" rel="noreferrer" className="inline-flex items-center gap-0.5 text-accent hover:underline">
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
            {done.landing.message && !repeats(done.landing.message, note.text) && <p className="mt-0.5 text-muted">{done.landing.message}</p>}
            {done.landing.status === "conflict" && done.change !== null && (
              <p className="mt-1.5">
                <AskToRebase thread={thread} machine={machine} change={{ number: done.change, detail: thread.change?.number === done.change ? thread.change.detail : null }} />
              </p>
            )}
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
        sent.current = first;
        setBusy(true);
        setError(null);
        try {
          setDone(await client.run<{ type: "published" } & Published>(machine.id, { type: "thread.publish", thread: thread.id, message: message.trim(), land: first ? !!project?.repo : land }, "published"));
        } catch (err) {
          setError(err instanceof Error ? err.message : String(err));
        } finally {
          setBusy(false);
        }
      }}
    >
      {first && (
        <p className="w-full text-[12.5px] text-muted">
          {project?.repo ? `${project.repo.owner}/${project.repo.repo}` : project?.name} has no commits yet: publishing makes the thread&apos;s files its first commit.
        </p>
      )}
      <input value={message} onChange={(e) => setMessage(e.target.value)} aria-label="Commit message" className={`${input} min-w-0 flex-1 basis-48`} />
      {!first && (
        <label className="flex items-center gap-1.5 text-[12px] text-muted" title="Through the landing rules (the merge queue on a queue repo)">
          <Switch checked={land} onCheckedChange={setLand} aria-label="and land" /> and land
        </label>
      )}
      <button type="submit" disabled={busy || !message.trim()} className={primary}>
        {busy ? <span className="shimmer">Publishing…</span> : first ? "Publish the first commit" : "Publish"}
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
