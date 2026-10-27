"use client";

import { HorizonMark } from "@roll/brand";
import { ExternalLink, GitMerge, ShieldAlert, ShieldCheck, ShieldQuestion } from "lucide-react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { useEffect, useState } from "react";
import { formatCount } from "@/lib/redis-api";
import {
  cloneCommand,
  codeApi,
  firstLine,
  formatMergeRate,
  shortGit,
  shortId,
  validRepoName,
  type AuditEntry,
  type CodeInfo,
  type LandPolicy,
  type Queue,
  type Visibility,
  type LandedEvent,
  type Repo,
} from "@/lib/code-api";
import { auditLabel, canWrite, queueStripText, summaryFromEvent, summaryFromStats, type QueueSummary } from "@/lib/queue";
import { CodeShell } from "./shell";
import { LiveChart } from "../queues/live-chart";
import { ago, btn, Confirm, CopyButton, ErrorNote, input, Panel, primary, Stats, useCopy } from "../ui";
import { CodeBrowser } from "./browser";
import { CodeSearch } from "./search";
import { ChangesTab } from "./changes";
import { ConnectAgent } from "./connect-agent";
import { QueueTab } from "./queue";
import { ReviewSettingsPanel } from "./review-settings";
import { LearningsPanel, ReviewerPanel } from "./reviewer-settings";
import { repoCrumb, RepoGate, useRepo } from "./frame";
import { Field, GitHubMark, Segmented, useAction, useRepoEvents, VisibilityBadge, type RatePoint, type Run } from "./shared";
import { CiStatusesPanel, WebhooksPanel } from "./webhooks";
import { WorkTab } from "./work";

const TABS = ["overview", "work", "changes", "queue", "code", "settings"] as const;
type Tab = (typeof TABS)[number];

