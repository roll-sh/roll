import Link from "next/link";
import type { SearchResult } from "@/lib/code-api";
import { groupMatches, highlightRanges, overlayNote, searchLinkRev, splitHighlights } from "@/lib/ai-review";
import { blobHref } from "./paths";

/**
 * Code search results grouped by file, each line linked to the file at the indexed head, with the
 * query's matches highlighted. Works in server and client components.
 */
export function SearchResults({
  repo,
  result,
  query,
  defaultBranch,
  what,
}: {
  repo: string;
  result: SearchResult;
  query: { q: string; regex?: boolean; case?: boolean };
  /** For the note when a revision was searched. */
  defaultBranch?: string;
  /** What the revision is, in that note: "this change". */
  what?: string;
}) {
  const files = groupMatches(result.matches);
  const rev = searchLinkRev(result);
  const note = overlayNote(result, defaultBranch, what);
  if (!files.length) {
    return (
      <div className="space-y-2">
        <p className="rounded-xl border border-line bg-panel px-5 py-6 text-[13px] text-subtle">
          No matches for <span className="font-mono text-muted">{query.q}</span> in {result.files_scanned.toLocaleString()} file
          {result.files_scanned === 1 ? "" : "s"}.
        </p>
        {note && <p className="text-[11px] text-subtle">{note}</p>}
      </div>
    );
  }
  return (
    <div className="space-y-3">
      <p className="text-[12px] text-subtle">
        {result.matches.length.toLocaleString()} match{result.matches.length === 1 ? "" : "es"} in {files.length} file{files.length === 1 ? "" : "s"}
        {result.truncated && " (showing the first ones; narrow the path to see more)"}
      </p>
      {note && <p className="text-[11px] text-subtle">{note}</p>}
      {files.map((f) => (
        <section key={f.path} className="overflow-hidden rounded-xl border border-line bg-panel">
          <h3 className="border-b border-line px-4 py-2 font-mono text-[12px]">
            <Link href={blobHref(repo, rev, f.path)} className="text-fg hover:text-accent">
              {f.path}
            </Link>
            <span className="ml-2 text-subtle">{f.matches.length}</span>
          </h3>
          <ol className="py-1 font-mono text-[12px] leading-6">
            {f.matches.map((m) => (
              <li key={m.line}>
                <Link href={`${blobHref(repo, rev, f.path)}#L${m.line}`} className="flex gap-3 px-4 transition hover:bg-panel-2/60">
                  <span className="w-10 shrink-0 text-right text-subtle tabular-nums select-none">{m.line}</span>
                  <span className="min-w-0 truncate whitespace-pre text-muted">
                    {splitHighlights(m.text, highlightRanges(m.text, query.q, query)).map((s, i) =>
                      s.hit ? (
                        <mark key={i} className="rounded-sm bg-accent/20 text-fg">
                          {s.text}
                        </mark>
                      ) : (
                        <span key={i}>{s.text}</span>
                      ),
                    )}
                  </span>
                </Link>
              </li>
            ))}
          </ol>
        </section>
      ))}
    </div>
  );
}
