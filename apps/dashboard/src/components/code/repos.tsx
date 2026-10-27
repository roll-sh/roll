"use client";

import { HorizonMark } from "@roll/brand";
import { ArrowLeft, ChevronRight, ExternalLink, Lock, Plus, Search } from "lucide-react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { useCallback, useEffect, useState, type ReactNode } from "react";
import { formatCount } from "@/lib/redis-api";
import {
  codeApi,
  cloneCommand,
  formatMergeRate,
  shortId,
  type CodeInfo,
  type GithubApp,
  type GithubInstallation,
  type GithubRepo,
  type Repo,
  type RepoInput,
  type RepoSource,
  type Visibility,
} from "@/lib/code-api";
import { CodeShell } from "./shell";
import { btn, Connecting, CopyButton, EmptyPanel, ErrorNote, input, Offline, panel, primary, Stats, useCopy } from "../ui";
import { NameHint, OwnerPicker, useRepoName } from "./names";
import { Field, GitHubMark, POLL_MS, Segmented, useAction, VisibilityBadge } from "./shared";
/** Poll the control plane: code info and every repo. */
function useRepoList() {
  const [info, setInfo] = useState<CodeInfo | null>(null);
  const [repos, setRepos] = useState<Repo[] | null>(null);
  const [offline, setOffline] = useState(false);

  const refresh = useCallback(async () => {
    try {
      const [i, r] = await Promise.all([codeApi.info(), codeApi.repos()]);
      setInfo(i);
      setRepos(r);
      setOffline(false);
    } catch {
      setOffline(true);
    }
  }, []);

  useEffect(() => {
    let alive = true;
    let timer: ReturnType<typeof setTimeout>;
    const loop = async () => {
      await refresh();
      if (alive) timer = setTimeout(loop, POLL_MS);
    };
    loop();
    return () => {
      alive = false;
      clearTimeout(timer);
    };
  }, [refresh]);

  return { info, repos, offline, refresh };
}

export function CodeRepos() {
  const router = useRouter();
  const { info, repos, offline, refresh } = useRepoList();
  const { busy, error, run } = useAction(refresh);
  const [creating, setCreating] = useState(false);

  const create = async (input: RepoInput) => {
    const repo = await run("create", () => codeApi.createRepo(input));
    if (repo) {
      setCreating(false);
      router.push(`/dashboard/code/${repo.full_name}`);
    }
  };

  return (
    <CodeShell
      crumb="code"
      actions={
        <button onClick={() => setCreating((c) => !c)} disabled={offline || !info} className={primary}>
          <Plus className="size-3.5" /> New repository
        </button>
      }
    >
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <h1 className="text-2xl font-semibold tracking-[-0.04em]">Code</h1>
          <p className="mt-1 text-[13px] text-muted">
            Keep the code rolling. Source control for agents — claim work, write through the API, land tens of
            thousands of changes a day, and get a preview for every change.
          </p>
        </div>
        {info && (
          <p className="font-mono text-[11px] text-subtle">
            {info.storage} · {info.git_url}
          </p>
        )}
      </div>

      <ErrorNote error={error} />

      {creating && info && <NewRepo busy={busy === "create"} onCancel={() => setCreating(false)} onCreate={create} />}

      {offline ? (
        <Offline />
      ) : repos === null || info === null ? (
        <Connecting />
      ) : repos.length === 0 ? (
        !creating && (
          <EmptyPanel>
            <h2 className="text-[15px] font-medium">No repositories yet</h2>
            <p className="max-w-md text-[13px] text-muted">
              Create one on roll, or connect a GitHub repository: roll imports its history, lands every change and
              mirrors the result back to GitHub, so agents get the throughput and your team keeps GitHub.
            </p>
            <button onClick={() => setCreating(true)} className={primary}>
              <Plus className="size-3.5" /> New repository
            </button>
          </EmptyPanel>
        )
      ) : (
        <>
          <div className="mt-6">
            <Stats
              items={[
                { label: "Repositories", value: repos.length, hint: `${repos.filter((r) => r.github).length} connected to GitHub` },
                { label: "Merges today", value: formatCount(info.stats.merges_today), hint: "since 00:00 UTC" },
                { label: "Landing rate", value: formatMergeRate(info.stats.merges_per_s), hint: "last minute" },
                {
                  label: "Queue depth",
                  value: formatCount(info.stats.queue_depth),
                  hint: info.stats.queue_depth ? "waiting to land" : "nothing waiting",
                },
              ]}
            />
          </div>
          <RepoList repos={repos} />
        </>
      )}
    </CodeShell>
  );
}