export function CodeRepo({ fullName }: { fullName: string }) {
  const router = useRouter();
  const { repo, info, offline, missing, refresh } = useRepo(fullName);
  const { busy, error, setError, run } = useAction(refresh);
  const [tab, setTabRaw] = useState<Tab>("overview");
  // Bumped by change/work/check/preview/queue events so open tabs refetch.
  const [tick, setTick] = useState(0);
  const [queueNow, setQueueNow] = useState<QueueSummary | null>(null);
  const events = useRepoEvents(repo?.full_name ?? null, {
    fallbackRate: repo?.stats.merges_per_s,
    onOther: () => setTick((t) => t + 1),
    onQueue: (e) => setQueueNow(summaryFromEvent(e)),
  });

  useEffect(() => {
    const h = window.location.hash.slice(1) as Tab;
    if (TABS.includes(h)) setTabRaw(h);
  }, []);
  const setTab = (t: Tab) => {
    setTabRaw(t);
    setError(null);
    window.history.replaceState(null, "", t === "overview" ? window.location.pathname : `#${t}`);
  };

  return (
    <RepoGate id={fullName} crumb={repoCrumb(fullName)} offline={offline} missing={missing} ready={!!repo && !!info}>
      {() => {
        const r = repo!;
        return (
          <CodeShell crumb={repoCrumb(r.full_name)}>
            <div className="flex flex-wrap items-end justify-between gap-3">
              <div className="min-w-0">
                <h1 className="flex items-center gap-3 text-2xl font-semibold tracking-[-0.04em]">
                  <span className="truncate">
                    <span className="text-muted">{r.owner}</span>
                    <span className="text-subtle"> / </span>
                    {r.name}
                  </span>
                  <VisibilityBadge visibility={r.visibility} />
                </h1>
                {r.description && <p className="mt-1 max-w-2xl text-[13px] text-muted">{r.description}</p>}
                <p className="mt-1 flex flex-wrap items-center gap-x-2 gap-y-1 text-[13px] text-muted">
                  {r.github ? (
                    <a href={r.github.url} target="_blank" rel="noreferrer" className="inline-flex items-center gap-1.5 hover:text-fg">
                      <GitHubMark className="size-3.5" /> {r.github.owner}/{r.github.repo}
                    </a>
                  ) : (
                    <span className="inline-flex items-center gap-1.5">
                      <HorizonMark size={14} interactive={false} /> native
                    </span>
                  )}
                  <span className="text-subtle">·</span>
                  <span className="font-mono text-[12px] text-subtle">{r.id}</span>
                  <span className="text-subtle">·</span>
                  <span className="font-mono text-[12px] text-subtle">
                    {r.default_branch} @ {r.head ? shortId(r.head) : "empty"}
                  </span>
                </p>
              </div>
              <div className="flex flex-col items-end gap-2">
                <Link href={`/${r.full_name}`} className={btn}>
                  <ExternalLink className="size-3.5" /> View on roll.sh
                </Link>
                <p className="font-mono text-[11px] text-subtle">
                  {r.land_policy === "post_merge"
                    ? "post-merge checks"
                    : (queueStripText(queueNow ?? summaryFromStats(r.stats)) ?? "merge queue")}{" "}
                  · created{" "}
                  {ago(r.created_at)}
                </p>
              </div>
            </div>

            <nav className="mt-6 flex gap-1 overflow-x-auto border-b border-line" aria-label="Repository sections">
              {TABS.map((t) => (
                <button
                  key={t}
                  onClick={() => setTab(t)}
                  aria-current={t === tab ? "page" : undefined}
                  className={`-mb-px border-b px-3 py-2 text-[13px] capitalize transition ${
                    t === tab ? "border-fg text-fg" : "border-transparent text-muted hover:text-fg"
                  }`}
                >
                  {t}
                  {t === "work" && r.stats.open_work > 0 && (
                    <span className="ml-1.5 font-mono text-[11px] text-subtle">{r.stats.open_work}</span>
                  )}
                  {t === "changes" && r.stats.open_changes > 0 && (
                    <span className="ml-1.5 font-mono text-[11px] text-subtle">{r.stats.open_changes}</span>
                  )}
                  {t === "queue" && r.land_policy !== "post_merge" && (queueNow?.depth ?? r.stats.queue_depth) > 0 && (
                    <span className="ml-1.5 font-mono text-[11px] text-accent">{queueNow?.depth ?? r.stats.queue_depth}</span>
                  )}
                </button>
              ))}
            </nav>

            <ErrorNote error={error} />

            <div className="mt-6">
              {tab === "overview" && <OverviewTab repo={r} info={info!} feed={events.feed} rate={events.rate} current={events.current} live={events.open} />}
              {tab === "work" && <WorkTab repo={r} tick={tick} busy={busy} run={run} />}
              {tab === "changes" && <ChangesTab repo={r} tick={tick} />}
              {tab === "queue" && <QueueTab repo={r} info={info!} tick={tick} busy={busy} run={run} />}
              {tab === "code" && (
                <div className="space-y-4">
                  {r.head && <CodeSearch repo={r.full_name} />}
                  <CodeBrowser key={r.id} repo={r} />
                </div>
              )}
              {tab === "settings" && (
                <SettingsTab
                  key={r.id}
                  repo={r}
                  info={info!}
                  tick={tick}
                  busy={busy}
                  run={run}
                  onRenamed={(full) => router.replace(`/dashboard/code/${full}#settings`)}
                  onDeleted={() => router.push("/dashboard/code")}
                />
              )}
            </div>
          </CodeShell>
        );
      }}
    </RepoGate>
  );
}

// --- Overview ----------------------------------------------------------------------------------

