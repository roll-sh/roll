import { ArrowUpRight, ExternalLink } from "lucide-react";
import { useState } from "react";
import type { PreviewEnvironment } from "@/lib/code-api";
import type { PreviewKind } from "@/lib/previews-api";
import { PREVIEW_KIND_ICON } from "../workspace/preview-sheet";
import { host, money, STATE_LABEL, StateDot } from "../workspace/previews-common";
import { a11yWords, previewsApi, thanMain } from "@/lib/previews-api";
import { errorText } from "../workspace/previews-common";

/**
 * A change's preview environment (roll previews): every service's URL, the data branches, what
 * it costs, and the way to its project's Previews tab. The change page's panel and its aside.
 */
export function PreviewEnvironmentView({ env, compact = false }: { env: PreviewEnvironment; compact?: boolean }) {
  return (
    <div className={compact ? "space-y-2" : "space-y-3"}>
      <p className="flex items-center gap-2 text-[12px] text-muted">
        <StateDot state={env.state} />
        {STATE_LABEL[env.state] ?? env.state}
        <span className="font-mono text-[11px] text-subtle">{env.slug}</span>
      </p>
      {env.services.length > 0 && (
        <ul className="space-y-1">
          {env.services.map((s) => (
            <li key={s.name} className="flex min-w-0 items-center gap-2 text-[12px]">
              <StateDot state={s.state} />
              <span className="shrink-0 text-muted">{s.name}</span>
              <a href={s.url} target="_blank" rel="noreferrer" className="flex min-w-0 items-center gap-1 font-mono text-[12px] text-accent hover:underline">
                <span className="truncate">{host(s.url)}</span>
                <ExternalLink className="size-3 shrink-0" />
              </a>
            </li>
          ))}
        </ul>
      )}
      {env.resources.length > 0 && (
        <ul className="space-y-1 text-[12px] text-subtle" aria-label="Data branches">
          {env.resources.map((r) => {
            const Icon = PREVIEW_KIND_ICON[r.kind as PreviewKind];
            return (
              <li key={`${r.kind}:${r.parent}`} className="flex min-w-0 items-center gap-2">
                {Icon && <Icon className="size-3.5 shrink-0" strokeWidth={1.5} />}
                <span className="truncate font-mono text-[11.5px]">{r.parent}</span>
                <span>→</span>
                <span className="truncate font-mono text-[11.5px] text-muted">{r.ref ?? "branching…"}</span>
                {r.data === "real" && (
                  <span className="shrink-0 text-soft" title="Its branch holds the parent's data as it is: the preview's code reads it unmasked">
                    unmasked
                  </span>
                )}
              </li>
            );
          })}
        </ul>
      )}
      <p className="font-mono text-[11px] text-subtle tabular-nums">
        {money(env.sleeping_usd_month)}/mo asleep · {money(env.active_usd_hour)}/h active
      </p>
      {env.page_health && (env.page_health.errors > 0 || env.page_health.failed > 0) && (
        <p className="break-words text-[12px] text-danger">
          While its first page loaded: {[env.page_health.errors > 0 ? `${env.page_health.errors} ${env.page_health.errors === 1 ? "error" : "errors"}` : "", env.page_health.failed > 0 ? `${env.page_health.failed} failed ${env.page_health.failed === 1 ? "request" : "requests"}` : ""].filter(Boolean).join(", ")}
          {env.page_health.first_error ? ` (${env.page_health.first_error})` : ""}
          {env.base_change?.errors_added != null && `; ${env.base_change.errors_added === 0 ? "none" : env.base_change.errors_added} new against ${env.base_change.branch}`}
        </p>
      )}
      {env.page_a11y && a11yWords(env.page_a11y).length > 0 && (
        <p className="text-[12px] text-muted">
          Accessibility: {a11yWords(env.page_a11y).join(", ")}
          {env.base_change?.a11y_added != null && ` (${env.base_change.a11y_added === 0 ? "none" : env.base_change.a11y_added} new against ${env.base_change.branch})`}
        </p>
      )}
      {env.page_speed && (
        <p className="font-mono text-[11px] text-subtle tabular-nums">
          load {env.page_speed.load_ms < 1000 ? `${env.page_speed.load_ms} ms` : `${(env.page_speed.load_ms / 1000).toFixed(1)} s`}
          {env.page_speed.before_load_ms != null && env.page_speed.before_load_ms !== env.page_speed.load_ms
            ? ` (${env.page_speed.load_ms > env.page_speed.before_load_ms ? "+" : "−"}${Math.abs(env.page_speed.load_ms - env.page_speed.before_load_ms)} ms since the push before)`
            : ""}
          {env.base_change && thanMain(env.page_speed.load_ms, env.base_change.load_ms, env.base_change.branch) ? ` · ${thanMain(env.page_speed.load_ms, env.base_change.load_ms, env.base_change.branch)}` : ""}
        </p>
      )}
      {env.visual_change && (
        <p className="text-[12px] text-muted">
          {env.visual_change.percent > 0
            ? `The page changed ${Math.max(env.visual_change.percent, 0.1).toFixed(1)}% since ${env.visual_change.since.replace(/^cs_/, "").slice(0, 10)}.`
            : `The page looks the same as at ${env.visual_change.since.replace(/^cs_/, "").slice(0, 10)}.`}
        </p>
      )}
      {env.base_change && (
        <p className="text-[12px] text-muted">
          {env.base_change.percent > 0 ? `Differs ${Math.max(env.base_change.percent, 0.1).toFixed(1)}% from ${env.base_change.branch}.` : `Looks the same as ${env.base_change.branch}.`}
        </p>
      )}
      {env.visual_review && env.visual_review.state !== "unchanged" && <VisualReview previewId={env.id} review={env.visual_review} />}
      {env.expires_at != null && (
        <p className="text-[12px] text-muted">
          It goes {new Date(env.expires_at).toLocaleString(undefined, { weekday: "short", hour: "numeric", minute: "2-digit" })}; keep it from Previews.
        </p>
      )}
      {env.dashboard_url && (
        <a href={env.dashboard_url} className="inline-flex items-center gap-1 text-[12px] text-muted hover:text-fg">
          Open in Previews <ArrowUpRight className="size-3.5" strokeWidth={1.5} />
        </a>
      )}
    </div>
  );
}

