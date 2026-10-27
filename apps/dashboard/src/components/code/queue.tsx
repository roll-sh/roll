"use client";

import { ArrowUpToLine, CircleX, GitMerge, Pause, Play, X } from "lucide-react";
import Link from "next/link";
import { useEffect, useState } from "react";
import { formatCount } from "@/lib/redis-api";
import { codeApi, shortId, type CodeInfo, type Queue, type QueueEntry, type Repo } from "@/lib/code-api";
import { batchSegments, canWrite, formatWait, queueBusy, queueHeadline, queueLanes, waitSeconds } from "@/lib/queue";
import { ago, btn, panel, Panel, Stats } from "../ui";
import { CheckIcon, ChecksSummary, POLL_MS, useNow, type Run } from "./shared";

/**
 * The merge queue: the batch being tested, the two waiting lanes and what left recently. Refetched
 * on every repo event (`tick`) and polled while the page is open.
 */
export function QueueTab({ repo, info, tick, busy, run }: { repo: Repo; info: CodeInfo; tick: number; busy: string | null; run: Run }) {
  const [queue, setQueue] = useState<Queue | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [version, setVersion] = useState(0);
  const now = useNow();
  const writer = canWrite(info, repo);

  useEffect(() => {
    let alive = true;
    let timer: ReturnType<typeof setTimeout>;
    const load = async () => {
      try {
        const q = await codeApi.queue(repo.full_name);
        if (alive) (setQueue(q), setError(null));
      } catch (e) {
        if (alive) setError(e instanceof Error ? e.message : String(e));
      }
      if (alive) timer = setTimeout(load, POLL_MS);
    };
    load();
    return () => {
      alive = false;
      clearTimeout(timer);
    };
  }, [repo.full_name, tick, version]);

  const act = async (key: string, fn: () => Promise<unknown>) => {
    await run(key, fn);
    setVersion((v) => v + 1);
  };

  if (!queue) {
    return error ? (
      <p className="text-[13px] text-danger">{error}</p>
    ) : (
      <p className="text-[13px] text-subtle">
        <span className="shimmer">Loading the queue…</span>
      </p>
    );
  }

  const q = queue;
  const lanes = queueLanes(q);
  const busyNow = queueBusy(q.state);
  const s = q.stats;

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <p className="flex items-center gap-2 text-[13px]">
          <span
            className={`inline-block size-1.5 rounded-full ${
              busyNow ? "bg-accent" : q.paused ? "border border-line-2" : "bg-ok"
            }`}
          />
          {busyNow ? <span className="shimmer">{queueHeadline(q)}</span> : <span className="text-muted">{queueHeadline(q)}</span>}
        </p>
        {writer && q.policy === "queue" && (
          <button
            onClick={() => act("queue-pause", () => (q.paused ? codeApi.resumeQueue(repo.full_name) : codeApi.pauseQueue(repo.full_name)))}
            disabled={busy !== null}
            className={btn}
          >
            {q.paused ? <Play className="size-3.5" /> : <Pause className="size-3.5" />}
            {q.paused ? "Resume" : "Pause"}
          </button>
        )}
      </div>

      {q.policy !== "queue" && (
        <p className="rounded-lg border border-line bg-panel px-3 py-2 text-[13px] text-muted">
          This repository lands post-merge: changes land when they merge cleanly and checks run on{" "}
          <span className="font-mono">{repo.default_branch}</span> afterwards. Turn the merge queue on in Settings to test every
          change before it lands.
        </p>
      )}
      {q.settings.error && (
        <p className="rounded-lg border border-danger/40 bg-danger/10 px-3 py-2 text-[13px] text-danger">
          roll.toml on {repo.default_branch} couldn&apos;t be read, so the defaults apply: {q.settings.error}
        </p>
      )}
      {error && <p className="text-[13px] text-danger">{error}</p>}

      <Stats
        items={[
          { label: "In queue", value: formatCount(s.depth), hint: s.depth ? `${lanes.urgent.length} urgent` : "nothing waiting" },
          { label: "Landed / hour", value: formatCount(s.landed_1h), hint: "through the queue" },
          { label: "Average wait", value: s.landed_1h ? formatWait(s.avg_wait_s) : "—", hint: "enqueue → landed, last hour" },
          {
            label: "Batch size",
            value: `${s.batch_size} / ${s.max_batch_size}`,
            hint: s.batch_size < s.max_batch_size ? "smaller after a red batch" : "adapts to failures",
          },
        ]}
      />

      {q.batch && <BatchPanel queue={q} batch={lanes.batch} repo={repo} />}

      <Panel
        title="Waiting"
        aside={<span className="font-mono text-[11px] text-subtle">urgent first · then in order</span>}
      >
        {lanes.urgent.length + lanes.normal.length === 0 ? (
          <p className="px-5 py-4 text-[12px] text-subtle">
            {q.policy === "queue"
              ? "Nothing waiting. Landing a change, or pushing to the default branch, adds it here."
              : "The queue is off for this repository."}
          </p>
        ) : (
          <div>
            {lanes.urgent.length > 0 && (
              <Lane label="Urgent" entries={lanes.urgent} repo={repo} now={now} writer={writer} busy={busy} act={act} />
            )}
            {lanes.normal.length > 0 && (
              <Lane label="Normal" entries={lanes.normal} repo={repo} now={now} writer={writer} busy={busy} act={act} />
            )}
          </div>
        )}
      </Panel>

      <Panel
        title="Recently"
        aside={
          q.last_green && (
            <span className="font-mono text-[11px] text-subtle">
              last green <span className="text-ok">{shortId(q.last_green)}</span>
            </span>
          )
        }
      >
        {q.recent.length === 0 ? (
          <p className="px-5 py-4 text-[12px] text-subtle">Changes appear here as they land or are ejected.</p>
        ) : (
          <ul>
            {q.recent.map((e) => (
              <RecentRow key={e.id} entry={e} repo={repo} now={now} />
            ))}
          </ul>
        )}
      </Panel>
    </div>
  );
}

