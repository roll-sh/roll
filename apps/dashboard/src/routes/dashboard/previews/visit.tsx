import { RollMark } from "@roll/brand";
import { createFileRoute } from "@tanstack/react-router";
import { useEffect, useState } from "react";
import { pageHead } from "@/lib/head";
import { previewsApi } from "@/lib/previews-api";

/**
 * A team-access preview sends a visitor here (`?preview=&to=`): signed in to roll, the page asks
 * for a one-use ticket to `to` as them and goes on with it; the preview's own host trades the
 * ticket for its session (docs/previews.md, "Access").
 */
export const Route = createFileRoute("/dashboard/previews/visit")({
  head: () => pageHead({ title: "Opening" }),
  component: function PreviewVisitRoute() {
    const { preview, to } = Route.useSearch() as { preview?: string; to?: string };
    const [error, setError] = useState<string | null>(null);
    useEffect(() => {
      if (!preview || !to) {
        setError("This link doesn't say where to go.");
        return;
      }
      let live = true;
      previewsApi
        .visit(preview, to)
        .then((r) => {
          if (live && /^https?:\/\//.test(r.url)) window.location.replace(r.url);
        })
        .catch((e: unknown) => live && setError(e instanceof Error ? e.message : String(e)));
      return () => {
        live = false;
      };
    }, [preview, to]);
    return (
      <div className="grid min-h-[60vh] place-items-center px-4">
        <div className="flex max-w-sm flex-col items-center gap-5 text-center">
          <RollMark state={error ? "error" : "thinking"} size={96} className="text-fg" label={error ? "roll mark, error" : "roll mark, thinking"} />
          {error ? (
            <>
              <p className="text-[15px] font-semibold tracking-[-0.04em]">You can't open this preview</p>
              <p className="text-[13px] text-muted">{error}</p>
            </>
          ) : (
            <p className="shimmer text-[13px] text-muted">Signing you in to {to ? new URL(to, "http://x").host : "the preview"}…</p>
          )}
        </div>
      </div>
    );
  },
});