function OverviewTab({
  repo,
  info,
  feed,
  rate,
  current,
  live,
}: {
  repo: Repo;
  info: CodeInfo;
  feed: LandedEvent[];
  rate: RatePoint[];
  current: number;
  live: boolean;
}) {
  const s = repo.stats;
  return (
    <div className="space-y-4">
      <Stats
        items={[
          { label: "Merges today", value: formatCount(s.merges_today), hint: `${formatCount(s.changesets)} changesets in history` },
          { label: "Landing rate", value: formatMergeRate(current), hint: live ? "live" : "last minute" },
          {
            label: "Queue depth",
            value: formatCount(s.queue_depth),
            hint: s.queue_depth ? (repo.land_policy !== "post_merge" ? "waiting to land" : "waiting for the lander") : "nothing waiting",
          },
          {
            label: "Head",
            value: repo.head ? shortId(repo.head) : "empty",
            hint: repo.head ? `git ${shortGit(repo.head_git)} · gen ${formatCount(repo.generation)}` : "push or land a first change",
          },
        ]}
      />
      {repo.import && repo.import.state !== "done" && <ImportNote repo={repo} />}
      <div className="grid gap-4 lg:grid-cols-5">
        <Panel
          title="Landing"
          className="lg:col-span-3"
          aside={
            <span className="flex items-center gap-1.5 font-mono text-[11px] text-subtle">
              {live && <span className="inline-block size-1.5 rounded-full bg-accent" />}
              {repo.default_branch}
            </span>
          }
        >
          <LiveChart
            points={rate}
            series={[{ key: "merges", label: "merges", tone: "fg", fill: true }]}
            format={formatMergeRate}
            height={96}
            live={live}
            empty="Waiting for landings…"
          />
          <LandingFeed repo={repo} feed={feed} />
        </Panel>
        <div className="space-y-4 lg:col-span-2">
          <ConnectPanel repo={repo} />
          <ConnectAgent owner={repo.owner} info={info} manage={canWrite(info, { owner: repo.owner })} />
          {repo.github && <GithubPanel repo={repo} />}
        </div>
      </div>
    </div>
  );
}

function ImportNote({ repo }: { repo: Repo }) {
  const im = repo.import!;
  if (im.state === "error") {
    return <p className="rounded-lg border border-danger/40 bg-danger/10 px-3 py-2 text-[13px] text-danger">Import failed: {im.error}</p>;
  }
  return (
    <p className="rounded-lg border border-line bg-panel px-3 py-2 text-[13px] text-muted">
      <span className="shimmer">Importing history from GitHub</span>{" "}
      <span className="font-mono text-[12px] text-subtle">
        {formatCount(im.commits)}
        {im.total ? ` / ${formatCount(im.total)}` : ""} commits
      </span>
    </p>
  );
}

function LandingFeed({ repo, feed }: { repo: Repo; feed: LandedEvent[] }) {
  if (!feed.length) {
    return (
      <p className="border-t border-line px-5 py-4 text-[12px] text-subtle">
        Every change that lands on <span className="font-mono">{repo.default_branch}</span> appears here as it happens.
      </p>
    );
  }
  return (
    <ul className="max-h-[26rem] overflow-y-auto border-t border-line">
      {feed.map((e) => (
        <li key={e.changeset} className="rise flex items-center gap-3 border-b border-line px-5 py-2.5 last:border-0">
          <GitMerge className="size-3.5 shrink-0 text-ok" />
          <div className="min-w-0 flex-1">
            <p className="truncate text-[13px]">{firstLine(e.message) || "(no message)"}</p>
            <p className="truncate font-mono text-[11px] text-subtle">
              {shortId(e.changeset)} · git {shortGit(e.git_commit)} · {e.author} · {e.files} file{e.files === 1 ? "" : "s"}
              {e.change && (
                <>
                  {" · "}
                  <Link href={`/dashboard/code/${repo.full_name}/changes/${e.change}`} className="hover:text-fg">
                    {e.change}
                  </Link>
                </>
              )}
            </p>
          </div>
          <span className="shrink-0 text-[11px] text-subtle">{ago(e.at)}</span>
        </li>
      ))}
    </ul>
  );
}

