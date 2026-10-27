"use client";

import { Regex, Search } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { codeApi, type SearchResult } from "@/lib/code-api";
import { validRegex } from "@/lib/ai-review";
import { SearchResults } from "../repo/search-results";
import { input } from "../ui";

/** The search form: query, regex and case toggles, and a path glob. `/` focuses it. */
export function SearchForm({
  initial,
  onSearch,
  action,
  autoFocus,
  rev,
  placeholder = "Search code…",
  slashKey = true,
}: {
  initial: { q?: string; regex?: boolean; case?: boolean; path?: string };
  /** Client-side search (dashboard); without it the form submits as a GET to `action`. */
  onSearch?: (q: { q: string; regex: boolean; case: boolean; path: string }) => void;
  action?: string;
  autoFocus?: boolean;
  /** Search this revision (a change head, say); a GET submit carries it as `rev`. */
  rev?: string;
  placeholder?: string;
  /** Whether `/` focuses this form (only one form on a page should take it). */
  slashKey?: boolean;
}) {
  const [q, setQ] = useState(initial.q ?? "");
  const [regex, setRegex] = useState(!!initial.regex);
  const [caseSensitive, setCase] = useState(!!initial.case);
  const [path, setPath] = useState(initial.path ?? "");
  const ref = useRef<HTMLInputElement>(null);
  const bad = regex && q && !validRegex(q);

  useEffect(() => {
    if (!slashKey) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "/" || e.metaKey || e.ctrlKey || e.altKey) return;
      const el = e.target as HTMLElement | null;
      if (el && (el.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(el.tagName))) return;
      e.preventDefault();
      ref.current?.focus();
      ref.current?.select();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [slashKey]);

  return (
    <form
      action={action}
      method="get"
      role="search"
      onSubmit={(e) => {
        if (bad || !q.trim()) return e.preventDefault();
        if (onSearch) {
          e.preventDefault();
          onSearch({ q: q.trim(), regex, case: caseSensitive, path: path.trim() });
        }
      }}
      className="flex flex-wrap items-center gap-2"
    >
      <div className={`flex min-w-0 flex-1 items-center gap-2 rounded-lg border bg-panel px-2.5 transition focus-within:border-line-2 ${bad ? "border-danger/60" : "border-line"}`}>
        <Search className="size-3.5 shrink-0 text-subtle" />
        <input
          ref={ref}
          data-repo-search
          name="q"
          value={q}
          onChange={(e) => setQ(e.target.value)}
          autoFocus={autoFocus}
          placeholder={placeholder}
          spellCheck={false}
          className="min-w-0 flex-1 bg-transparent py-1.5 font-mono text-[13px] outline-none placeholder:font-sans placeholder:text-subtle"
        />
        <kbd className="hidden rounded border border-line px-1 font-mono text-[10px] text-subtle sm:inline">/</kbd>
        <button
          type="button"
          onClick={() => setCase((c) => !c)}
          aria-pressed={caseSensitive}
          title="Match case"
          className={`rounded px-1 font-mono text-[11px] transition ${caseSensitive ? "bg-panel-2 text-fg" : "text-subtle hover:text-fg"}`}
        >
          Aa
        </button>
        <button
          type="button"
          onClick={() => setRegex((r) => !r)}
          aria-pressed={regex}
          title="Regular expression"
          className={`rounded p-0.5 transition ${regex ? "bg-panel-2 text-fg" : "text-subtle hover:text-fg"}`}
        >
          <Regex className="size-3.5" />
        </button>
      </div>
      <input
        name="path"
        value={path}
        onChange={(e) => setPath(e.target.value)}
        placeholder="path glob"
        spellCheck={false}
        className={`${input} w-40 font-mono text-[12px]`}
      />
      {/* GET submits carry the toggles as fields. */}
      {regex && <input type="hidden" name="regex" value="1" />}
      {caseSensitive && <input type="hidden" name="case" value="1" />}
      {rev && <input type="hidden" name="rev" value={rev} />}
    </form>
  );
}

/**
 * Code search on the dashboard (the Code tab, or one change with `rev`): results open on the public
 * file pages.
 */
export function CodeSearch({
  repo,
  rev,
  defaultBranch,
  placeholder,
}: {
  repo: string;
  rev?: string;
  defaultBranch?: string;
  placeholder?: string;
}) {
  const [query, setQuery] = useState<{ q: string; regex: boolean; case: boolean; path: string } | null>(null);
  const [result, setResult] = useState<SearchResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  useEffect(() => {
    if (!query) return;
    let alive = true;
    setLoading(true);
    codeApi.search(repo, { q: query.q, regex: query.regex, case: query.case, path: query.path || undefined, rev }).then(
      (r) => alive && (setResult(r), setError(null), setLoading(false)),
      (e) => alive && (setError(e instanceof Error ? e.message : String(e)), setResult(null), setLoading(false)),
    );
    return () => {
      alive = false;
    };
  }, [repo, query, rev]);

  return (
    <div className="space-y-3">
      <SearchForm initial={{}} onSearch={setQuery} placeholder={placeholder} slashKey={!rev} />
      {loading && (
        <p className="text-[12px] text-subtle">
          <span className="shimmer">Searching…</span>
        </p>
      )}
      {error && <p className="text-[12px] text-danger">{error}</p>}
      {query && result && !loading && (
        <div className="space-y-3">
          <SearchResults repo={repo} result={result} query={query} defaultBranch={defaultBranch} what={rev ? "this change" : undefined} />
          <button type="button" onClick={() => (setQuery(null), setResult(null))} className="text-[12px] text-subtle hover:text-fg">
            Clear search
          </button>
        </div>
      )}
    </div>
  );
}