/** The batch in flight: its changes as a row of segments (halved while bisecting) and its checks. */
function BatchPanel({ queue, batch, repo }: { queue: Queue; batch: QueueEntry[]; repo: Repo }) {
  const b = queue.batch!;
  const { segments, split } = batchSegments(b, batch);
  return (
    <Panel
      title="In flight"
      aside={
        <span className="flex items-center gap-3 font-mono text-[11px] text-subtle">
          <ChecksSummary checks={b.checks} />
          <span>tip {shortId(b.tip)}</span>
          <span>{ago(b.started_at)}</span>
        </span>
      }
    >
      <div className="space-y-4 px-5 py-4">
        <p className="text-[12px] text-subtle">
          {b.state === "testing" && "Testing the batch rebased on the default branch, once, at its tip."}
          {b.state === "bisecting" &&
            (split != null
              ? `A check failed on the batch. Testing the first ${split} of ${b.changes.length} to find the change that broke it; the ones before it land, it's ejected, the rest keep their places.`
              : "A check failed on the batch. Testing its prefixes to find the change that broke it; the ones before it land, it's ejected, the rest keep their places.")}
          {b.state === "landing" && "Green. Landing the whole batch with one compare-and-swap."}
        </p>
        <ol className="flex flex-wrap items-stretch gap-1.5" aria-label="Changes in the batch, in landing order">
          {segments.map((sg, i) => (
            <li key={sg.change} className="flex items-stretch gap-1.5">
              {split != null && i === split && <span aria-hidden className="w-px self-stretch bg-line-2" />}
              <Link
                href={`/dashboard/code/${repo.full_name}/changes/${sg.number ?? sg.change}`}
                title={sg.title}
                className={`flex max-w-[14rem] min-w-0 items-center gap-1.5 rounded-md border px-2 py-1 text-[12px] transition duration-300 motion-reduce:transition-none ${
                  b.state === "landing" ? "border-ok/50" : "border-accent/50"
                } ${sg.tested ? "" : "opacity-50"} hover:border-line-2`}
              >
                <span className="font-mono text-[11px] text-subtle">#{sg.number ?? "?"}</span>
                <span className="truncate">{sg.title}</span>
              </Link>
            </li>
          ))}
        </ol>
        {b.checks.length > 0 && (
          <ul className={`${panel} bg-bg`}>
            {b.checks.map((ck) => (
              <li key={ck.name} className="flex items-center gap-2.5 border-b border-line px-4 py-2 last:border-0">
                <CheckIcon status={ck.status} />
                <span className="font-mono text-[12px]">{ck.name}</span>
                {ck.summary && <span className="min-w-0 flex-1 truncate text-[11px] text-subtle">{ck.summary}</span>}
              </li>
            ))}
          </ul>
        )}
      </div>
    </Panel>
  );
}

