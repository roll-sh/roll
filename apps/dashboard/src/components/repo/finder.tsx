"use client";

import { File, Search } from "lucide-react";
import { useRouter } from "next/navigation";
import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { codeApi, isNotFound } from "@/lib/code-api";

const MAX_RESULTS = 50;
/** Without a paths endpoint, walk at most this many directories. */
const MAX_DIRS = 300;

/** Every file path at `rev`: the paths endpoint, or a bounded walk of the tree API. */
async function loadPaths(repo: string, rev: string): Promise<{ paths: string[]; truncated: boolean }> {
  try {
    const p = await codeApi.paths(repo, rev);
    return { paths: p.paths, truncated: p.truncated };
  } catch (e) {
    if (!isNotFound(e)) throw e;
  }
  const paths: string[] = [];
  const queue = [""];
  let walked = 0;
  while (queue.length && walked < MAX_DIRS) {
    const batch = queue.splice(0, 6);
    walked += batch.length;
    const trees = await Promise.all(batch.map((d) => codeApi.tree(repo, rev, d).catch(() => null)));
    for (const t of trees) {
      for (const e of t?.entries ?? []) {
        if (e.kind === "dir") queue.push(e.path);
        else if (e.kind === "file" || e.kind === "symlink") paths.push(e.path);
      }
    }
  }
  return { paths, truncated: queue.length > 0 };
}

/** Subsequence match, scored for consecutive runs and hits in the file name. */
function score(path: string, q: string): { score: number; hits: number[] } | null {
  const lower = path.toLowerCase();
  const nameStart = lower.lastIndexOf("/") + 1;
  const hits: number[] = [];
  let s = 0;
  let from = 0;
  let prev = -2;
  for (const ch of q) {
    const i = lower.indexOf(ch, from);
    if (i === -1) return null;
    hits.push(i);
    s += i === prev + 1 ? 6 : 1;
    if (i >= nameStart) s += 3;
    if (i === nameStart || lower[i - 1] === "/" || lower[i - 1] === "-" || lower[i - 1] === "_" || lower[i - 1] === ".") s += 4;
    prev = i;
    from = i + 1;
  }
  return { score: s - path.length * 0.02, hits };
}

function Highlighted({ text, hits }: { text: string; hits: number[] }) {
  const set = new Set(hits);
  return (
    <>
      {[...text].map((c, i) =>
        set.has(i) ? (
          <span key={i} className="text-fg">
            {c}
          </span>
        ) : (
          c
        ),
      )}
    </>
  );
}

interface Target {
  rev: string;
  permalink: string | null;
}

const TargetCtx = createContext<(t: Target | null) => void>(() => {});

/**
 * Repo-wide keys: `t` opens a fuzzy file finder over every path at the current revision, `/` code search;
 * `y` swaps the URL for its permalink, pinned to a changeset id. Pages say what "current" is with
 * <KeysTarget>.
 */
export function RepoKeys({ repo, defaultRev, children }: { repo: string; defaultRev: string; children: ReactNode }) {
  const router = useRouter();
  const [open, setOpen] = useState(false);
  const [target, setTarget] = useState<Target | null>(null);
  const rev = target?.rev ?? defaultRev;
  const permalink = target?.permalink ?? null;

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.metaKey || e.ctrlKey || e.altKey) return;
      const el = e.target as HTMLElement | null;
      if (el && (el.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(el.tagName))) return;
      if (e.key === "t") {
        e.preventDefault();
        setOpen(true);
      } else if (e.key === "/") {
        // A search box on the page takes `/` itself; elsewhere it opens code search.
        if (document.querySelector("[data-repo-search]")) return;
        e.preventDefault();
        router.push(`/${repo}/search`);
      } else if (e.key === "y" && permalink) {
        e.preventDefault();
        router.replace(`${permalink}${window.location.search}${window.location.hash}`, { scroll: false });
      }
    };
    const onOpen = () => setOpen(true);
    window.addEventListener("keydown", onKey);
    window.addEventListener("roll:finder", onOpen);
    return () => {
      window.removeEventListener("keydown", onKey);
      window.removeEventListener("roll:finder", onOpen);
    };
  }, [permalink, router, repo]);

  return (
    <TargetCtx.Provider value={setTarget}>
      {children}
      {open && <Finder repo={repo} rev={rev} onClose={() => setOpen(false)} />}
    </TargetCtx.Provider>
  );
}

/** Renders nothing; tells <RepoKeys> which revision this page shows and its permalink. */
export function KeysTarget({ rev, permalink = null }: { rev: string; permalink?: string | null }) {
  const set = useContext(TargetCtx);
  useEffect(() => {
    set({ rev, permalink });
    return () => set(null);
  }, [set, rev, permalink]);
  return null;
}

