import { RollMark } from "@roll/brand";
import { createFileRoute } from "@tanstack/react-router";
import { useEffect, useState } from "react";
import { appsApi } from "@/lib/apps-api";
import { pageHead } from "@/lib/head";

/**
 * A sign-in-gated environment sends a visitor here (`?service=&env=&to=`): signed in to roll, the
 * page asks for a one-use ticket to `to` as them and goes on with it; the environment's own host
 * trades the ticket for its session.
 */
export const Route = createFileRoute("/dashboard/apps/visit")({
  head: () => pageHead({ title: "Opening" }),
  component: function VisitRoute() {
    const { service, env, to } = Route.useSearch() as { service?: string; env?: string; to?: string };
    const [error, setError] = useState<string | null>(null);
    useEffect(() => {
      if (!service || !env || !to) {
        setError("This link doesn't say where to go.");
        return;
      }
      let live = true;
      appsApi
        .visit(service, env, to)
        .then((r) => {
          if (live && /^https?:\/\//.test(r.url)) window.location.replace(r.url);
        })
        .catch((e: unknown) => live && setError(e instanceof Error ? e.message : String(e)));
      return () => {
        live = false;
      };
    }, [service, env, to]);
    return (
      <div className="grid min-h-[60vh] place-items-center px-4">
        <div className="flex max-w-sm flex-col items-center gap-5 text-center">
          <RollMark state={error ? "error" : "thinking"} size={96} className="text-fg" label={error ? "roll mark, error" : "roll mark, thinking"} />
          {error ? (
            <>
              <p className="text-[15px] font-semibold tracking-[-0.04em]">You can't open this one</p>
              <p className="text-[13px] text-muted">{error}</p>
            </>
          ) : (
            <p className="shimmer text-[13px] text-muted">Signing you in to {to ? new URL(to, "http://x").host : "it"}…</p>
          )}
        </div>
      </div>
    );
  },
});