function RepoList({ repos }: { repos: Repo[] }) {
  const { copied, copy } = useCopy();
  return (
    <section className={`${panel} mt-4`}>
      <div className="flex items-center justify-between border-b border-line px-5 py-4">
        <h2 className="text-[15px] font-medium">Repositories</h2>
        <span className="font-mono text-[12px] text-subtle">{repos.length}</span>
      </div>
      <ul>
        {repos.map((r) => (
          <li key={r.id} className="rise group relative border-b border-line last:border-0">
            <Link
              href={`/dashboard/code/${r.full_name}`}
              className="grid grid-cols-[auto_minmax(0,1fr)_auto] items-center gap-x-4 gap-y-2 px-5 py-3.5 transition hover:bg-panel-2/60 md:grid-cols-[auto_minmax(0,1.4fr)_minmax(0,1fr)_minmax(0,0.7fr)_auto]"
            >
              <span className="grid size-6 place-items-center rounded-md border border-line text-muted">
                {r.github ? <GitHubMark className="size-3.5" /> : <HorizonMark size={14} interactive={false} />}
              </span>
              <div className="min-w-0">
                <p className="flex items-center gap-2 truncate text-[13px]">
                  <span className="truncate">
                    <span className="text-muted">{r.owner}/</span>
                    {r.name}
                  </span>
                  <VisibilityBadge visibility={r.visibility} />
                </p>
                <p className="truncate text-[12px] text-subtle">
                  {r.description ?? (
                    <span className="font-mono text-[11px]">
                      {r.default_branch} @ {r.head ? shortId(r.head) : "empty"}
                    </span>
                  )}
                </p>
              </div>
              <div className="hidden min-w-0 font-mono text-[11px] text-subtle md:block">
                {r.import?.state === "importing" ? (
                  <p className="truncate text-muted">
                    <span className="shimmer">importing</span> {formatCount(r.import.commits)}
                    {r.import.total ? ` / ${formatCount(r.import.total)}` : ""} commits
                  </p>
                ) : (
                  <p className="truncate">
                    {formatCount(r.stats.merges_today)} {r.stats.merges_today === 1 ? "merge" : "merges"} today · {formatMergeRate(r.stats.merges_per_s)}
                  </p>
                )}
                <p className="truncate">
                  {r.stats.open_changes} open change{r.stats.open_changes === 1 ? "" : "s"} · {r.stats.open_work} work item
                  {r.stats.open_work === 1 ? "" : "s"}
                </p>
              </div>
              <div className="hidden min-w-0 md:block">
                <code className="block truncate font-mono text-[11px] text-subtle">{r.clone_url}</code>
              </div>
              <ChevronRight className="size-3.5 text-subtle" />
            </Link>
            <span className="absolute top-1/2 right-10 hidden -translate-y-1/2 group-hover:block focus-within:block">
              <CopyButton id={r.id} text={cloneCommand(r)} copied={copied} onCopy={copy} />
            </span>
          </li>
        ))}
      </ul>
      <p className="border-t border-line px-5 py-3 text-[12px] text-subtle">
        Clone with any git client, or skip the checkout: agents read and land through{" "}
        <span className="font-mono text-muted">/v1/code</span>.
      </p>
    </section>
  );
}