/** The header's "Go to file" button; opens the finder the hotkeys own. */
export function FinderButton() {
  return (
    <button
      type="button"
      onClick={() => window.dispatchEvent(new Event("roll:finder"))}
      className="flex items-center gap-2 rounded-lg border border-line bg-panel px-2.5 py-1.5 text-[12px] text-subtle transition hover:border-line-2 hover:text-fg"
    >
      <Search className="size-3.5" />
      <span>Go to file</span>
      <kbd className="rounded border border-line px-1.5 font-mono text-[10px]">t</kbd>
    </button>
  );
}

function Finder({ repo, rev, onClose }: { repo: string; rev: string; onClose: () => void }) {
  const router = useRouter();
  const [all, setAll] = useState<{ paths: string[]; truncated: boolean } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [q, setQ] = useState("");
  const [sel, setSel] = useState(0);
  const listRef = useRef<HTMLUListElement>(null);

  useEffect(() => {
    let alive = true;
    loadPaths(repo, rev).then(
      (p) => alive && setAll(p),
      (e) => alive && setError(e instanceof Error ? e.message : String(e)),
    );
    return () => {
      alive = false;
    };
  }, [repo, rev]);

  const results = useMemo(() => {
    if (!all) return [];
    const query = q.trim().toLowerCase().replace(/\s+/g, "");
    if (!query) return all.paths.slice(0, MAX_RESULTS).map((p) => ({ path: p, hits: [] as number[] }));
    const scored: { path: string; hits: number[]; score: number }[] = [];
    for (const p of all.paths) {
      const s = score(p, query);
      if (s) scored.push({ path: p, ...s });
    }
    return scored.sort((a, b) => b.score - a.score).slice(0, MAX_RESULTS);
  }, [all, q]);

  useEffect(() => setSel(0), [q]);
  useEffect(() => {
    listRef.current?.querySelector(`[data-i="${sel}"]`)?.scrollIntoView({ block: "nearest" });
  }, [sel]);

  const go = useCallback(
    (path: string) => {
      onClose();
      router.push(`/${repo}/blob/${encodeURIComponent(rev)}/${path.split("/").map(encodeURIComponent).join("/")}`);
    },
    [onClose, repo, rev, router],
  );

  return (
    <div
      className="fixed inset-0 z-50 flex items-start justify-center bg-black/60 px-4 pt-[14vh] backdrop-blur-sm"
      onMouseDown={(e) => e.target === e.currentTarget && onClose()}
    >
      <div className="rise w-full max-w-xl overflow-hidden rounded-xl border border-line bg-panel">
        <label className="flex items-center gap-2.5 border-b border-line px-4 py-3">
          <Search className="size-4 text-subtle" />
          <input
            autoFocus
            value={q}
            onChange={(e) => setQ(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Escape") onClose();
              else if (e.key === "ArrowDown") {
                e.preventDefault();
                setSel((s) => Math.min(results.length - 1, s + 1));
              } else if (e.key === "ArrowUp") {
                e.preventDefault();
                setSel((s) => Math.max(0, s - 1));
              } else if (e.key === "Enter" && results[sel]) go(results[sel].path);
            }}
            placeholder={`Find a file in ${repo}…`}
            spellCheck={false}
            autoComplete="off"
            className="min-w-0 flex-1 bg-transparent text-[14px] outline-none placeholder:text-subtle"
          />
          <kbd className="rounded border border-line px-1.5 font-mono text-[10px] text-subtle">esc</kbd>
        </label>
        {error ? (
          <p className="px-4 py-3 text-[13px] text-danger">{error}</p>
        ) : !all ? (
          <p className="px-4 py-3 text-[13px] text-subtle">
            <span className="shimmer">Reading the tree…</span>
          </p>
        ) : results.length === 0 ? (
          <p className="px-4 py-3 text-[13px] text-subtle">No files match.</p>
        ) : (
          <ul ref={listRef} className="max-h-[50vh] overflow-y-auto py-1" role="listbox">
            {results.map((r, i) => {
              const slash = r.path.lastIndexOf("/");
              const dir = slash === -1 ? "" : r.path.slice(0, slash + 1);
              const name = r.path.slice(slash + 1);
              return (
                <li key={r.path} data-i={i} role="option" aria-selected={i === sel}>
                  <button
                    type="button"
                    onMouseEnter={() => setSel(i)}
                    onClick={() => go(r.path)}
                    className={`flex w-full items-center gap-2.5 px-4 py-1.5 text-left font-mono text-[12px] ${
                      i === sel ? "bg-panel-2" : ""
                    }`}
                  >
                    <File className="size-3.5 shrink-0 text-subtle" />
                    <span className="min-w-0 truncate text-subtle">
                      <Highlighted text={dir} hits={r.hits} />
                      <span className="text-muted">
                        <Highlighted text={name} hits={r.hits.filter((h) => h > slash).map((h) => h - slash - 1)} />
                      </span>
                    </span>
                  </button>
                </li>
              );
            })}
          </ul>
        )}
        {all?.truncated && (
          <p className="border-t border-line px-4 py-2 text-[11px] text-subtle">Large tree: showing matches from the first part.</p>
        )}
      </div>
    </div>
  );
}