function ConnectPanel({ repo }: { repo: Repo }) {
  const { copied, copy } = useCopy();
  const land = `curl -X POST \\
  ${codeApi.url(repo.full_name)}/land \\
  -H 'content-type: application/json' \\
  -d '{"base":"${repo.default_branch}","message":"Fix typo",
       "files":{"README.md":{"content":"…"}}}'`;
  return (
    <Panel title="Connect">
      <div className="space-y-4 px-5 py-4">
        <div className="space-y-2">
          <p className="text-[12px] text-subtle">Clone</p>
          <div className="flex items-center gap-1 rounded-lg border border-line bg-bg px-2.5 py-2">
            <code className="min-w-0 flex-1 truncate font-mono text-[11px]">{cloneCommand(repo)}</code>
            <CopyButton id="clone" text={cloneCommand(repo)} copied={copied} onCopy={copy} />
          </div>
          <p className="text-[11px] text-subtle">
            Pushing to <span className="font-mono">{repo.default_branch}</span> lands; any other branch opens a change.
          </p>
        </div>
        <div className="space-y-2">
          <p className="text-[12px] text-subtle">Land without a checkout</p>
          <div className="relative rounded-lg border border-line bg-bg">
            <pre className="overflow-x-auto px-3 py-2 pr-9 font-mono text-[11px] leading-5">{land}</pre>
            <span className="absolute top-1.5 right-1.5">
              <CopyButton id="land" text={land} copied={copied} onCopy={copy} />
            </span>
          </div>
          <p className="text-[11px] text-subtle">
            Answers with the landed changeset, or <span className="font-mono">409</span> and the conflicting paths.
          </p>
        </div>
      </div>
    </Panel>
  );
}

function GithubPanel({ repo }: { repo: Repo }) {
  const gh = repo.github!;
  const m = gh.mirror;
  return (
    <Panel
      title={
        <span className="flex items-center gap-2">
          <GitHubMark className="size-4" /> GitHub
        </span>
      }
      aside={
        <a href={gh.url} target="_blank" rel="noreferrer" className="text-subtle hover:text-fg" aria-label="Open on GitHub">
          <ExternalLink className="size-3.5" />
        </a>
      }
    >
      <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-2 px-5 py-4 text-[13px]">
        <dt className="text-subtle">Mirror</dt>
        <dd className="flex items-center gap-2">
          <MirrorDot state={m.state} />
          {m.state === "ok" ? "in sync" : m.state}
          {m.behind > 0 && <span className="font-mono text-[11px] text-subtle">{formatCount(m.behind)} behind</span>}
        </dd>
        <dt className="text-subtle">Last push</dt>
        <dd className="font-mono text-[12px]">{m.last_push_at ? ago(m.last_push_at) : "—"}</dd>
        <dt className="text-subtle">{repo.default_branch}</dt>
        <dd>
          <Protection value={gh.default_branch_protected} />
        </dd>
      </dl>
      {m.error && <p className="border-t border-line px-5 py-3 text-[12px] text-danger">{m.error}</p>}
    </Panel>
  );
}

function MirrorDot({ state }: { state: string }) {
  const cls =
    state === "ok" ? "bg-ok" : state === "error" ? "bg-danger" : state === "pending" ? "bg-accent animate-pulse motion-reduce:animate-none" : "border border-line-2";
  return <span className={`inline-block size-2 rounded-full ${cls}`} />;
}

function Protection({ value }: { value: boolean | null }) {
  if (value === true)
    return (
      <span className="inline-flex items-center gap-1.5">
        <ShieldCheck className="size-3.5 text-ok" /> only roll can push
      </span>
    );
  if (value === false)
    return (
      <span className="inline-flex items-center gap-1.5 text-muted">
        <ShieldAlert className="size-3.5 text-danger" /> unprotected
      </span>
    );
  return (
    <span className="inline-flex items-center gap-1.5 text-subtle">
      <ShieldQuestion className="size-3.5" /> unknown
    </span>
  );
}

// --- Settings ----------------------------------------------------------------------------------