// --- New repository ----------------------------------------------------------------------------

type Mode = "native" | "github";
type Create = (input: RepoInput) => void;

function NewRepo({ busy, onCreate, onCancel }: { busy: boolean; onCreate: Create; onCancel: () => void }) {
  const [mode, setMode] = useState<Mode | null>(null);
  return (
    <section className={`${panel} rise mt-6 p-5`}>
      {mode === null ? (
        <>
          <h2 className="text-[15px] font-medium">New repository</h2>
          <p className="mt-1 text-[13px] text-muted">Where should the code live?</p>
          <div className="mt-4 grid gap-3 md:grid-cols-2">
            <OptionCard icon={<GitHubMark className="size-4" />} title="Connect GitHub" onClick={() => setMode("github")}>
              Keep the repository on GitHub. roll imports the history, turns issues into work items and PRs into
              changes, lands at agent speed and pushes the result back to GitHub.
            </OptionCard>
            <OptionCard icon={<HorizonMark size={16} interactive={false} />} title="Create on roll" onClick={() => setMode("native")}>
              A native repository at <span className="font-mono text-[12px]">roll.sh/owner/name</span>. Clone it with
              git, or never check it out — agents read and write through the API.
            </OptionCard>
          </div>
          <div className="mt-4">
            <button type="button" onClick={onCancel} className={btn}>
              Cancel
            </button>
          </div>
        </>
      ) : (
        <>
          <button
            type="button"
            onClick={() => setMode(null)}
            className="mb-4 inline-flex items-center gap-1 text-[12px] text-subtle hover:text-fg"
          >
            <ArrowLeft className="size-3.5" /> Back
          </button>
          {mode === "native" ? (
            <RepoForm
              title={
                <>
                  <HorizonMark size={16} interactive={false} /> Create on roll
                </>
              }
              source="native"
              defaultBranch="main"
              busy={busy}
              onCreate={onCreate}
              onCancel={onCancel}
            />
          ) : (
            <GithubConnect busy={busy} onCreate={onCreate} onCancel={onCancel} />
          )}
        </>
      )}
    </section>
  );
}

function OptionCard({
  icon,
  title,
  onClick,
  children,
}: {
  icon: ReactNode;
  title: string;
  onClick: () => void;
  children: ReactNode;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className="group flex flex-col items-start gap-2 rounded-xl border border-line bg-bg p-5 text-left transition hover:border-line-2"
    >
      <span className="flex items-center gap-2 text-[14px] font-medium">
        <span className="grid size-7 place-items-center rounded-lg border border-line text-fg">{icon}</span>
        {title}
        <ChevronRight className="size-3.5 text-subtle transition group-hover:translate-x-0.5 group-hover:text-fg motion-reduce:transition-none" />
      </span>
      <span className="text-[13px] text-muted">{children}</span>
    </button>
  );
}

