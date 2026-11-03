import Link from "next/link";
import { treeHref } from "./paths";

/** `repo / dir / dir / file`, every part but the last linked to its tree. */
export function Breadcrumbs({ repo, rev, path }: { repo: string; rev: string; path: string }) {
  const name = repo.split("/")[1];
  const parts = path.split("/").filter(Boolean);
  return (
    <nav className="flex min-w-0 flex-wrap items-center gap-x-1 font-mono text-[13px]" aria-label="Path">
      <Link href={treeHref(repo, rev)} className="font-medium text-accent hover:underline">
        {name}
      </Link>
      {parts.map((p, i) => {
        const last = i === parts.length - 1;
        return (
          <span key={i} className="flex items-center gap-x-1">
            <span className="text-subtle">/</span>
            {last ? (
              <span className="font-medium text-fg">{p}</span>
            ) : (
              <Link href={treeHref(repo, rev, parts.slice(0, i + 1).join("/"))} className="text-accent hover:underline">
                {p}
              </Link>
            )}
          </span>
        );
      })}
    </nav>
  );
}