function GeneralSettings({
  repo,
  busy,
  run,
  onRenamed,
}: {
  repo: Repo;
  busy: string | null;
  run: Run;
  onRenamed: (fullName: string) => void;
}) {
  const [name, setName] = useState(repo.name);
  const [description, setDescription] = useState(repo.description ?? "");
  const [visibility, setVisibility] = useState<Visibility>(repo.visibility);
  const valid = validRepoName(name);
  const dirty = name !== repo.name || description !== (repo.description ?? "") || visibility !== repo.visibility;
  return (
    <Panel title="General">
      <form
        className="space-y-4 px-5 py-4"
        onSubmit={async (e) => {
          e.preventDefault();
          if (!valid) return;
          const out = await run("general", () =>
            codeApi.updateRepo(repo.full_name, {
              name: name === repo.name ? undefined : name,
              description,
              visibility,
            }),
          );
          if (out && out.full_name !== repo.full_name) onRenamed(out.full_name);
        }}
      >
        <div className="grid gap-4 sm:grid-cols-2">
          <Field label="Name" hint={name !== repo.name ? `${repo.owner}/${repo.name} will redirect here` : `${repo.owner}/…`}>
            <input
              value={name}
              onChange={(e) => setName(e.target.value)}
              spellCheck={false}
              className={`${input} w-full font-mono text-[12px]`}
            />
          </Field>
          <Field
            label="Visibility"
            hint={visibility === "public" ? "anyone can read it on roll.sh and clone it" : "readable with a token only"}
          >
            <Segmented
              value={visibility}
              options={[
                { label: "public", value: "public" },
                { label: "private", value: "private" },
              ]}
              onChange={setVisibility}
            />
          </Field>
        </div>
        <Field label="Description">
          <input
            value={description}
            onChange={(e) => setDescription(e.target.value)}
            placeholder="One line about this repository"
            className={`${input} w-full`}
          />
        </Field>
        {!valid && <p className="text-[12px] text-danger">Use 1–100 letters, digits, ., - or _ (not ending in .git).</p>}
        <button type="submit" disabled={!dirty || !valid || busy !== null} className={primary}>
          {busy === "general" ? <span className="shimmer">Saving…</span> : "Save"}
        </button>
      </form>
    </Panel>
  );
}

/** `required_checks` is the old name of `queue`; the server still accepts and may echo it. */
const policyOf = (p: string): LandPolicy => (p === "post_merge" ? "post_merge" : "queue");

const POLICY_HINT: Record<LandPolicy, string> = {
  post_merge: "land when it merges cleanly; checks run on main after, and a failure opens a work item (or a revert)",
  queue: "every change is tested in a batch on top of main before it lands; failing changes are ejected",
};