/** Owner, name (checked live), description, visibility and default branch — shared by both sources. */
function RepoForm({
  title,
  intro,
  source,
  initialName = "",
  defaultBranch,
  initialVisibility = "public",
  busy,
  submitLabel = "Create repository",
  onCreate,
  onCancel,
  secondary,
}: {
  title: ReactNode;
  intro?: ReactNode;
  source: RepoSource;
  initialName?: string;
  defaultBranch: string;
  initialVisibility?: Visibility;
  busy: boolean;
  submitLabel?: string;
  onCreate: Create;
  onCancel: () => void;
  secondary?: ReactNode;
}) {
  const [owner, setOwner] = useState("");
  const [name, setName] = useState(initialName);
  const [description, setDescription] = useState("");
  const [visibility, setVisibility] = useState<Visibility>(initialVisibility);
  const [branch, setBranch] = useState(defaultBranch);
  const status = useRepoName(owner, name);
  const ok = !!owner && status.state === "available" && branch.trim().length > 0;

  return (
    <form
      className="space-y-4"
      onSubmit={(e) => {
        e.preventDefault();
        if (ok)
          onCreate({
            owner,
            name,
            description: description.trim() || undefined,
            visibility,
            default_branch: branch.trim(),
            source,
          });
      }}
    >
      <h2 className="flex items-center gap-2 text-[15px] font-medium">{title}</h2>
      {intro}
      <div className="grid gap-3 sm:grid-cols-[minmax(0,1fr)_auto_minmax(0,1.4fr)] sm:items-start">
        <OwnerPicker value={owner} onChange={setOwner} />
        <span className="hidden pt-6 text-[15px] text-subtle sm:block">/</span>
        <label className="block min-w-0 space-y-1">
          <span className="block text-[11px] text-subtle">Repository name</span>
          <input
            autoFocus
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="my-app"
            spellCheck={false}
            autoComplete="off"
            className={`${input} w-full font-mono text-[12px]`}
          />
          <NameHint status={status} available={owner ? `roll.sh/${owner}/${name} is yours` : "available"} />
        </label>
      </div>
      <Field label="Description (optional)">
        <input
          value={description}
          onChange={(e) => setDescription(e.target.value)}
          placeholder="One line about this repository"
          className={`${input} w-full`}
        />
      </Field>
      <div className="grid gap-3 sm:grid-cols-2">
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
        <Field label="Default branch">
          <input
            value={branch}
            onChange={(e) => setBranch(e.target.value)}
            spellCheck={false}
            className={`${input} w-full font-mono text-[12px]`}
          />
        </Field>
      </div>
      <div className="flex flex-wrap gap-2">
        <button type="submit" disabled={busy || !ok} className={primary}>
          {busy ? <span className="shimmer">Creating…</span> : submitLabel}
        </button>
        {secondary}
        <button type="button" onClick={onCancel} className={btn}>
          Cancel
        </button>
      </div>
    </form>
  );
}

