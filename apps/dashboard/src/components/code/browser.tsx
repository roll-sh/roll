"use client";

import { ExternalLink, File, FileSymlink, Folder, FolderGit2 } from "lucide-react";
import { useEffect, useState } from "react";
import { formatBytes } from "@/lib/roll-api";
import { codeApi, firstLine, shortId, type ChangesetSummary, type Ref, type Repo, type Tree, type TreeEntry } from "@/lib/code-api";
import { BlameCell, BlameNoteBar, BlameToggle, useBlame } from "../repo/code-lines";
import { ago, input, panel } from "../ui";

/** Files larger than this are linked, not shown. */
const MAX_VIEW_BYTES = 1 << 20;

const entryIcon = (e: TreeEntry) =>
  e.kind === "dir" ? (
    <Folder className="size-4 text-muted" />
  ) : e.kind === "symlink" ? (
    <FileSymlink className="size-4 text-subtle" />
  ) : e.kind === "submodule" ? (
    <FolderGit2 className="size-4 text-subtle" />
  ) : (
    <File className="size-4 text-subtle" />
  );

/** Read-only tree and file browser at any ref or changeset, straight from the read API. */
export function CodeBrowser({ repo }: { repo: Repo }) {
  const [refs, setRefs] = useState<Ref[]>([]);
  const [rev, setRev] = useState(repo.default_branch);
  const [dir, setDir] = useState("");
  const [file, setFile] = useState<string | null>(null);
  const [tree, setTree] = useState<Tree | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    codeApi.refs(repo.full_name).then(setRefs, () => {});
  }, [repo.full_name, repo.head]);

  useEffect(() => {
    if (!repo.head) return;
    let alive = true;
    setTree(null);
    codeApi.tree(repo.full_name, rev, dir).then(
      (t) => alive && (setTree(t), setError(null)),
      (e) => alive && setError(e instanceof Error ? e.message : String(e)),
    );
    return () => {
      alive = false;
    };
  }, [repo.full_name, repo.head, rev, dir]);

  if (!repo.head) {
    return (
      <p className={`${panel} px-5 py-6 text-[13px] text-subtle`}>
        Empty repository. Push with git or land a first change through the API.
      </p>
    );
  }

  const open = (path: string) => {
    setFile(null);
    setDir(path);
  };
  const parts = (file ?? dir).split("/").filter(Boolean);

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center gap-3">
        <select
          value={rev}
          onChange={(e) => {
            setRev(e.target.value);
            setFile(null);
          }}
          className={`${input} max-w-56 font-mono text-[12px]`}
          aria-label="Ref"
        >
          {!refs.some((r) => r.name === rev) && <option value={rev}>{rev}</option>}
          {refs.map((r) => (
            <option key={r.name} value={r.name}>
              {r.name}
            </option>
          ))}
        </select>
        <nav className="flex min-w-0 flex-wrap items-center gap-1 font-mono text-[12px]" aria-label="Path">
          <button onClick={() => open("")} className="text-muted hover:text-fg">
            {repo.name}
          </button>
          {parts.map((p, i) => {
            const path = parts.slice(0, i + 1).join("/");
            const last = i === parts.length - 1;
            return (
              <span key={path} className="flex items-center gap-1">
                <span className="text-subtle">/</span>
                {last ? (
                  <span className="text-fg">{p}</span>
                ) : (
                  <button onClick={() => open(path)} className="text-muted hover:text-fg">
                    {p}
                  </button>
                )}
              </span>
            );
          })}
        </nav>
        {tree && <span className="ml-auto font-mono text-[11px] text-subtle">{shortId(tree.changeset)}</span>}
      </div>

      {error && <p className="text-[13px] text-danger">{error}</p>}

      <div className="grid gap-4 lg:grid-cols-3">
        <div className="min-w-0 lg:col-span-2">
          {file ? (
            <FileView repo={repo} rev={rev} path={file} size={tree?.entries.find((e) => e.path === file)?.size ?? null} />
          ) : (
            <section className={panel}>
              {!tree ? (
                <p className="px-5 py-4 text-[13px] text-subtle">
                  <span className="shimmer">Reading tree…</span>
                </p>
              ) : tree.entries.length === 0 ? (
                <p className="px-5 py-4 text-[13px] text-subtle">Empty directory.</p>
              ) : (
                <ul>
                  {[...tree.entries]
                    .sort((a, b) => Number(b.kind === "dir") - Number(a.kind === "dir") || a.name.localeCompare(b.name))
                    .map((e) => (
                      <li key={e.path} className="border-b border-line last:border-0">
                        <button
                          onClick={() => (e.kind === "dir" ? open(e.path) : e.kind === "file" || e.kind === "symlink" ? setFile(e.path) : undefined)}
                          disabled={e.kind === "submodule"}
                          className="flex w-full items-center gap-2.5 px-5 py-2 text-left text-[13px] transition hover:bg-panel-2/60 disabled:cursor-default disabled:hover:bg-transparent"
                        >
                          {entryIcon(e)}
                          <span className="min-w-0 flex-1 truncate">{e.name}</span>
                          {e.mode === "100755" && <span className="font-mono text-[10px] text-subtle">exec</span>}
                          {e.size != null && e.kind === "file" && (
                            <span className="font-mono text-[11px] text-subtle">{formatBytes(e.size)}</span>
                          )}
                        </button>
                      </li>
                    ))}
                </ul>
              )}
            </section>
          )}
        </div>
        <History repo={repo} rev={rev} path={file ?? dir} />
      </div>
    </div>
  );
}

