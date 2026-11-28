import { createFileRoute, Link } from "@tanstack/react-router";
import { useEffect, useState } from "react";
import { AlertRow } from "@/components/apps/notifications";
import { appsApi, type OpenAlert } from "@/lib/apps-api";
import { pageHead } from "@/lib/head";

/**
 * An alert message's "Acknowledge" link (`?project=&alert=`): signed in to roll (the link holds no
 * token, so a forwarded one acknowledges nothing), the person sees the alert and acknowledges it
 * as themselves with a click, never by opening the page (link previews fetch it).
 */
export const Route = createFileRoute("/dashboard/apps/alerts")({
  head: () => pageHead({ title: "Alert" }),
  component: function AlertRoute() {
    const { project, alert } = Route.useSearch() as { project?: string; alert?: string };
    const [found, setFound] = useState<OpenAlert | null | undefined>(undefined);
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState<string | null>(null);
    useEffect(() => {
      if (!project || !alert) {
        setError("This link doesn't say which alert.");
        return;
      }
      let live = true;
      appsApi.alerts(project).then(
        (list) => live && setFound(list.find((a) => a.id === alert) ?? null),
        (e: unknown) => live && setError(e instanceof Error ? e.message : String(e)),
      );
      return () => {
        live = false;
      };
    }, [project, alert]);
    const ack = () => {
      if (!project || !found) return;
      setBusy(true);
      appsApi
        .ackAlert(project, found.id)
        .then(setFound, (e: unknown) => setError(e instanceof Error ? e.message : String(e)))
        .finally(() => setBusy(false));
    };
    return (
      <div className="mx-auto flex max-w-xl flex-col gap-4 px-4 py-16">
        <h1 className="text-[22px] font-semibold tracking-[-0.04em]">Acknowledge an alert</h1>
        {error ? (
          <p className="text-[13px] text-danger">{error}</p>
        ) : found === undefined ? (
          <p className="shimmer text-[13px] text-muted">Reading the project's alerts…</p>
        ) : found === null ? (
          <p className="text-[13px] text-muted">It isn't open any more: it cleared, or it's another project's.</p>
        ) : (
          <div className="rounded-lg border border-line bg-panel px-4 py-3">
            <ul>
              <AlertRow alert={found} busy={busy} onAck={ack} />
            </ul>
            <p className="mt-2 text-[12px] text-subtle">
              Acknowledging stops its escalation; it stays open until it clears. {found.environment} ·{" "}
              <Link to="/dashboard/apps/$id" params={{ id: found.service }} search={{ env: found.environment }} className="text-accent">
                Open the service
              </Link>
            </p>
          </div>
        )}
      </div>
    );
  },
});