function SettingsTab({
  repo,
  info,
  tick,
  busy,
  run,
  onRenamed,
  onDeleted,
}: {
  repo: Repo;
  info: CodeInfo;
  tick: number;
  busy: string | null;
  run: Run;
  onRenamed: (fullName: string) => void;
  onDeleted: () => void;
}) {
  const [policy, setPolicy] = useState<LandPolicy>(policyOf(repo.land_policy));
  const [checks, setChecks] = useState(repo.required_checks.join(", "));
  const [deleting, setDeleting] = useState(false);
  const parsed = checks
    .split(",")
    .map((c) => c.trim())
    .filter(Boolean);
  const dirty = policy !== policyOf(repo.land_policy) || parsed.join(",") !== repo.required_checks.join(",");

  return (
    <div className="space-y-4">
      <GeneralSettings repo={repo} busy={busy} run={run} onRenamed={onRenamed} />
      <Panel title="Landing">
        <form
          className="space-y-4 px-5 py-4"
          onSubmit={(e) => {
            e.preventDefault();
            run("settings", () => codeApi.updateRepo(repo.full_name, { land_policy: policy, required_checks: parsed }));
          }}
        >
          <div className="grid gap-4 sm:grid-cols-2">
            <Field label="Policy" hint={POLICY_HINT[policy]}>
              <Segmented
                value={policy}
                options={[
                  { label: "post-merge", value: "post_merge" },
                  { label: "merge queue", value: "queue" },
                ]}
                onChange={setPolicy}
              />
            </Field>
            <Field label="Required checks" hint="each change must pass these before it's queued: roll.toml checks or any reported over the API">
              <input
                value={checks}
                onChange={(e) => setChecks(e.target.value)}
                disabled={policy === "post_merge"}
                placeholder="test, lint"
                spellCheck={false}
                className={`${input} w-full font-mono text-[12px] disabled:opacity-40`}
              />
            </Field>
          </div>
          <button type="submit" disabled={!dirty || busy !== null} className={primary}>
            {busy === "settings" ? <span className="shimmer">Saving…</span> : "Save"}
          </button>
        </form>
      </Panel>

      <QueueSettings repo={repo} tick={tick} />
      <ReviewSettingsPanel repo={repo} writer={canWrite(info, repo)} tick={tick} busy={busy} run={run} />
      <ReviewerPanel repo={repo} writer={canWrite(info, repo)} tick={tick} busy={busy} run={run} />
      <LearningsPanel repo={repo} writer={canWrite(info, repo)} tick={tick} busy={busy} run={run} />
      <WebhooksPanel repo={repo} writer={canWrite(info, repo)} busy={busy} run={run} />
      <CiStatusesPanel repo={repo} />
      {canWrite(info, repo) && <AuditLog repo={repo} tick={tick} />}

      <Panel title="GitHub">
        <div className="space-y-3 px-5 py-4 text-[13px]">
          {repo.github ? (
            <>
              <p className="flex items-center gap-2">
                <GitHubMark className="size-3.5" />
                <a href={repo.github.url} target="_blank" rel="noreferrer" className="text-accent hover:underline">
                  {repo.github.owner}/{repo.github.repo}
                </a>
                <span className="font-mono text-[11px] text-subtle">installation {repo.github.installation_id}</span>
              </p>
              <p className="flex items-center gap-2">
                <span className="text-subtle">{repo.default_branch}:</span> <Protection value={repo.github.default_branch_protected} />
              </p>
              {repo.github.default_branch_protected !== true && (
                <p className="max-w-2xl text-[12px] text-muted">
                  roll is the landing authority for this repository and mirrors to GitHub in batches. Protect{" "}
                  <span className="font-mono">{repo.default_branch}</span> on GitHub so only the roll App can push — direct
                  pushes are ingested, but they race the lander and can make a mirror push fail.
                </p>
              )}
              <p className="flex items-center gap-2">
                <span className="text-subtle">Mirror:</span> <MirrorDot state={repo.github.mirror.state} />
                {repo.github.mirror.state === "ok" ? "in sync" : repo.github.mirror.state}
                {repo.github.mirror.last_push_at && (
                  <span className="text-[12px] text-subtle">· pushed {ago(repo.github.mirror.last_push_at)}</span>
                )}
              </p>
              {repo.github.mirror.error && <p className="text-[12px] text-danger">{repo.github.mirror.error}</p>}
            </>
          ) : (
            <p className="text-muted">
              Native repository — not connected to GitHub. Clone it from{" "}
              <span className="font-mono text-[12px] text-fg">{repo.clone_url}</span>.
            </p>
          )}
        </div>
      </Panel>

      <Panel title="Danger zone">
        <div className="px-5 py-4">
          {deleting ? (
            <Confirm
              word={repo.name}
              action="Delete repository"
              busy={busy === "delete"}
              onCancel={() => setDeleting(false)}
              onConfirm={async () => {
                const ok = await run("delete", () => codeApi.deleteRepo(repo.full_name));
                if (ok !== undefined) onDeleted();
              }}
            >
              Deletes <span className="font-mono text-fg">{repo.name}</span>, its history, changes, work items and previews
              from roll.{repo.github ? " The GitHub repository is left as it is." : " This can't be undone."}
            </Confirm>
          ) : (
            <div className="flex flex-wrap items-center justify-between gap-3">
              <p className="text-[13px] text-muted">Delete this repository from roll.</p>
              <button onClick={() => setDeleting(true)} className={btn}>
                Delete repository
              </button>
            </div>
          )}
        </div>
      </Panel>
    </div>
  );
}

