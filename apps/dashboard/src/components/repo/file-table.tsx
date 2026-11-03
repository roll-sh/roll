import { File, FileSymlink, Folder, FolderGit2 } from "lucide-react";
import Link from "next/link";
import type { ReactNode } from "react";
import { firstLine, shortId, type Tree, type TreeEntry } from "@/lib/code-api";
import { ago } from "@/lib/time";
import { blobHref, changesetHref, treeHref } from "./paths";

function EntryIcon({ e }: { e: TreeEntry }) {
  if (e.kind === "dir") return <Folder className="size-4 shrink-0 text-muted" fill="currentColor" fillOpacity={0.12} />;
  if (e.kind === "symlink") return <FileSymlink className="size-4 shrink-0 text-subtle" />;
  if (e.kind === "submodule") return <FolderGit2 className="size-4 shrink-0 text-subtle" />;
  return <File className="size-4 shrink-0 text-subtle" />;
}

/** The listing of one directory: name, the change that last touched it, and when. */
export function FileTable({ repo, rev, tree, now, header }: { repo: string; rev: string; tree: Tree; now: number; header?: ReactNode }) {
  const entries = [...tree.entries].sort(
    (a, b) => Number(b.kind === "dir") - Number(a.kind === "dir") || a.name.localeCompare(b.name, "en", { numeric: true }),
  );
  const parent = tree.path.includes("/") ? tree.path.slice(0, tree.path.lastIndexOf("/")) : "";
  return (
    <section className="overflow-hidden rounded-xl border border-line bg-panel">
      {header}
      <table className="w-full table-fixed border-collapse text-[13px]">
        <colgroup>
          <col className="w-[38%] sm:w-[32%]" />
          <col />
          <col className="w-24" />
        </colgroup>
        <tbody>
          {tree.path && (
            <tr className="border-t border-line first:border-t-0 hover:bg-panel-2/50">
              <td colSpan={3} className="px-4 py-2">
                <Link href={treeHref(repo, rev, parent)} className="block font-mono text-subtle hover:text-fg">
                  ..
                </Link>
              </td>
            </tr>
          )}
          {entries.map((e) => {
            const href = e.kind === "dir" ? treeHref(repo, rev, e.path) : e.kind === "submodule" ? null : blobHref(repo, rev, e.path);
            return (
              <tr key={e.path} className="group border-t border-line first:border-t-0 hover:bg-panel-2/50">
                <td className="px-4 py-2">
                  <span className="flex min-w-0 items-center gap-2.5">
                    <EntryIcon e={e} />
                    {href ? (
                      <Link href={href} className="truncate hover:text-accent hover:underline">
                        {e.name}
                      </Link>
                    ) : (
                      <span className="truncate text-muted">{e.name}</span>
                    )}
                  </span>
                </td>
                <td className="truncate px-4 py-2 text-muted">
                  {e.last_change && (
                    <Link
                      href={changesetHref(repo, e.last_change.changeset)}
                      className="truncate hover:text-fg"
                      title={`${shortId(e.last_change.changeset)} — ${firstLine(e.last_change.message)}`}
                    >
                      {firstLine(e.last_change.message) || "(no message)"}
                    </Link>
                  )}
                </td>
                <td className="px-4 py-2 text-right text-[12px] whitespace-nowrap text-subtle tabular-nums">
                  {e.last_change ? ago(e.last_change.time, now) : ""}
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </section>
  );
}
