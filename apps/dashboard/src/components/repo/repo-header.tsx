import Link from "next/link";
import { formatCount } from "@/lib/redis-api";
import type { Repo } from "@/lib/code-api";
import { GitHubMark, VisibilityBadge } from "../code/shared";
import { Monogram } from "./avatar";
import { CopyLine } from "./clone-box";
import { FinderButton } from "./finder";
import { LiveStrip } from "./live";
import { RepoTabs } from "./repo-tabs";

/** `owner / repo`, description, the live strip, clone box and section tabs. */
export function RepoHeader({ repo }: { repo: Repo }) {
  return (
    <div className="border-b border-line">
      <div className="mx-auto max-w-6xl px-4 pt-8">
        <div className="flex flex-wrap items-start justify-between gap-x-8 gap-y-5">
          <div className="min-w-0 flex-1">
            <h1 className="flex min-w-0 flex-wrap items-center gap-x-2.5 gap-y-1 text-[22px] leading-tight font-semibold tracking-[-0.045em]">
              <Monogram name={repo.owner} size={26} />
              <Link href={`/${repo.owner}`} className="text-muted transition hover:text-fg">
                {repo.owner}
              </Link>
              <span className="font-normal text-line-2">/</span>
              <Link href={`/${repo.full_name}`} className="truncate transition hover:text-accent">
                {repo.name}
              </Link>
              <VisibilityBadge visibility={repo.visibility} />
              {repo.github && (
                <a
                  href={repo.github.url}
                  target="_blank"
                  rel="noreferrer"
                  className="inline-flex items-center gap-1 rounded-full border border-line px-2 py-0.5 text-[11px] font-normal tracking-normal text-muted transition hover:border-line-2 hover:text-fg"
                  title="Mirrored to GitHub"
                >
                  <GitHubMark className="size-3" /> {repo.github.owner}/{repo.github.repo}
                </a>
              )}
            </h1>
            {repo.description && <p className="mt-2 max-w-2xl text-[14px] leading-relaxed text-muted">{repo.description}</p>}
            <div className="mt-3">
              <LiveStrip />
            </div>
          </div>
          <div className="flex w-full flex-wrap items-center gap-2 sm:w-auto">
            <FinderButton />
            <CopyLine label="https" text={`git clone ${repo.clone_url}`} className="min-w-0 flex-1 sm:w-[23rem] sm:flex-none" />
          </div>
        </div>
        {repo.import?.state === "importing" && (
          <p className="mt-4 text-[12px] text-subtle">
            <span className="shimmer">Importing history from GitHub</span>{" "}
            <span className="font-mono">
              {formatCount(repo.import.commits)}
              {repo.import.total ? ` / ${formatCount(repo.import.total)}` : ""}
            </span>
          </p>
        )}
        <div className="mt-6">
          <RepoTabs
            repo={repo.full_name}
            defaultBranch={repo.default_branch}
            openChanges={repo.stats.open_changes}
            openWork={repo.stats.open_work}
          />
        </div>
      </div>
    </div>
  );
}