/** The `[queue]` and `[main]` tables of roll.toml on the default branch, read-only. */
function QueueSettings({ repo, tick }: { repo: Repo; tick: number }) {
  const [queue, setQueue] = useState<Queue | null>(null);
  useEffect(() => {
    let alive = true;
    codeApi.queue(repo.full_name).then(
      (q) => alive && setQueue(q),
      () => alive && setQueue(null),
    );
    return () => {
      alive = false;
    };
  }, [repo.full_name, tick]);

  const st = queue?.settings;
  const rows: [string, string][] = st
    ? [
        ["[queue] checks", st.checks ? st.checks.join(", ") || "none" : "every [checks.*]"],
        ["[queue] batch_size", String(st.batch_size)],
        ["[queue] timeout", `${st.timeout_s}s`],
        ["[queue] require_change_checks", String(st.require_change_checks)],
        ["change checks", st.change_checks.join(", ") || "none"],
        ["[main] auto_revert", String(st.auto_revert)],
      ]
    : [];
  return (
    <Panel title="roll.toml" aside={<span className="font-mono text-[11px] text-subtle">{repo.default_branch}</span>}>
      <div className="space-y-3 px-5 py-4">
        <p className="text-[12px] text-subtle">
          Queue and main-branch settings always come from <span className="font-mono">roll.toml</span> on the default branch,
          never from the change being landed, so a change can&apos;t loosen its own rules.
        </p>
        {st?.error && (
          <p className="rounded-lg border border-danger/40 bg-danger/10 px-3 py-2 text-[12px] text-danger">
            Couldn&apos;t read it, so the defaults apply: {st.error}
          </p>
        )}
        {st ? (
          <dl className="grid grid-cols-[auto_1fr] gap-x-6 gap-y-1.5 text-[12px]">
            {rows.map(([k, v]) => (
              <div key={k} className="contents">
                <dt className="font-mono text-subtle">{k}</dt>
                <dd className="font-mono">{v}</dd>
              </div>
            ))}
          </dl>
        ) : (
          <p className="text-[12px] text-subtle">
            <span className="shimmer">Reading roll.toml…</span>
          </p>
        )}
      </div>
    </Panel>
  );
}

/** Bypasses, pauses and run approvals: who did what, newest first. Writers only. */
function AuditLog({ repo, tick }: { repo: Repo; tick: number }) {
  const [entries, setEntries] = useState<AuditEntry[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let alive = true;
    codeApi.audit(repo.full_name).then(
      (a) => alive && (setEntries(a), setError(null)),
      (e) => alive && setError(e instanceof Error ? e.message : String(e)),
    );
    return () => {
      alive = false;
    };
  }, [repo.full_name, tick]);

  return (
    <Panel title="Audit log">
      {error ? (
        <p className="px-5 py-4 text-[12px] text-danger">{error}</p>
      ) : !entries ? (
        <p className="px-5 py-4 text-[12px] text-subtle">
          <span className="shimmer">Loading…</span>
        </p>
      ) : entries.length === 0 ? (
        <p className="px-5 py-4 text-[12px] text-subtle">No bypasses, pauses, approvals or settings changes yet.</p>
      ) : (
        <ul className="max-h-80 overflow-y-auto">
          {entries.map((a) => (
            <li key={a.id} className="flex items-center gap-3 border-b border-line px-5 py-2 text-[13px] last:border-0">
              <span className="min-w-0 flex-1 truncate">
                <span className="font-mono text-[12px]">{a.actor}</span> <span className="text-muted">{auditLabel(a.action)}</span>
                {typeof a.detail.change === "string" && <span className="font-mono text-[11px] text-subtle"> · {a.detail.change}</span>}
              </span>
              <span className="shrink-0 text-[11px] text-subtle">{ago(a.at)}</span>
            </li>
          ))}
        </ul>
      )}
    </Panel>
  );
}