function Lane({
  label,
  entries,
  repo,
  now,
  writer,
  busy,
  act,
}: {
  label: string;
  entries: QueueEntry[];
  repo: Repo;
  now: number;
  writer: boolean;
  busy: string | null;
  act: (key: string, fn: () => Promise<unknown>) => Promise<void>;
}) {
  return (
    <section className="border-b border-line last:border-0">
      <h3 className="px-5 pt-3 pb-1 text-[11px] text-subtle">{label}</h3>
      <ul>
        {entries.map((e) => (
          <li key={e.id} className="rise flex items-center gap-3 px-5 py-2.5">
            <span className="w-8 shrink-0 text-right font-mono text-[12px] text-subtle tabular-nums">{e.position ?? "—"}</span>
            <Link href={`/dashboard/code/${repo.full_name}/changes/${e.number}`} className="min-w-0 flex-1 hover:text-fg">
              <p className="truncate text-[13px]">{e.title}</p>
              <p className="truncate font-mono text-[11px] text-subtle">
                #{e.number} · {e.author}
                {e.enqueued_by && e.enqueued_by !== e.author && ` · queued by ${e.enqueued_by}`}
              </p>
            </Link>
            <span className="shrink-0 font-mono text-[11px] text-subtle tabular-nums" title="waiting for">
              {formatWait(waitSeconds(e, now))}
            </span>
            {writer && (
              <span className="flex shrink-0 items-center gap-1">
                {e.lane !== "urgent" && (
                  <button
                    // Keeps its place in time: it moves ahead of every normal entry, behind urgent ones queued earlier.
                    onClick={() => act(`urgent-${e.change}`, () => codeApi.moveLane(e.change, "urgent"))}
                    disabled={busy !== null}
                    className="p-1 text-subtle transition hover:text-fg disabled:opacity-40"
                    title="Move to the urgent lane"
                    aria-label={`Move #${e.number} to the urgent lane`}
                  >
                    <ArrowUpToLine className="size-3.5" />
                  </button>
                )}
                <button
                  onClick={() => act(`dequeue-${e.change}`, () => codeApi.dequeue(e.change))}
                  disabled={busy !== null}
                  className="p-1 text-subtle transition hover:text-fg disabled:opacity-40"
                  title="Remove from the queue"
                  aria-label={`Remove #${e.number} from the queue`}
                >
                  <X className="size-3.5" />
                </button>
              </span>
            )}
          </li>
        ))}
      </ul>
    </section>
  );
}

/** A change that left: landed, or ejected with why (the first line, then the log tail on demand). */
function RecentRow({ entry: e, repo, now }: { entry: QueueEntry; repo: Repo; now: number }) {
  const ejected = e.state === "ejected";
  const first = e.error?.trim() ?? "";
  const tail = e.output?.trim() ?? "";
  return (
    <li className="border-b border-line px-5 py-2.5 last:border-0">
      <div className="flex items-center gap-3">
        {ejected ? <CircleX className="size-3.5 shrink-0 text-danger" /> : <GitMerge className="size-3.5 shrink-0 text-ok" />}
        <Link href={`/dashboard/code/${repo.full_name}/changes/${e.number}`} className="min-w-0 flex-1 hover:text-fg">
          <p className="truncate text-[13px]">{e.title}</p>
          <p className="truncate font-mono text-[11px] text-subtle">
            #{e.number} · {e.author} · {ejected ? "ejected" : "landed"} after {formatWait(waitSeconds(e, now))}
          </p>
        </Link>
        {e.finished_at && <span className="shrink-0 text-[11px] text-subtle">{ago(e.finished_at)}</span>}
      </div>
      {ejected && first && (
        <details className="group mt-1.5 ml-6">
          <summary className="cursor-pointer list-none text-[12px] text-danger marker:hidden">
            {first}
            {tail && <span className="ml-1.5 text-subtle group-open:hidden">· show output</span>}
          </summary>
          {tail && (
            <pre className="mt-2 max-h-64 overflow-auto rounded-lg border border-line bg-bg px-3 py-2 font-mono text-[11px] leading-5 whitespace-pre-wrap text-muted">
              {tail}
            </pre>
          )}
        </details>
      )}
    </li>
  );
}