/** The change's visual review (`roll/visual`): who accepted it, or the way to. */
function VisualReview({ previewId, review }: { previewId: string; review: NonNullable<PreviewEnvironment["visual_review"]> }) {
  const [by, setBy] = useState<string | null>(review.state === "accepted" ? (review.by ?? "someone") : null);
  if (by) return <p className="text-[12px] text-subtle">Its look was accepted by {by} at this push.</p>;
  return (
    <div className="flex flex-wrap items-center gap-2 text-[12px]">
      <span className="text-soft">
        A visual change to review{review.required ? " before it lands" : ""}: {Math.max(review.percent, 0.1).toFixed(1)}% unlike {review.branch}.
      </span>
      <AcceptVisual previewId={previewId} review={review} onAccepted={setBy} />
    </div>
  );
}

/** Accept a change's visual change as it was looked at (its screenshot's head): the panel's and
 * the "Before it can land" blocker's button. A newer screenshot, or the change's own author, is
 * refused and says so. */
export function AcceptVisual({ previewId, review, onAccepted }: { previewId: string; review: NonNullable<PreviewEnvironment["visual_review"]>; onAccepted?: (by: string) => void }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [done, setDone] = useState(false);
  if (done && !onAccepted) return <span className="text-[12px] text-subtle">Accepted.</span>;
  return (
    <span className="inline-flex flex-wrap items-center gap-2">
      <button
        type="button"
        disabled={busy}
        onClick={async () => {
          setBusy(true);
          setError(null);
          try {
            const p = await previewsApi.acceptVisual(previewId, review.head);
            setDone(true);
            onAccepted?.(p.visual_review?.by ?? "you");
          } catch (e) {
            setError(errorText(e));
          } finally {
            setBusy(false);
          }
        }}
        className="h-6 rounded-md border border-line-2 px-2 text-[11.5px] text-fg hover:bg-white/[0.05] disabled:opacity-40"
      >
        {busy ? "Accepting…" : "Accept"}
      </button>
      {error && <span className="text-[11.5px] text-danger">{error}</span>}
    </span>
  );
}