function FileView({ repo, rev, path, size }: { repo: Repo; rev: string; path: string; size: number | null }) {
  const [text, setText] = useState<string | null>(null);
  const [binary, setBinary] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const url = codeApi.blobUrl(repo.full_name, rev, path);
  const tooBig = size != null && size > MAX_VIEW_BYTES;

  useEffect(() => {
    if (tooBig) return;
    let alive = true;
    setText(null);
    setBinary(false);
    fetch(url, { cache: "no-store" })
      .then(async (res) => {
        if (!res.ok) throw new Error(`${res.status} ${res.statusText}`);
        const buf = new Uint8Array(await res.arrayBuffer());
        if (!alive) return;
        if (buf.subarray(0, 8000).includes(0)) setBinary(true);
        else setText(new TextDecoder().decode(buf));
      })
      .catch((e) => alive && setError(e instanceof Error ? e.message : String(e)));
    return () => {
      alive = false;
    };
  }, [url, tooBig]);

  const lines = text?.replace(/\n$/, "").split("\n") ?? [];
  const blame = useBlame(text != null ? { repo: repo.full_name, rev, path } : undefined, lines.length);
  return (
    <section className={panel}>
      <div className="flex items-center gap-2 border-b border-line px-4 py-2.5">
        <span className="min-w-0 flex-1 truncate font-mono text-[12px]">{path}</span>
        {text != null && <span className="font-mono text-[11px] text-subtle">{lines.length} lines</span>}
        {size != null && <span className="font-mono text-[11px] text-subtle">{formatBytes(size)}</span>}
        {text != null && <BlameToggle on={blame.on} onToggle={blame.toggle} />}
        <a href={url} target="_blank" rel="noreferrer" className="text-subtle hover:text-fg" aria-label="Raw file">
          <ExternalLink className="size-3.5" />
        </a>
      </div>
      {error ? (
        <p className="px-4 py-3 text-[13px] text-danger">{error}</p>
      ) : tooBig || binary ? (
        <p className="px-4 py-3 text-[13px] text-subtle">
          {binary ? "Binary file" : "Large file"} —{" "}
          <a href={url} target="_blank" rel="noreferrer" className="text-accent hover:underline">
            view raw
          </a>
        </p>
      ) : text == null ? (
        <p className="px-4 py-3 text-[13px] text-subtle">
          <span className="shimmer">Reading file…</span>
        </p>
      ) : (
        <div className="overflow-x-auto">
          <BlameNoteBar blame={blame} />
          <table className="w-full border-collapse font-mono text-[12px] leading-5">
            <tbody>
              {lines.map((l, i) => (
                <tr key={i}>
                  {blame.rows && <BlameCell row={blame.rows[i]} repo={repo.full_name} now={blame.now} first={i === 0} />}
                  <td className="w-12 select-none border-r border-line px-2 text-right text-[11px] text-subtle tabular-nums">{i + 1}</td>
                  <td className="pr-4 pl-3 whitespace-pre">{l}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}

/** The latest changesets that touched the current path. */
function History({ repo, rev, path }: { repo: Repo; rev: string; path: string }) {
  const [items, setItems] = useState<ChangesetSummary[] | null>(null);
  useEffect(() => {
    let alive = true;
    setItems(null);
    codeApi.log(repo.full_name, { rev, path: path || undefined, limit: 12 }).then(
      (p) => alive && setItems(p.items),
      () => alive && setItems([]),
    );
    return () => {
      alive = false;
    };
  }, [repo.full_name, repo.head, rev, path]);

  return (
    <section className={`${panel} min-w-0 self-start`}>
      <div className="border-b border-line px-4 py-2.5">
        <h3 className="text-[13px] font-medium">History</h3>
        <p className="truncate font-mono text-[11px] text-subtle">{path || "/"}</p>
      </div>
      {items === null ? (
        <p className="px-4 py-3 text-[12px] text-subtle">
          <span className="shimmer">Reading history…</span>
        </p>
      ) : items.length === 0 ? (
        <p className="px-4 py-3 text-[12px] text-subtle">No changesets.</p>
      ) : (
        <ul>
          {items.map((c) => (
            <li key={c.id} className="border-b border-line px-4 py-2 last:border-0">
              <p className="truncate text-[12px]">{firstLine(c.message) || "(no message)"}</p>
              <p className="truncate font-mono text-[10px] text-subtle">
                {shortId(c.id)} · {c.author.name} · {ago(c.author.time)}
              </p>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}
