"use client";

import { CircleDot, Code2, GitCommitHorizontal, GitPullRequest, Search } from "lucide-react";
import Link from "next/link";
import { usePathname } from "next/navigation";

/** Code · Commits · Changes · Work, with the current section underlined. */
export function RepoTabs({
  repo,
  defaultBranch,
  openChanges,
  openWork,
}: {
  repo: string;
  defaultBranch: string;
  openChanges: number;
  openWork: number;
}) {
  const pathname = usePathname() ?? "";
  const base = `/${repo}`;
  const tabs = [
    { label: "Code", icon: Code2, href: base, match: (p: string) => p === base || /\/(tree|blob)\//.test(p) },
    {
      label: "Commits",
      icon: GitCommitHorizontal,
      href: `${base}/commits/${encodeURIComponent(defaultBranch)}`,
      match: (p: string) => /\/(commits|changeset)\//.test(p),
    },
    { label: "Changes", icon: GitPullRequest, href: `${base}/changes`, count: openChanges, match: (p: string) => p === `${base}/changes` || p.startsWith(`${base}/changes/`) },
    { label: "Work", icon: CircleDot, href: `${base}/work`, count: openWork, match: (p: string) => p === `${base}/work` || p.startsWith(`${base}/work/`) },
    { label: "Search", icon: Search, href: `${base}/search`, match: (p: string) => p === `${base}/search` },
  ];
  return (
    <nav className="-mb-px flex gap-1 overflow-x-auto" aria-label="Repository">
      {tabs.map((t) => {
        const active = t.match(pathname);
        const Icon = t.icon;
        return (
          <Link
            key={t.label}
            href={t.href}
            aria-current={active ? "page" : undefined}
            className={`flex shrink-0 items-center gap-1.5 border-b px-2.5 pt-1 pb-2.5 text-[13px] transition ${
              active ? "border-fg text-fg" : "border-transparent text-muted hover:text-fg"
            }`}
          >
            <Icon className="size-3.5" />
            {t.label}
            {!!t.count && <span className="rounded-full bg-panel-2 px-1.5 font-mono text-[10px] text-muted tabular-nums">{t.count}</span>}
          </Link>
        );
      })}
    </nav>
  );
}
