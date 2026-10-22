import type { Metadata } from "next";
import { notFound } from "next/navigation";
import { Suspense } from "react";
import { Breadcrumbs } from "@/components/repo/breadcrumbs";
import { joinPath, param } from "@/components/repo/paths";
import { RefPicker } from "@/components/repo/ref-picker";
import { Directory, TableSkeleton } from "@/components/repo/sections";
import { repoMetadata } from "@/lib/code-meta";
import { getRepo } from "@/lib/code-server";

type Props = PageProps<"/[owner]/[repo]/tree/[rev]/[[...path]]">;

export async function generateMetadata({ params }: Props): Promise<Metadata> {
  const { owner, repo, rev, path } = await params;
  const p = joinPath(path);
  return repoMetadata(owner, repo, `${p || repo} at ${param(rev)}`);
}

/** A directory at any ref or changeset. */
export default function TreePage(props: Props) {
  return (
    <Suspense fallback={<TableSkeleton />}>
      <TreePageContent {...props} />
    </Suspense>
  );
}

async function TreePageContent({ params }: Props) {
  const { owner, repo, rev: rawRev, path: parts } = await params;
  const found = await getRepo(owner, repo);
  if (!found || !("repo" in found)) notFound();
  const r = found.repo;
  const rev = param(rawRev);
  const path = joinPath(parts);
  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center gap-3">
        <RefPicker repo={r.full_name} rev={rev} kind="tree" path={path} />
        <Breadcrumbs repo={r.full_name} rev={rev} path={path} />
      </div>
      <Suspense key={`${rev}/${path}`} fallback={<TableSkeleton />}>
        <Directory repo={r} rev={rev} path={path} />
      </Suspense>
    </div>
  );
}