function GithubConnect({ busy, onCreate, onCancel }: { busy: boolean; onCreate: Create; onCancel: () => void }) {
  const [app, setApp] = useState<GithubApp | null>(null);
  const [installs, setInstalls] = useState<GithubInstallation[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [filter, setFilter] = useState("");
  const [picked, setPicked] = useState<{ install: GithubInstallation; repo: GithubRepo } | null>(null);

  useEffect(() => {
    let alive = true;
    (async () => {
      try {
        const a = await codeApi.github();
        if (!alive) return;
        setApp(a);
        if (a.configured) {
          const i = await codeApi.installations();
          if (alive) setInstalls(i);
        }
      } catch (e) {
        if (alive) setError(e instanceof Error ? e.message : String(e));
      }
    })();
    return () => {
      alive = false;
    };
  }, []);

  const heading = (
    <h2 className="flex items-center gap-2 text-[15px] font-medium">
      <GitHubMark className="size-4" /> Connect GitHub
    </h2>
  );

  if (error) {
    return (
      <div className="space-y-3">
        {heading}
        <ErrorNote error={error} />
      </div>
    );
  }
  if (!app || (app.configured && !installs)) {
    return (
      <div className="space-y-3">
        {heading}
        <p className="text-[13px] text-subtle">
          <span className="shimmer">Asking GitHub which repositories roll can see…</span>
        </p>
      </div>
    );
  }
  if (!app.configured) {
    return (
      <div className="space-y-3">
        {heading}
        <p className="max-w-xl text-[13px] text-muted">
          This rolld has no GitHub App yet. Create a GitHub App with access to contents, issues, pull requests and
          checks, then start rolld with its id, private key and webhook secret —{" "}
          <span className="font-mono text-fg">rolld --help</span> lists the flags. Its webhook URL is{" "}
          <span className="font-mono text-fg">/v1/code/github/webhook</span>.
        </p>
        <button type="button" onClick={onCancel} className={btn}>
          Close
        </button>
      </div>
    );
  }

  if (picked) {
    return (
      <RepoForm
        key={picked.repo.full_name}
        title={
          <>
            <GitHubMark className="size-4" /> Connect {picked.repo.full_name}
          </>
        }
        intro={
          <p className="max-w-2xl text-[13px] text-muted">
            roll imports its history, then becomes the landing authority for{" "}
            <span className="font-mono text-fg">{picked.repo.default_branch}</span> and mirrors every landed commit back
            to GitHub in batches. Recommended: protect <span className="font-mono">{picked.repo.default_branch}</span> on
            GitHub so only the roll App can push.
          </p>
        }
        source={{ github: { installation_id: picked.install.id, owner: picked.repo.owner, repo: picked.repo.name } }}
        initialName={picked.repo.name}
        defaultBranch={picked.repo.default_branch}
        initialVisibility={picked.repo.private ? "private" : "public"}
        busy={busy}
        submitLabel="Connect and import"
        onCreate={onCreate}
        onCancel={onCancel}
        secondary={
          <button type="button" onClick={() => setPicked(null)} className={btn}>
            Pick another
          </button>
        }
      />
    );
  }

  const q = filter.trim().toLowerCase();
  const total = installs!.reduce((s, i) => s + i.repos.length, 0);
  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        {heading}
        {app.install_url && (
          <a href={app.install_url} target="_blank" rel="noreferrer" className={btn}>
            <ExternalLink className="size-3.5" /> Install on another account
          </a>
        )}
      </div>
      {total === 0 ? (
        <p className="max-w-xl text-[13px] text-muted">
          The roll App isn&apos;t installed on any account yet, or it can&apos;t see any repositories.{" "}
          {app.install_url ? (
            <a href={app.install_url} target="_blank" rel="noreferrer" className="text-accent hover:underline">
              Install it on GitHub
            </a>
          ) : (
            "Install it on GitHub"
          )}
          , then come back here.
        </p>
      ) : (
        <>
          <label className="relative block max-w-sm">
            <Search className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-subtle" />
            <input
              autoFocus
              value={filter}
              onChange={(e) => setFilter(e.target.value)}
              placeholder="Filter repositories"
              spellCheck={false}
              className={`${input} w-full pl-8`}
            />
          </label>
          <div className="max-h-96 overflow-y-auto rounded-lg border border-line">
            {installs!.map((inst) => {
              const shown = inst.repos.filter((r) => !q || r.full_name.toLowerCase().includes(q));
              if (!shown.length) return null;
              return (
                <div key={inst.id}>
                  <p className="sticky top-0 border-b border-line bg-panel-2 px-4 py-1.5 font-mono text-[11px] text-subtle">
                    {inst.account} · {inst.account_type.toLowerCase()}
                  </p>
                  <ul>
                    {shown.map((r) => (
                      <li key={r.full_name} className="flex items-center gap-3 border-b border-line px-4 py-2.5 last:border-0">
                        <div className="min-w-0 flex-1">
                          <p className="flex items-center gap-1.5 truncate text-[13px]">
                            {r.full_name}
                            {r.private && <Lock className="size-3 text-subtle" aria-label="private" />}
                          </p>
                          <p className="font-mono text-[11px] text-subtle">{r.default_branch}</p>
                        </div>
                        {r.connected_repo ? (
                          <Link href={`/dashboard/code/${r.connected_repo}`} className="text-[12px] text-subtle hover:text-fg">
                            {r.connected_repo} →
                          </Link>
                        ) : (
                          <button type="button" className={btn} onClick={() => setPicked({ install: inst, repo: r })}>
                            Connect
                          </button>
                        )}
                      </li>
                    ))}
                  </ul>
                </div>
              );
            })}
          </div>
        </>
      )}
      <button type="button" onClick={onCancel} className={btn}>
        Cancel
      </button>
    </div>
  );
}
