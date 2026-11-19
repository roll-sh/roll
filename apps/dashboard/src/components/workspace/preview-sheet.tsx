import { Boxes, Check, Copy, Database, ExternalLink, Eye, Inbox, ListChecks, Moon, Package, Pin, RefreshCw, RotateCcw, Search, Smartphone, SquareTerminal, Sun, Trash2, Waves, X, Zap, type LucideIcon } from "lucide-react";
import { useEffect, useState } from "react";
import { appsApi, followShell as followAppsShell } from "@/lib/apps-api";
import { codeApi } from "@/lib/code-api";
import { Confirm } from "@/components/ui";
import { TerminalScreen } from "@/components/terminal-screen";
import { endpointSlower, type EndpointTime, a11yWords, checkSummary, seoWords, duration, followLogs, followShell, openUrl, pageLabel, previewsApi, noteCropUrl, qrUrl, screenshotUrl, resourceId, thanMain, type CaughtMail, type PageCheck, type Preview, type TextDiff, type PreviewActivity, type PreviewVisitor, type PreviewKind, type PreviewShare, type RequestLine, type ServiceMetrics, type ErrorGroup } from "@/lib/previews-api";
import { verdict, type VerdictTarget } from "@/lib/preview-verdict";
import { formatBytes } from "@/lib/roll-api";
import { Head, Label, Rows, Stat, Tabs } from "./inspector";
import { FeedbackForm, replyBody, replyOf, type PreviewFeedback } from "./preview-feedback";
import { errorText, host, money, previewOf, since, STATE_LABEL, StateDot } from "./previews-common";

export const PREVIEW_KIND_ICON: Record<PreviewKind, LucideIcon> = {
  postgres: Database,
  redis: Zap,
  nosql: Boxes,
  stream: Waves,
  bucket: Package,
  queue: Inbox,
  tasks: ListChecks,
  search: Search,
};

const KIND_NAME: Record<PreviewKind, string> = {
  postgres: "Postgres",
  redis: "Redis",
  nosql: "NoSQL",
  stream: "Stream",
  bucket: "Bucket",
  queue: "Queue",
  tasks: "Tasks",
  search: "Search",
};

const small =
  "flex h-7 items-center gap-1.5 rounded-md border border-line-2 bg-raised px-2.5 text-[12px] text-muted inset-shadow-highlight transition-colors hover:border-line-3 hover:text-fg disabled:opacity-40";

const input = "h-7 rounded-md border border-line-2 bg-canvas px-2 font-mono text-[11.5px] outline-none transition-colors focus:border-accent/60";

type Act = <T>(fn: () => Promise<T>) => Promise<T>;

/**
 * One preview in the sheet over the Previews tab: its services and their URLs, its data
 * branches, cost, activity and logs, and what can be done to it.
 */
export function PreviewSheet({
  preview: p,
  names,
  masked,
  act,
  onChange,
  onClose,
  feedback,
  resolve,
  onFeedbackPosted,
}: {
  preview: Preview;
  /** A parent resource's name on the canvas, by `"<kind>:<ref>"`. */
  names: Map<string, string>;
  /** How many columns the project's rules mask, by `"<kind>:<ref>"`. */
  masked?: Map<string, number>;
  act: Act;
  onChange: (p: Preview) => void;
  onClose: () => void;
  /** A note from the preview page's feedback button, to post on the change. */
  feedback?: PreviewFeedback;
  /** A note the preview page's Resolve names (its review id), to resolve on the change. */
  resolve?: string;
  /** It was posted (the link that brought it is spent). */
  onFeedbackPosted?: () => void;
}) {
  const [tab, setTab] = useState<"Overview" | "Activity" | "Logs" | "Shell" | "Requests" | "Errors">("Overview");
  const [busy, setBusy] = useState<string | null>(null);
  // The service whose link is shown as a QR code, to open on a phone.
  const [phoneFor, setPhoneFor] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [deleting, setDeleting] = useState(false);
  const gone = p.desired === "deleted";
  const asleep = p.state === "sleeping";
  const metrics = useServiceMetrics(p);

  const run = async (key: string, fn: () => Promise<Preview>) => {
    setBusy(key);
    setError(null);
    try {
      onChange(await act(fn));
      return true;
    } catch (e) {
      setError(errorText(e));
      return false;
    } finally {
      setBusy(null);
    }
  };

  return (
    <>
      <Head
        tile={
          <span className="grid size-[30px] shrink-0 place-items-center rounded-md border border-line-2 bg-raised inset-shadow-highlight">
            <Eye className="size-3.5" />
          </span>
        }
        title={p.title}
        sub={
          <>
            <StateDot state={p.state} />
            {STATE_LABEL[p.state]} · <span className="truncate font-mono">{previewOf(p) || p.slug}</span>
          </>
        }
        actions={
          p.url && (
            <a
              href={openUrl(p)}
              target="_blank"
              rel="noreferrer"
              className="flex h-6 items-center gap-1 rounded-md border border-line-2 bg-raised px-2 text-[11px] text-muted inset-shadow-highlight hover:border-line-3 hover:text-fg"
            >
              Open <ExternalLink className="size-3" />
            </a>
          )
        }
        onClose={onClose}
      />
      <Tabs tabs={["Overview", "Activity", "Logs", "Shell", "Requests", "Errors"] as const} value={tab} onChange={setTab} />
      <div key={tab} className="rise flex-1 overflow-y-auto p-3 [scrollbar-width:none]">
        {tab === "Overview" ? (
          <>
            {p.error && <p className="mb-3 rounded-lg border border-danger/40 bg-danger/10 px-2.5 py-2 text-[11.5px] break-words whitespace-pre-line text-danger">{p.error}</p>}
            {feedback && (
              <div className="mb-3">
                <FeedbackForm preview={p} feedback={feedback} onPosted={onFeedbackPosted} />
              </div>
            )}
            {resolve && p.change && (
              <div className="mb-3">
                <NoteThread previewId={p.id} change={p.change} review={resolve} onDone={onFeedbackPosted} />
              </div>
            )}
            {p.state === "awaiting_approval" &&
              (p.pull?.fork ? (
                <div className="mb-3 flex items-center gap-2 rounded-lg border border-accent/40 bg-accent/[0.06] px-2.5 py-2 text-[11.5px] text-muted">
                  <span className="min-w-0 flex-1">
                    <a href={p.pull.url || undefined} target="_blank" rel="noreferrer" className="text-accent hover:underline">
                      Pull request #{p.pull.number}
                    </a>{" "}
                    is from a fork: nothing of it runs until a writer approves <span className="font-mono text-fg">{p.head.slice(0, 12)}</span>. Its next push waits again.
                  </span>
                  <button
                    type="button"
                    disabled={busy !== null || gone}
                    onClick={() => run("approve", () => previewsApi.approve(p.id, p.head))}
                    className="h-6 shrink-0 rounded-md border border-line-2 bg-raised px-2 text-[11px] text-fg inset-shadow-highlight hover:border-line-3 disabled:opacity-50"
                  >
                    {busy === "approve" ? "Approving…" : "Approve and run"}
                  </button>
                </div>
              ) : (
                <p className="mb-3 rounded-lg border border-accent/40 bg-accent/[0.06] px-2.5 py-2 text-[11.5px] text-muted">
                  A pull request from a fork: a writer approves its head (<span className="font-mono text-fg">approve-run</span>) before its services run.
                </p>
              ))}
            {p.pull && p.state !== "awaiting_approval" && (
              <p className="mb-3 text-[11.5px] text-muted">
                <a href={p.pull.url || undefined} target="_blank" rel="noreferrer" className="text-accent hover:underline">
                  Pull request #{p.pull.number}
                </a>
                {p.pull.fork && p.pull.approved_by ? ` · from a fork, ${p.head.slice(0, 12)} approved by ${p.pull.approved_by}` : ""}
              </p>
            )}
            {!p.trusted && p.reaches === "roll" && (
              <p className="mb-3 text-[11.5px] text-muted">
                Reaches roll only: a pull request from a fork can't call anything outside roll but the project&apos;s allowed hosts, so a call to an outside API fails here. The project&apos;s rules can open it (A fork&apos;s preview reaches: Anything).
              </p>
            )}
            <VerdictStrip p={p} />
            <span data-section="services" />
            <Label>Services</Label>
            {p.services.length ? (
              <div className="rounded-lg border border-line-2 bg-raised inset-shadow-highlight">
                {p.services.map((s) => (
                  <div key={s.name} className="border-t border-line px-2.5 py-1.5 first:border-t-0">
                    <div className="flex items-center gap-2 text-[12px]">
                      <StateDot state={s.state} />
                      <span className="font-medium">{s.name}</span>
                      <span className="text-[11px] text-subtle">{s.state}</span>
                      {metrics.get(s.name) && <span className="font-mono text-[10.5px] text-subtle">{metricsText(metrics.get(s.name)!)}</span>}
                      {s.deployed && <span className="ml-auto font-mono text-[10.5px] text-subtle">{s.deployed.slice(0, 8)}</span>}
                    </div>
                    <div className="mt-0.5 flex items-center gap-2">
                      <a href={openUrl(s)} target="_blank" rel="noreferrer" className="flex min-w-0 items-center gap-1 font-mono text-[11px] text-accent hover:underline">
                        <span className="truncate">{host(s.url)}</span>
                        <ExternalLink className="size-3 shrink-0" />
                      </a>
                      <button
                        type="button"
                        onClick={() => setPhoneFor((x) => (x === s.name ? null : s.name))}
                        aria-pressed={phoneFor === s.name}
                        aria-label={`Open ${s.name} on a phone`}
                        title="Open it on a phone: scan the code"
                        className="ml-auto shrink-0 text-muted hover:text-fg aria-pressed:text-fg"
                      >
                        <Smartphone className="size-3.5" strokeWidth={1.5} />
                      </button>
                    </div>
                    {phoneFor === s.name && <PhoneCode id={p.id} service={s.name} />}
                    {s.error && <p className="mt-0.5 text-[11px] break-words whitespace-pre-line text-danger">{s.error}</p>}
                  </div>
                ))}
              </div>
            ) : (
              <p className="text-[11.5px] text-subtle">No services: a repo in this project runs here once its roll.toml has a [preview] section.</p>
            )}
            {(p.state === "ready" || p.state === "sleeping") && p.trusted && <Promote preview={p} />}
            {!!p.earlier?.length && (
              <>
                <Label>Earlier pushes</Label>
                <ul className="space-y-1">
                  {p.earlier.map((x) => (
                    <li key={`${x.service}@${x.revision}`} className="flex items-center gap-2 text-[12px]">
                      <span className="text-soft">{x.service}</span>
                      <span className="font-mono text-[10.5px] text-subtle">{x.revision.replace(/^cs_/, "").slice(0, 10)}</span>
                      <a href={openUrl(x)} target="_blank" rel="noreferrer" className="ml-auto flex items-center gap-1 font-mono text-[11px] text-accent hover:underline" title="On the same data, asleep until opened">
                        <span className="truncate">{host(x.url)}</span>
                        <ExternalLink className="size-3 shrink-0" />
                      </a>
                    </li>
                  ))}
                </ul>
              </>
            )}

            <Label>
              <span>Data branches</span>
              {p.resources.length > 1 && !gone && (
                <button disabled={busy !== null} onClick={() => run("reset", () => previewsApi.reset(p.id))} className="text-muted hover:text-fg disabled:opacity-40">
                  {busy === "reset" ? <span className="shimmer">Resetting…</span> : "Reset all"}
                </button>
              )}
            </Label>
            {p.resources.length ? (
              <div className="rounded-lg border border-line-2 bg-raised inset-shadow-highlight">
                {p.resources.map((r) => {
                  const Icon = PREVIEW_KIND_ICON[r.kind];
                  const id = resourceId(r);
                  return (
                    <div key={id} className="flex items-center gap-2 border-t border-line px-2.5 py-1.5 first:border-t-0">
                      <Icon className="size-3.5 shrink-0 text-muted" strokeWidth={1.5} />
                      <div className="min-w-0 flex-1 leading-tight">
                        <div className="flex items-center gap-1.5 text-[11.5px]">
                          <StateDot state={r.state} />
                          <span className="max-w-[50%] shrink-0 truncate">{names.get(id) ?? r.parent}</span>
                          <span className="shrink-0 text-subtle">→</span>
                          <span className="min-w-0 truncate font-mono text-[11px]">{r.ref ?? "—"}</span>
                        </div>
                        <div className="mt-0.5 flex flex-wrap gap-x-1 font-mono text-[10.5px] text-subtle">
                          <span className="whitespace-nowrap">
                            {KIND_NAME[r.kind]} · {r.mode === "schema" ? "schema only" : "branch"} · {formatBytes(r.bytes)}
                          </span>
                          {!!masked?.get(id) && <span className="whitespace-nowrap">· masked ({masked.get(id)})</span>}
                          {r.data === "real" && (
                            <span className="whitespace-nowrap text-soft" title="Its branch holds the parent's data as it is: the preview's code reads it unmasked">
                              · production data, unmasked
                            </span>
                          )}
                        </div>
                        {r.error && <p className="mt-0.5 text-[11px] break-words whitespace-pre-line text-danger">{r.error}</p>}
                      </div>
                      {!gone && r.ref && <CopyConnection previewId={p.id} kind={r.kind} parent={r.parent} name={names.get(id) ?? r.parent} />}
                      {!gone && (
                        <button
                          disabled={busy !== null}
                          onClick={() => run(`reset:${id}`, () => previewsApi.reset(p.id, [id]))}
                          aria-label={`Reset ${names.get(id) ?? r.parent}`}
                          title="Branch it again from its parent"
                          className="grid size-6 shrink-0 place-items-center rounded-md text-muted hover:bg-white/[0.06] hover:text-fg disabled:opacity-40"
                        >
                          <RotateCcw className={`size-3.5 ${busy === `reset:${id}` ? "animate-spin [animation-direction:reverse] motion-reduce:animate-none" : ""}`} />
                        </button>
                      )}
                    </div>
                  );
                })}
              </div>
            ) : (
              <p className="text-[11.5px] text-subtle">No data branches.</p>
            )}

            {p.release && (
              <p className="mt-1.5 font-mono text-[11px] text-subtle tabular-nums" title="The project's release (migrations), run on this preview's data before its services">
                release {duration(p.release.ms)}
                {p.release.before_ms != null ? ` · ${thanMain(p.release.ms, p.release.before_ms, "the push before") ?? "as before"}` : ""}
              </p>
            )}
            {p.schema_changes && p.schema_changes.length > 0 && (
              <>
                <span data-section="schema" />
                <Label>Schema changes</Label>
                {p.schema_risks && p.schema_risks.length > 0 && (
                  <ul className="mb-1.5 space-y-0.5 rounded-lg border border-danger/40 bg-danger/10 px-2.5 py-1.5 text-[11.5px] text-danger">
                    {p.schema_risks.map((r, i) => (
                      <li key={i} className="break-words">
                        {r}
                      </li>
                    ))}
                  </ul>
                )}
                <pre className="max-h-48 overflow-auto rounded-lg border border-line-2 bg-raised px-2.5 py-1.5 font-mono text-[11px] leading-relaxed whitespace-pre-wrap [overflow-wrap:anywhere] inset-shadow-highlight">
                  {p.schema_changes.map((l, i) => (
                    <div key={i} className={l.startsWith("- ") ? "text-danger" : l.startsWith("+ ") ? "text-fg" : "text-subtle"}>
                      {l}
                    </div>
                  ))}
                </pre>
              </>
            )}

            <span data-section="visual" />
            {p.screenshot_at ? <Screenshot id={p.id} at={p.screenshot_at} change={p.visual_change} phone={p.phone_screenshot} base={p.base_change} pages={p.pages} /> : null}
            {p.visual_review && p.visual_review.state !== "unchanged" && (
              <div className="mt-1.5 flex items-center gap-2 text-[11.5px]">
                {p.visual_review.state === "accepted" ? (
                  <span className="text-subtle">Accepted by {p.visual_review.by ?? "someone"} at this push.</span>
                ) : (
                  <>
                    <span className="text-soft">
                      Differs from {p.visual_review.branch} by {Math.max(p.visual_review.percent, 0.1).toFixed(1)}%: waiting for a review{p.visual_review.required ? " before it can land" : ""}.
                    </span>
                    <button
                      type="button"
                      disabled={busy !== null || gone}
                      onClick={() => run("accept", () => previewsApi.acceptVisual(p.id, p.visual_review?.head))}
                      className="ml-auto h-6 shrink-0 rounded-md border border-line-2 px-2 text-[11.5px] text-fg hover:bg-white/[0.05] disabled:opacity-40"
                    >
                      {busy === "accept" ? "Accepting…" : p.visual_review.pages.length > 1 ? "Accept all" : "Accept"}
                    </button>
                  </>
                )}
              </div>
            )}
            {p.visual_review && p.visual_review.state === "changed" && p.visual_review.pages.length > 1 && (
              <ul className="mt-1 space-y-0.5 text-[11.5px]" aria-label="Pages to review">
                {p.visual_review.pages.map((pg) => (
                  <li key={pg.path} className="flex min-w-0 items-center gap-2">
                    <span className="truncate font-mono text-[11px] text-muted">{pageLabel(pg.path)}</span>
                    <span className="shrink-0 font-mono text-[11px] tabular-nums text-subtle">{Math.max(pg.percent, 0.1).toFixed(1)}%</span>
                    {pg.unstable && <span className="shrink-0 text-subtle" title="Two captures a second apart differ: the difference may be only that">moves by itself</span>}
                    {pg.by ? (
                      <span className="ml-auto shrink-0 text-subtle">accepted by {pg.by}</span>
                    ) : (
                      <button
                        type="button"
                        disabled={busy !== null || gone}
                        onClick={() => run(`accept:${pg.path}`, () => previewsApi.acceptVisual(p.id, p.visual_review?.head, pg.path))}
                        className="ml-auto h-6 shrink-0 rounded-md border border-line-2 px-2 text-[11.5px] text-fg hover:bg-white/[0.05] disabled:opacity-40"
                      >
                        {busy === `accept:${pg.path}` ? "Accepting…" : "Accept"}
                      </button>
                    )}
                  </li>
                ))}
              </ul>
            )}
            {p.visual_review?.state === "changed" && p.visual_review.pages.some((pg) => pg.unstable && !pg.by) && (
              <p className="mt-1 text-[11.5px] text-subtle">
                {p.visual_review.pages
                  .filter((pg) => pg.unstable && !pg.by)
                  .map((pg) => pageLabel(pg.path))
                  .join(", ")}{" "}
                moves by itself (two captures a second apart differ), so the difference may be only that: Hide in the rules can blank what moves.
              </p>
            )}
            {/* What else differs from main, after the picture and its review: words, API, endpoints. */}
            {p.base_change?.text && <TextAgainst diff={p.base_change.text} branch={p.base_change.branch} />}
            <span data-section="api" />
            {p.base_change?.api && <TextAgainst diff={p.base_change.api} branch={p.base_change.branch} what="api" />}
            {p.base_change?.api_breaking && <ApiBreaking items={p.base_change.api_breaking} />}
            {p.base_change?.api_unread && <p className="mt-1.5 text-[11.5px] text-danger">Its OpenAPI document couldn't be read (gone, broken, or too big or slow): its API isn't compared with {p.base_change.branch}'s.</p>}
            <span data-section="endpoints" />
            {p.base_change?.endpoints && <Endpoints items={p.base_change.endpoints} branch={p.base_change.branch} />}
            <span data-section="checks" />
            {(p.page_health || p.page_links || p.page_a11y || p.page_speed) && <ChecksHeading p={p} />}
            {p.page_health && (p.page_health.errors > 0 || p.page_health.failed > 0) && (
              <div className="mt-1.5 space-y-0.5 text-[11.5px] text-danger">
                {p.page_health.errors > 0 && (
                  <p className="break-words">
                    {p.page_health.errors} {p.page_health.errors === 1 ? "error" : "errors"} while it loaded{p.page_health.first_error ? `: ${p.page_health.first_error}` : ""}
                  </p>
                )}
                {p.page_health.failed > 0 && (
                  <p className="break-words">
                    {p.page_health.failed} failed {p.page_health.failed === 1 ? "request" : "requests"}
                    {p.page_health.first_failed ? `: ${p.page_health.first_failed}` : ""}
                  </p>
                )}
                {p.base_change?.errors_added != null && (
                  <p className="text-muted">{p.base_change.errors_added === 0 ? `None of them new against ${p.base_change.branch}.` : `${p.base_change.errors_added} new against ${p.base_change.branch}.`}</p>
                )}
              </div>
            )}
            {p.page_links && p.page_links.broken.length > 0 && (
              <p className="mt-1.5 text-[11.5px] break-words text-danger">
                {p.page_links.broken.length} broken {p.page_links.broken.length === 1 ? "link" : "links"} of {p.page_links.checked}:{" "}
                <span className="font-mono text-[11px]">{p.page_links.broken.map((b) => `${b.path} ${b.status || "no answer"}`).join(", ")}</span>
                {p.base_change?.links_added != null && ` (${p.base_change.links_added === 0 ? "none" : p.base_change.links_added} new against ${p.base_change.branch})`}
              </p>
            )}
            {p.page_a11y?.seo && seoWords(p.page_a11y.seo).length > 0 && (
              <p className="mt-1.5 text-[11.5px] text-muted">
                SEO: {seoWords(p.page_a11y.seo).join(", ")}
                {p.base_change?.seo_added != null && ` (${p.base_change.seo_added === 0 ? "none" : p.base_change.seo_added} new against ${p.base_change.branch})`}
              </p>
            )}
            {p.page_a11y && a11yWords(p.page_a11y).length > 0 && (
              <p className="mt-1.5 text-[11.5px] text-muted" title={p.page_a11y.issues.map((i) => i.first).filter(Boolean).join("\n")}>
                Accessibility: {a11yWords(p.page_a11y).join(", ")}
                {p.base_change?.a11y_added != null && ` (${p.base_change.a11y_added === 0 ? "none" : p.base_change.a11y_added} new against ${p.base_change.branch})`}
              </p>
            )}
            {p.page_speed && (
              <p className="mt-1.5 font-mono text-[11px] text-subtle tabular-nums" title="Timed by the browser that took the screenshot">
                load {duration(p.page_speed.now.load_ms)}
                {p.page_speed.before && p.page_speed.before.load_ms !== p.page_speed.now.load_ms
                  ? ` (${p.page_speed.now.load_ms > p.page_speed.before.load_ms ? "+" : "−"}${duration(Math.abs(p.page_speed.now.load_ms - p.page_speed.before.load_ms))})`
                  : ""}
                {p.page_speed.now.fcp_ms != null ? ` · first paint ${duration(p.page_speed.now.fcp_ms)}` : ""} · {formatBytes(p.page_speed.now.bytes)} · first byte{" "}
                {duration(p.page_speed.now.ttfb_ms)}
                {p.base_change && thanMain(p.page_speed.now.load_ms, p.base_change.load_ms, p.base_change.branch) ? ` · ${thanMain(p.page_speed.now.load_ms, p.base_change.load_ms, p.base_change.branch)}` : ""}
              </p>
            )}
            {p.page_speed && (p.page_speed.now.lcp_ms != null || p.page_speed.now.cls != null) && (
              <p className="mt-0.5 font-mono text-[11px] text-subtle tabular-nums" title="Core Web Vitals, measured by the browser that took the screenshot">
                {[
                  p.page_speed.now.lcp_ms != null ? `LCP ${duration(p.page_speed.now.lcp_ms)}` : "",
                  p.page_speed.now.lcp_ms != null && p.base_change ? thanMain(p.page_speed.now.lcp_ms, p.base_change.lcp_ms, p.base_change.branch) : "",
                  p.page_speed.now.cls != null ? `CLS ${p.page_speed.now.cls.toFixed(2)}` : "",
                ]
                  .filter(Boolean)
                  .join(" · ")}
              </p>
            )}

            <Mail id={p.id} />
            <Label>Cost</Label>
            <div className="grid grid-cols-3 gap-2">
              <Stat label="Asleep" value={money(p.cost.sleeping_usd_month)} note="a month" />
              <Stat label="Active" value={money(p.cost.active_usd_hour)} note="an hour" />
              <Stat label="So far" value={money(p.cost.month_to_date_usd)} note="this month" />
            </div>

            <Vars id={p.id} />
            <Shares id={p.id} />

            <Label>Details</Label>
            <Rows
              rows={[
                ["repo", p.repo],
                ...(p.branch ? ([["branch", p.branch]] as [string, string][]) : []),
                ["head", p.head.slice(0, 12)],
                ...(p.runs ? ([["runs", `${p.runs.slice(0, 12)} (later pushes touched only ignored paths)`]] as [string, string][]) : []),
                ["slug", p.slug],
                ["last visit", since(p.last_active_at)],
                ...(p.last_reset_at ? ([["last reset", since(p.last_reset_at)]] as [string, string][]) : []),
                [
                  "expires",
                  p.kept_until && p.kept_until > Date.now()
                    ? `kept until ${new Date(p.kept_until).toLocaleDateString()}`
                    : p.goes_at
                      ? `goes ${new Date(p.goes_at).toLocaleString(undefined, { weekday: "short", hour: "numeric", minute: "2-digit" })}`
                      : p.expires_at
                      ? new Date(p.expires_at).toLocaleDateString()
                      : "with its change",
                ],
                ["created by", p.created_by],
                ...(p.reported_by ? ([["reported by", `${p.reported_by}: the pull request hears from that project's preview, not this one`]] as [string, string][]) : []),
              ]}
            />
          </>
        ) : tab === "Activity" ? (
          <ActivityList id={p.id} updated={p.updated_at} />
        ) : tab === "Logs" ? (
          <Logs preview={p} />
        ) : tab === "Shell" ? (
          <Shell preview={p} />
        ) : tab === "Requests" ? (
          <Requests preview={p} />
        ) : (
          <Errors preview={p} />
        )}
      </div>
      <div className="border-t border-line px-3 py-2">
        {error && <p className="mb-2 rounded-lg border border-danger/40 bg-danger/10 px-2.5 py-1.5 text-[11.5px] break-words text-danger">{error}</p>}
        {deleting ? (
          <Confirm
            word={p.slug}
            action="Delete preview"
            busy={busy === "delete"}
            onCancel={() => setDeleting(false)}
            onConfirm={async () => {
              if (await run("delete", () => previewsApi.remove(p.id))) setDeleting(false);
            }}
          >
            Its services stop, and every data branch and its secrets environment are deleted. The parents stay as they are.
          </Confirm>
        ) : gone ? (
          <p className="text-[11.5px] text-subtle">
            <span className="shimmer">Deleting its services and branches…</span>
          </p>
        ) : (
          <div className="flex flex-wrap items-center gap-1.5">
            {asleep ? (
              <button className={small} disabled={busy !== null} onClick={() => run("wake", () => previewsApi.wake(p.id))}>
                <Sun className="size-3.5" /> Wake
              </button>
            ) : (
              <button className={small} disabled={busy !== null || !p.services.length} onClick={() => run("sleep", () => previewsApi.sleep(p.id))}>
                <Moon className="size-3.5" /> Sleep
              </button>
            )}
            <button className={small} disabled={busy !== null} onClick={() => run("refresh", () => previewsApi.refresh(p.id))} title="Redeploy its services now">
              <RefreshCw className="size-3.5" /> Refresh
            </button>
            {p.resources.length > 0 && (
              <button className={small} disabled={busy !== null} onClick={() => run("reset", () => previewsApi.reset(p.id))} title="Branch every resource again from its parent">
                <RotateCcw className="size-3.5" /> Reset data
              </button>
            )}
            {p.kept_until && p.kept_until > Date.now() ? (
              <button className={small} disabled={busy !== null} onClick={() => run("keep", () => previewsApi.keep(p.id, 0))} title="Let the project's rules decide when it goes again">
                <Pin className="size-3.5" /> Release
              </button>
            ) : (
              <button className={small} disabled={busy !== null} onClick={() => run("keep", () => previewsApi.keep(p.id, 168))} title="Keep it for 7 days past its TTL (a landed or closed change still ends it)">
                <Pin className="size-3.5" /> Keep 7 days
              </button>
            )}
            <button
              className="ml-auto grid size-7 place-items-center rounded-md text-muted hover:bg-danger/10 hover:text-danger disabled:opacity-40"
              disabled={busy !== null}
              onClick={() => setDeleting(true)}
              aria-label="Delete"
              title="Delete this preview"
            >
              <Trash2 className="size-3.5" />
            </button>
          </div>
        )}
      </div>
    </>
  );
}

/** Links for someone outside the team, each open until it ends (or is revoked). */
function Shares({ id }: { id: string }) {
  const [list, setList] = useState<PreviewShare[] | null>(null);
  const [made, setMade] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let live = true;
    previewsApi.shares(id).then(
      (l) => live && setList(l),
      (e) => live && setError(errorText(e)),
    );
    return () => {
      live = false;
    };
  }, [id]);
  const act = async (fn: () => Promise<void>) => {
    setBusy(true);
    try {
      await fn();
      setList(await previewsApi.shares(id));
      setError(null);
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  if (!list) return error ? <p className="text-[11.5px] text-danger">{error}</p> : null;
  const open = list.filter((x) => !x.revoked && x.expires_at > Date.now());
  return (
    <>
      <Label>Share</Label>
      <p className="text-[11.5px] text-subtle">A link for someone outside the team, open until it ends. Revoke it any time.</p>
      <div className="flex gap-1.5">
        {[
          [24, "For a day"],
          [168, "For a week"],
        ].map(([hours, label]) => (
          <button key={hours} type="button" className={small} disabled={busy} onClick={() => void act(async () => setMade((await previewsApi.share(id, hours as number)).url))}>
            {label}
          </button>
        ))}
      </div>
      {made && (
        <div className="flex items-center gap-1.5 rounded-md border border-line-2 bg-canvas px-2 py-1.5">
          <span className="min-w-0 flex-1 truncate font-mono text-[11px]" title={made}>
            {made}
          </span>
          <button type="button" className="text-[11.5px] text-accent hover:underline" onClick={() => void navigator.clipboard?.writeText(made)}>
            Copy
          </button>
        </div>
      )}
      {made && <p className="text-[11px] text-subtle">Shown only now: roll keeps no copy of it.</p>}
      {open.length > 0 && (
        <ul className="space-y-1">
          {open.map((x) => (
            <li key={x.id} className="flex items-center gap-2 text-[11.5px]">
              <span className="font-mono text-subtle">{x.id}</span>
              <span className="min-w-0 flex-1 truncate text-muted">until {new Date(x.expires_at).toLocaleString(undefined, { month: "short", day: "numeric", hour: "numeric", minute: "2-digit" })}</span>
              <button type="button" className="text-muted hover:text-danger disabled:opacity-40" disabled={busy} onClick={() => void act(() => previewsApi.unshare(id, x.id))}>
                Revoke
              </button>
            </li>
          ))}
        </ul>
      )}
      {error && <p className="text-[11.5px] text-danger">{error}</p>}
    </>
  );
}

/** The preview's own variables, over the project's `env` (its services redeploy with them). */
function Vars({ id }: { id: string }) {
  const [vars, setVars] = useState<Record<string, string> | null>(null);
  const [name, setName] = useState("");
  const [value, setValue] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let live = true;
    previewsApi.vars(id).then(
      (v) => live && setVars(v.vars),
      (e) => live && setError(errorText(e)),
    );
    return () => {
      live = false;
    };
  }, [id]);
  const save = async (change: Record<string, string | null>) => {
    setBusy(true);
    try {
      setVars((await previewsApi.setVars(id, change)).vars);
      setError(null);
      return true;
    } catch (e) {
      setError(errorText(e));
      return false;
    } finally {
      setBusy(false);
    }
  };
  if (!vars) return error ? <p className="text-[11.5px] text-danger">{error}</p> : null;
  return (
    <>
      <Label>Variables</Label>
      <p className="text-[11.5px] text-subtle">Its own, over the project's. Its services redeploy with them.</p>
      {Object.keys(vars).length > 0 && (
        <ul className="space-y-1">
          {Object.entries(vars).map(([k, v]) => (
            <li key={k} className="group flex items-center gap-2 font-mono text-[11.5px]">
              <span className="shrink-0">{k}</span>
              <span className="min-w-0 flex-1 truncate text-muted">{v}</span>
              <button className="text-subtle hover:text-danger disabled:opacity-40" disabled={busy} onClick={() => void save({ [k]: null })} aria-label={`Remove ${k}`}>
                <Trash2 className="size-3.5" strokeWidth={1.5} />
              </button>
            </li>
          ))}
        </ul>
      )}
      <form
        className="flex gap-1.5"
        onSubmit={async (e) => {
          e.preventDefault();
          if (name.trim() && (await save({ [name.trim()]: value }))) {
            setName("");
            setValue("");
          }
        }}
      >
        <input value={name} onChange={(e) => setName(e.target.value)} placeholder="NAME" aria-label="Name" spellCheck={false} className={`${input} w-[38%]`} />
        <input value={value} onChange={(e) => setValue(e.target.value)} placeholder="value" aria-label="Value" spellCheck={false} className={`${input} min-w-0 flex-1`} />
        <button type="submit" className={small} disabled={busy || !name.trim()}>
          Set
        </button>
      </form>
      {error && <p className="text-[11.5px] text-danger">{error}</p>}
    </>
  );
}

function ActivityList({ id, updated }: { id: string; updated: number }) {
  const [items, setItems] = useState<PreviewActivity[] | null>(null);
  const [visitors, setVisitors] = useState<PreviewVisitor[]>([]);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let live = true;
    // The list is a nicety: an older node without it (a 404) leaves the activity as it is.
    Promise.all([previewsApi.activity(id), previewsApi.visitors(id).catch(() => [])]).then(
      ([a, v]) => live && (setItems(a), setVisitors(v ?? []), setError(null)),
      (e) => live && setError(errorText(e)),
    );
    return () => {
      live = false;
    };
  }, [id, updated]);
  if (error) return <p className="text-[11.5px] text-danger">{error}</p>;
  if (!items) return <p className="text-[11.5px] text-subtle">Loading…</p>;
  return (
    <>
      {visitors.length > 0 && (
        <section className="mb-4">
          <Label>Who opened it</Label>
          <ul className="mt-2 space-y-1.5">
            {visitors.map((v) => (
              <li key={v.who} className="flex gap-2.5 text-[12px]">
                <span className="w-14 shrink-0 pt-px text-right font-mono text-[10.5px] text-subtle" title={new Date(v.last_at).toLocaleString()}>
                  {since(v.last_at)}
                </span>
                <span className="min-w-0 flex-1 truncate font-mono text-soft">{v.who}</span>
              </li>
            ))}
          </ul>
        </section>
      )}
      {items.length ? (
        <ol className="space-y-2">
          {items.map((a, i) => (
            <li key={`${a.at}-${i}`} className="flex gap-2.5 text-[12px]">
              <span className="w-14 shrink-0 pt-px text-right font-mono text-[10.5px] text-subtle" title={new Date(a.at).toLocaleString()}>
                {since(a.at)}
              </span>
              <span className="min-w-0 flex-1 break-words text-soft">{a.message}</span>
            </li>
          ))}
        </ol>
      ) : (
        <p className="text-[11.5px] text-subtle">Nothing yet.</p>
      )}
    </>
  );
}

/** Whether the page is in view (a hidden tab follows no logs). */
function useVisible() {
  const [visible, setVisible] = useState(() => typeof document === "undefined" || document.visibilityState === "visible");
  useEffect(() => {
    const on = () => setVisible(document.visibilityState === "visible");
    document.addEventListener("visibilitychange", on);
    return () => document.removeEventListener("visibilitychange", on);
  }, []);
  return visible;
}

/** The first service's page as the preview's last push left it. */
function Screenshot({
  id,
  at,
  change: changeFirst,
  phone: hasPhoneFirst,
  base: baseFirst,
  pages = [],
}: {
  id: string;
  at: number;
  change?: { since: string; percent: number; phone_percent?: number | null } | null;
  phone?: boolean;
  base?: { branch: string; percent: number; phone_percent: number | null; head?: string | null; load_ms?: number | null } | null;
  pages?: PageCheck[];
}) {
  // Which page: the first (`/`) or another of the project's (desktop only).
  const [path, setPath] = useState("/");
  const other = pages.find((x) => x.path === path);
  const hasPhone = !other && hasPhoneFirst;
  const change = other ? (other.since && other.percent != null ? { since: other.since, percent: other.percent } : null) : changeFirst;
  const baseChange = baseFirst && other ? (other.base_percent != null ? { ...baseFirst, percent: other.base_percent, phone_percent: null } : null) : baseFirst;
  const page = other?.path;
  // Main: the default branch's preview's screenshot, the one this differs from.
  const [main, setMain] = useState(false);
  // Diff: where it differs from main's (else the push before's), marked.
  const [diff, setDiff] = useState(false);
  const [src, setSrc] = useState<string | null>(null);
  // Before: the screenshot at the head before, to compare; phone: at a phone's width.
  const [before, setBefore] = useState(false);
  const [phone, setPhone] = useState(false);
  useEffect(() => {
    let url: string | null = null;
    let live = true;
    screenshotUrl(id, before && !main && !diff, phone, main && !diff, page, diff ? (baseChange ? "base" : "previous") : undefined).then(
      (u) => {
        url = u;
        if (live) setSrc(u);
        else URL.revokeObjectURL(u);
      },
      () => live && setSrc(null),
    );
    return () => {
      live = false;
      if (url) URL.revokeObjectURL(url);
    };
  }, [id, at, before, phone, main, page, diff, !!baseChange]);
  if (!src && path === "/") return null;
  const since = change?.since.replace(/^cs_/, "").slice(0, 10);
  return (
    <>
      <Label>
        <span>Screenshot</span>
        <span className="ml-auto flex items-center gap-3">
          {hasPhone && (
            <button type="button" onClick={() => setPhone((p) => !p)} aria-pressed={phone} className="text-muted hover:text-fg">
              {phone ? "Desktop" : "Phone"}
            </button>
          )}
          {baseChange && (
            <button type="button" onClick={() => setMain((m) => !m)} aria-pressed={main} className="text-muted hover:text-fg" title={`The page on ${baseChange.branch}`}>
              {main ? "This change" : baseChange.branch}
            </button>
          )}
          {(baseChange || change) && (
            <button
              type="button"
              onClick={() => setDiff((d) => !d)}
              aria-pressed={diff}
              className="text-muted hover:text-fg"
              title={baseChange ? `What differs from ${baseChange.branch}, marked` : "What changed since the push before, marked"}
            >
              {diff ? "Page" : "Diff"}
            </button>
          )}
          {change && (
            <button type="button" onClick={() => setBefore((b) => !b)} aria-pressed={before} className="text-muted hover:text-fg" title={`The page at ${since}, the head before`}>
              {before ? "After" : "Before"}
            </button>
          )}
        </span>
      </Label>
      {pages.length > 0 && (
        <div className="mb-1.5 flex flex-wrap gap-1" role="group" aria-label="Page">
          {["/", ...pages.map((x) => x.path)].map((x) => (
            <button
              key={x}
              type="button"
              onClick={() => {
                setPath(x);
                setDiff(false);
                setPhone(false);
                setBefore(false);
                setMain(false);
              }}
              aria-pressed={path === x}
              className={`h-6 max-w-40 truncate rounded-md border px-1.5 font-mono text-[11px] transition-colors ${path === x ? "border-line-2 bg-panel text-fg" : "border-transparent text-muted hover:text-fg"}`}
            >
              {pageLabel(x)}
            </button>
          ))}
        </div>
      )}
      {diff && <p className="mb-1 text-[11.5px] text-subtle">Marked: what differs from {baseChange ? baseChange.branch : "the push before"}.</p>}
      {other && other.errors > 0 && (
        <p className="mb-1 text-[11.5px] text-danger">
          {other.errors} {other.errors === 1 ? "error" : "errors"} while it loaded.
        </p>
      )}
      {other && <PageChecks page={other} branch={baseChange?.branch ?? "main"} />}
      {baseChange && (
        <p className="mb-1 text-[11.5px] text-subtle">
          {main
            ? `On ${baseChange.branch}${baseChange.head ? ` at ${baseChange.head.replace(/^cs_/, "").slice(0, 10)}, where this change left it` : ""}.`
            : (() => {
                const pct = phone ? baseChange.phone_percent : baseChange.percent;
                return pct == null ? "" : pct > 0 ? `Differs ${Math.max(pct, 0.1).toFixed(1)}% from ${baseChange.branch}.` : `Looks the same as ${baseChange.branch}.`;
              })()}
        </p>
      )}
      {change && (
        <p className="mb-1.5 text-[11.5px] text-subtle">
          {(() => {
            if (before) return `At ${since}, before this push.`;
            const pct = phone ? change.phone_percent : change.percent;
            if (pct == null) return "No phone-width screenshot before this push to compare.";
            return pct > 0 ? `Changed ${Math.max(pct, 0.1).toFixed(1)}% since ${since}.` : `Looks the same as at ${since}.`;
          })()}
        </p>
      )}
      {src ? (
        <a href={src} target="_blank" rel="noreferrer" title="Open it full size">
          <img
            src={src}
            alt={other ? `The first service's ${other.path}` : phone ? "The first service's page at a phone's width" : "The first service's page"}
            className={`max-h-56 rounded-lg border border-line-2 object-cover object-top ${phone && !other ? "mx-auto w-28" : "w-full"}`}
          />
        </a>
      ) : (
        <p className="text-[11.5px] text-muted">No screenshot of {path} yet.</p>
      )}
    </>
  );
}

/** The "Checks" heading: what the screenshot's session found on the first page, problems first,
 * and how many are new against main; the lines under it are each check's. */
function ChecksHeading({ p }: { p: Preview }) {
  const { problems, added } = checkSummary(p);
  return (
    <Label>
      <span>Checks</span>
      <span className={`ml-auto normal-case ${problems > 0 ? "text-danger" : "text-subtle"}`}>
        {problems === 0 ? "nothing found" : `${problems} ${problems === 1 ? "problem" : "problems"}`}
        {problems > 0 && added != null && p.base_change ? ` · ${added === 0 ? "none" : added} new against ${p.base_change.branch}` : ""}
      </span>
    </Label>
  );
}

/** Another page's checks (the first page's, read on it too), with what's new against main's. */
function PageChecks({ page, branch }: { page: PageCheck; branch: string }) {
  const new_ = (n: number | null | undefined) => (n == null ? "" : n === 0 ? " (none new)" : ` (${n} new)`);
  const a11y = page.a11y ? a11yWords(page.a11y) : [];
  const seo = page.a11y?.seo ? seoWords(page.a11y.seo) : [];
  const broken = page.links?.broken ?? [];
  if (!page.step && !a11y.length && !seo.length && !broken.length && !page.against?.text?.added_n && !page.against?.text?.removed_n) return null;
  return (
    <div className="mb-1.5 space-y-0.5 text-[11.5px]">
      {page.step && <p className="break-words text-danger">Its steps stopped at {page.step}: taken as it was then.</p>}
      {broken.length > 0 && (
        <p className="break-words text-danger">
          {broken.length} broken {broken.length === 1 ? "link" : "links"}: <span className="font-mono text-[11px]">{broken.map((b) => `${b.path} ${b.status || "no answer"}`).join(", ")}</span>
          {new_(page.against?.links_added)}
        </p>
      )}
      {a11y.length > 0 && (
        <p className="text-muted">
          Accessibility: {a11y.join(", ")}
          {new_(page.against?.a11y_added)}
        </p>
      )}
      {seo.length > 0 && (
        <p className="text-muted">
          SEO: {seo.join(", ")}
          {new_(page.against?.seo_added)}
        </p>
      )}
      {page.against?.text && <TextAgainst diff={page.against.text} branch={branch} />}
    </div>
  );
}

/** The first page's words against the default branch's (Chromatic's DOM snapshots, in words):
 * lines it lost and gained, as text. */
export function TextAgainst({ diff, branch, what = "words" }: { diff: TextDiff; branch: string; what?: "words" | "api" }) {
  if (diff.added_n + diff.removed_n === 0) return null;
  const more = diff.added_n + diff.removed_n - diff.added.length - diff.removed.length;
  const label = what === "api" ? `Its API against ${branch}` : `Its words against ${branch}`;
  return (
    <div className="mt-1.5 text-[11.5px]" aria-label={label}>
      <p className={what === "api" && diff.removed_n > 0 ? "text-danger" : "text-muted"}>
        {what === "api" ? `${label}: ${diff.added_n} added, ${diff.removed_n} removed${diff.removed_n > 0 ? " (breaking for its callers)" : ""}` : `${label}: ${diff.added_n} new, ${diff.removed_n} gone`}
      </p>
      <ul className="mt-0.5 space-y-0.5 font-mono text-[11px] [overflow-wrap:anywhere]">
        {diff.removed.map((l, i) => (
          <li key={`-${i}`} className="text-subtle line-through decoration-white/20">
            − {l}
          </li>
        ))}
        {diff.added.map((l, i) => (
          <li key={`+${i}`} className="text-fg">
            + {l}
          </li>
        ))}
      </ul>
      {more > 0 && <p className="mt-0.5 text-subtle">and {more} more lines</p>}
    </div>
  );
}

/** The project's endpoints timed on the preview's data (`github.endpoints`: the median of 10
 * GETs) against main's, the clearly slower ones in red. */
export function Endpoints({ items, branch }: { items: EndpointTime[]; branch: string }) {
  if (!items.length) return null;
  return (
    <div className="mt-1.5 text-[11.5px]" aria-label={`Its endpoints against ${branch}`}>
      <p className="text-muted">Its endpoints against {branch}</p>
      <ul className="mt-0.5 space-y-0.5 font-mono text-[11px] tabular-nums">
        {items.map((e) => (
          <li key={e.path} className={endpointSlower(e) ? "text-danger" : "text-fg"}>
            GET {e.path} {e.ms} ms{e.main_ms != null ? ` (${branch} ${e.main_ms} ms)` : ""}
            {e.status === 0 ? ", no answer" : e.status >= 400 ? `, ${e.status}` : ""}
          </li>
        ))}
      </ul>
    </div>
  );
}

/** What the sheet leads with: what changed against main and what waits on someone, most serious
 * first, each jumping to its section below (Vercel's deployment summary). Says nothing new when
 * there's nothing, once the preview has been compared. */
function VerdictStrip({ p }: { p: Preview }) {
  const items = verdict(p);
  const jump = (target: VerdictTarget) => {
    const reduce = typeof window !== "undefined" && window.matchMedia?.("(prefers-reduced-motion: reduce)").matches;
    document.querySelector(`[data-section="${target}"]`)?.scrollIntoView({ behavior: reduce ? "auto" : "smooth", block: "start" });
  };
  if (!items.length) {
    return p.base_change ? <p className="mb-3 text-[11.5px] text-subtle">Nothing new against {p.base_change.branch}: its pages, API and checks are as they were.</p> : null;
  }
  const tone = { danger: "border-danger/40 text-danger", attention: "border-accent/40 text-accent", note: "border-line-2 text-muted" } as const;
  return (
    <div className="mb-3 flex flex-wrap gap-1.5" role="list" aria-label="What changed">
      {items.map((v, i) => (
        <button key={i} role="listitem" type="button" onClick={() => jump(v.target)} className={`h-6 rounded-md border px-2 text-[11.5px] hover:bg-white/[0.05] ${tone[v.tone]}`}>
          {v.text}
        </button>
      ))}
    </div>
  );
}

/** A roll apps service's build promoted to production (Vercel's promote): the preview
 * environment's live image deployed to production as is (no rebuild), with production's
 * variables (roll apps' `…/environments/production/promote`). It's the person's own call to roll
 * apps, which decides who may deploy production; previews holds no power of its own here. */
function Promote({ preview: p }: { preview: Preview }) {
  // Each roll apps service of the preview, with the deployment its environment ran when the
  // sheet opened: what was looked at is what's promoted (a newer push since is refused).
  const [services, setServices] = useState<{ id: string; name: string; seen: string | null }[]>([]);
  const [asking, setAsking] = useState<{ id: string; name: string; seen: string | null } | null>(null);
  const [busy, setBusy] = useState(false);
  const [done, setDone] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const names = p.services.map((s) => s.name).join(",");
  useEffect(() => {
    let live = true;
    const env = `pv-${p.id.replace(/^pv_/, "")}`;
    appsApi.services(p.project).then(
      async (all) => {
        const mine = all.filter((s) => s.project === p.project && names.split(",").includes(s.name));
        const seen = await Promise.all(mine.map((s) => appsApi.environments(s.id).then((es) => es.find((e) => e.name === env)?.active?.id ?? null, () => null)));
        if (live) setServices(mine.map((s, i) => ({ id: s.id, name: s.name, seen: seen[i] })).filter((s) => s.seen));
      },
      () => live && setServices([]),
    );
    return () => {
      live = false;
    };
  }, [p.project, names]);
  if (!services.length) return null;
  const env = `pv-${p.id.replace(/^pv_/, "")}`;
  return (
    <div className="mt-1.5 text-[11.5px]">
      {done && <p className="text-subtle">Promoting {done} to production: its build, not built again (roll apps).</p>}
      {error && <p className="break-words text-danger">{error}</p>}
      <div className="flex flex-wrap gap-1.5">
        {services.map((s) => (
          <button
            key={s.id}
            type="button"
            onClick={() => setAsking(s)}
            className="h-6 rounded-md border border-line-2 px-2 text-[11.5px] text-fg hover:bg-white/[0.05]"
            title="Deploy this preview's build of it to production as is, with production's variables"
          >
            Promote {s.name} to production
          </button>
        ))}
      </div>
      {asking && (
        <Confirm
          word={asking.name}
          action="Promote to production"
          busy={busy}
          onCancel={() => setAsking(null)}
          onConfirm={async () => {
            setBusy(true);
            setError(null);
            try {
              // roll apps promotes only the deployment the sheet showed (a 409 if a newer push
              // deployed since), from the same read the promotion is made of.
              await appsApi.promote(asking.id, "production", env, asking.seen ?? undefined);
              setDone(asking.name);
              setAsking(null);
            } catch (e) {
              setError(errorText(e));
            } finally {
              setBusy(false);
            }
          }}
        >
          Production gets the image this preview runs for {asking.name}, with production's own variables and data; nothing is built again.
        </Confirm>
      )}
    </div>
  );
}

/** What a change breaks inside the API operations main's and its own both have (a parameter
 * that becomes required, a response field that goes): the change's own document's words. */
export function ApiBreaking({ items }: { items: string[] }) {
  if (!items.length) return null;
  return (
    <div className="mt-1.5 text-[11.5px]" aria-label="What it breaks in the operations it keeps">
      <p className="text-danger">It breaks callers of the operations it keeps:</p>
      <ul className="mt-0.5 space-y-0.5 font-mono text-[11px] text-fg [overflow-wrap:anywhere]">
        {items.map((b, i) => (
          <li key={i}>{b}</li>
        ))}
      </ul>
    </div>
  );
}

/** A note from the preview's page with its replies (Vercel Comments' threads): anyone on the
 * project replies (a comment review naming the note, which the page lists under it) or resolves
 * it (`POST …/notes/{review}/resolve`, kept by previews), and the page stops listing it. */
function NoteThread({ previewId, change, review, onDone }: { previewId: string; change: string; review: string; onDone?: () => void }) {
  const [note, setNote] = useState<string | null>(null);
  const [replies, setReplies] = useState<{ id: string; by: string; text: string }[]>([]);
  const [reply, setReply] = useState("");
  const [state, setState] = useState<"idle" | "busy" | "done">("idle");
  const [error, setError] = useState<string | null>(null);
  // What the note is about, as the next screenshot run cropped it from the page.
  const [crop, setCrop] = useState<string | null>(null);
  useEffect(() => {
    let url: string | null = null;
    let live = true;
    noteCropUrl(previewId, review).then(
      (u) => {
        url = u;
        if (live) setCrop(u);
        else if (u) URL.revokeObjectURL(u);
      },
      () => {},
    );
    return () => {
      live = false;
      if (url) URL.revokeObjectURL(url);
    };
  }, [previewId, review]);
  const load = () =>
    codeApi.reviews(change).then(
      (all) => {
        const r = all.find((x) => x.id === review);
        if (!r) return setError("That note isn't on this change.");
        setNote(r.body.split("\n\n")[1] ?? r.body);
        setReplies(
          all
            .filter((x) => !x.dismissed && replyOf(x.body)?.note === review)
            .sort((a, b) => a.created_at - b.created_at)
            .map((x) => ({ id: x.id, by: x.reviewer.name, text: replyOf(x.body)?.text ?? "" })),
        );
      },
      (e) => setError(errorText(e)),
    );
  useEffect(() => {
    void load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [change, review]);
  if (error) return <p className="text-[11.5px] text-danger">{error}</p>;
  if (state === "done") return <p className="text-[11.5px] text-subtle">This note is resolved: the page no longer lists it.</p>;
  const button = "h-6 rounded-md border border-line-2 px-2 text-[11.5px] text-fg hover:bg-white/[0.05] disabled:opacity-40";
  return (
    <div className="rounded-lg border border-line-2 bg-raised px-2.5 py-2 text-[11.5px] inset-shadow-highlight">
      <p className="text-muted">A note on the preview's page</p>
      {note && <p className="mt-1 break-words text-fg">{note}</p>}
      {crop && <img src={crop} alt="The part of the page the note is about" className="mt-1.5 max-h-48 max-w-full rounded-md border border-line-2" />}
      {replies.map((r) => (
        <div key={r.id} className="mt-1.5 border-l border-line-2 pl-2">
          <p className="text-subtle">{r.by}</p>
          <p className="break-words text-fg">{r.text}</p>
        </div>
      ))}
      <textarea
        aria-label="Reply"
        value={reply}
        onChange={(e) => setReply(e.target.value)}
        placeholder="Reply…"
        rows={2}
        className="mt-2 w-full rounded-md border border-line-2 bg-canvas px-2 py-1.5 text-[11.5px] outline-none transition-colors placeholder:text-subtle focus:border-accent/60"
      />
      <div className="mt-1.5 flex justify-end gap-1.5">
        <button
          type="button"
          disabled={state === "busy" || note === null || !reply.trim()}
          onClick={async () => {
            setState("busy");
            try {
              await codeApi.submitReview(change, { verdict: "comment", body: replyBody(review, reply) });
              setReply("");
              await load();
            } catch (e) {
              setError(errorText(e));
            } finally {
              setState("idle");
            }
          }}
          className={button}
        >
          Reply
        </button>
        <button
          type="button"
          disabled={state === "busy" || note === null}
          onClick={async () => {
            setState("busy");
            try {
              await previewsApi.resolveNote(previewId, review);
              setState("done");
              onDone?.();
            } catch (e) {
              setError(errorText(e));
            }
          }}
          className={button}
        >
          {state === "busy" ? "Working…" : "Resolve"}
        </button>
      </div>
    </div>
  );
}

/** The mail the preview's services sent, caught by roll (never delivered): who it was for and
 * its subject, each opened as text (never rendered: it's the preview's own content). */
function Mail({ id }: { id: string }) {
  const [list, setList] = useState<CaughtMail[] | null>(null);
  const [open, setOpen] = useState<{ id: string; text: string } | null>(null);
  useEffect(() => {
    let live = true;
    previewsApi.mail(id).then(
      (r) => live && setList(r.mail),
      () => live && setList([]),
    );
    return () => {
      live = false;
    };
  }, [id]);
  if (!list?.length) return null;
  return (
    <>
      <Label>Mail caught ({list.length})</Label>
      <div className="rounded-lg border border-line-2 bg-raised inset-shadow-highlight">
        {list.map((m) => (
          <div key={m.id} className="border-t border-line px-2.5 py-1.5 first:border-t-0">
            <button
              type="button"
              onClick={async () => setOpen(open?.id === m.id ? null : { id: m.id, text: await previewsApi.mailText(id, m.id).catch(errorText) })}
              aria-expanded={open?.id === m.id}
              className="flex w-full min-w-0 items-center gap-2 text-left text-[11.5px]"
            >
              <span className="truncate">{m.subject ?? "(no subject)"}</span>
              <span className="ml-auto shrink-0 truncate font-mono text-[10.5px] text-subtle">to {m.to.join(", ")}</span>
            </button>
            {open?.id === m.id && (
              <pre className="mt-1 max-h-56 overflow-auto rounded-md border border-line px-2 py-1.5 font-mono text-[10.5px] whitespace-pre-wrap [overflow-wrap:anywhere] text-muted">{open.text}</pre>
            )}
          </div>
        ))}
      </div>
      <p className="mt-1 text-[11px] text-subtle">Its services' mail stops here: nobody in its data got it.</p>
    </>
  );
}

/** Copy a data branch's connection (`DATABASE_URL`, …: the branch's own credentials), to query
 * what the preview sees from a laptop (Neon's connection strings). Fetched only when asked. */
function CopyConnection({ previewId, kind, parent, name }: { previewId: string; kind: PreviewKind; parent: string; name: string }) {
  const [state, setState] = useState<"idle" | "busy" | "copied" | "none">("idle");
  return (
    <button
      type="button"
      disabled={state === "busy"}
      onClick={async () => {
        setState("busy");
        try {
          const { branches } = await previewsApi.connections(previewId);
          const url = Object.values(branches.find((b) => b.kind === kind && b.parent === parent)?.vars ?? {})[0];
          if (!url) return setState("none");
          await navigator.clipboard.writeText(url);
          setState("copied");
          setTimeout(() => setState("idle"), 1500);
        } catch {
          setState("none");
        }
      }}
      aria-label={`Copy ${name}'s connection`}
      title={state === "copied" ? "Copied" : state === "none" ? "No connection to copy" : "Copy its connection (the branch's own credentials)"}
      className="grid size-6 shrink-0 place-items-center rounded-md text-muted hover:bg-white/[0.06] hover:text-fg disabled:opacity-40"
    >
      {state === "copied" ? <Check className="size-3.5" /> : <Copy className="size-3.5" />}
    </button>
  );
}

/** A service's link, with the preview's access, as a QR code to scan (Vercel's toolbar): fetched
 * when asked, released when closed. */
export function PhoneCode({ id, service }: { id: string; service: string }) {
  const [src, setSrc] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let url: string | null = null;
    let live = true;
    qrUrl(id, service).then(
      (u) => {
        url = u;
        if (live) setSrc(u);
        else URL.revokeObjectURL(u);
      },
      (e) => live && setError(errorText(e)),
    );
    return () => {
      live = false;
      if (url) URL.revokeObjectURL(url);
    };
  }, [id, service]);
  if (error) return <p className="mt-1 text-[11px] text-danger">{error}</p>;
  return (
    <div className="mt-1.5 flex items-center gap-2.5">
      {src ? <img src={src} alt={`A QR code that opens ${service} on a phone`} className="size-28 rounded-md bg-white p-1" /> : <div className="size-28 rounded-md bg-white/[0.04]" />}
      <p className="text-[11px] text-subtle">Scan it with a phone's camera: it opens with the preview's access, so share it only with people who may see it.</p>
    </div>
  );
}

/** Lines a followed log keeps. */
const LOG_KEEP = 2000;

/** What its running services use, read again every 15 s while one runs and the sheet is in view. */
function useServiceMetrics(p: Preview): Map<string, ServiceMetrics> {
  const [m, setM] = useState<Map<string, ServiceMetrics>>(new Map());
  const running = p.services.some((s) => s.state === "running");
  const visible = useVisible();
  useEffect(() => {
    if (!running || !visible) {
      setM(new Map());
      return;
    }
    let live = true;
    const read = () =>
      previewsApi.metrics(p.id).then(
        (r) => live && setM(new Map(r.services.map((x) => [x.service, x]))),
        () => {},
      );
    void read();
    const t = setInterval(read, 15_000);
    return () => {
      live = false;
      clearInterval(t);
    };
  }, [p.id, running, visible]);
  return m;
}

/** `0.25 CPU · 96 of 256 MB`. */
function metricsText(m: ServiceMetrics): string {
  const mb = Math.round(m.memory_bytes / (1024 * 1024));
  return `${m.cpu < 0.01 ? "0" : m.cpu.toFixed(2)} CPU · ${mb}${m.memory_mb > 0 ? ` of ${m.memory_mb}` : ""} MB`;
}

/** Its latest requests through roll (Vercel's request log): newest first, errors at a glance. */
function Requests({ preview: p }: { preview: Preview }) {
  const [list, setList] = useState<RequestLine[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [errorsOnly, setErrorsOnly] = useState(false);
  const [n, setN] = useState(0);
  useEffect(() => {
    let live = true;
    setError(null);
    previewsApi.requests(p.id).then(
      (r) => live && setList(r.requests),
      (e) => live && setError(errorText(e)),
    );
    return () => {
      live = false;
    };
  }, [p.id, n]);
  const shown = (list ?? []).filter((r) => !errorsOnly || r.status >= 400);
  const several = p.services.length > 1;
  return (
    <>
      <div className="mb-2 flex items-center gap-2">
        <label className="flex flex-1 items-center gap-1.5 text-[11.5px] text-muted">
          <input type="checkbox" checked={errorsOnly} onChange={(e) => setErrorsOnly(e.target.checked)} className="accent-accent" />
          Errors only (4xx, 5xx)
        </label>
        <button onClick={() => setN((x) => x + 1)} aria-label="Read the requests again" className="grid size-7 place-items-center rounded-md text-muted hover:bg-white/[0.06] hover:text-fg">
          <RefreshCw className="size-3.5" />
        </button>
      </div>
      {error ? (
        <p className="text-[11.5px] text-danger">{error}</p>
      ) : !list ? (
        <p className="text-[11.5px] text-subtle">Loading…</p>
      ) : !shown.length ? (
        <p className="text-[11.5px] text-subtle">{list.length ? "No errors among its latest requests." : "No requests through roll yet (services on roll apps keep their own)."}</p>
      ) : (
        <div className="max-h-[60vh] overflow-auto rounded-lg border border-line-2 bg-canvas [scrollbar-width:thin]">
          {shown.map((r, i) => (
            <div key={`${r.at}-${i}`} className="flex items-baseline gap-2 border-b border-line px-2.5 py-1 font-mono text-[10.5px] last:border-b-0">
              <span className="text-subtle">{new Date(r.at).toLocaleTimeString()}</span>
              <span className={r.status >= 500 ? "text-danger" : r.status >= 400 ? "text-fg" : "text-muted"}>{r.status}</span>
              <span className="text-muted">{r.method}</span>
              <span className="min-w-0 flex-1 truncate text-soft" title={r.path}>
                {r.path}
              </span>
              {several && <span className="text-subtle">{r.service}</span>}
              <span className="text-subtle">{r.ms} ms</span>
            </div>
          ))}
        </div>
      )}
    </>
  );
}

/** Spans the Errors tab reads, in minutes. */
const ERROR_SPANS = [
  [60, "1 hour"],
  [360, "6 hours"],
  [1440, "24 hours"],
] as const;

/** Its services' errors, grouped (Sentry's issues, from its output alone): newest first, each opening to its newest sample. */
function Errors({ preview: p }: { preview: Preview }) {
  const [minutes, setMinutes] = useState<number>(60);
  const [list, setList] = useState<ErrorGroup[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [open, setOpen] = useState<string | null>(null);
  const [n, setN] = useState(0);
  useEffect(() => {
    let live = true;
    setError(null);
    setList(null);
    previewsApi.errors(p.id, minutes).then(
      (r) => live && setList(r.groups),
      (e) => live && setError(errorText(e)),
    );
    return () => {
      live = false;
    };
  }, [p.id, minutes, n]);
  const several = p.services.length > 1;
  return (
    <>
      <div className="mb-2 flex items-center gap-2">
        <select
          aria-label="Span"
          value={minutes}
          onChange={(e) => setMinutes(Number(e.target.value))}
          className="h-7 min-w-0 flex-1 rounded-md border border-line-2 bg-canvas px-2 text-[11.5px] outline-none focus:border-accent/60"
        >
          {ERROR_SPANS.map(([m, label]) => (
            <option key={m} value={m}>
              Last {label}
            </option>
          ))}
        </select>
        <button onClick={() => setN((x) => x + 1)} aria-label="Read the errors again" className="grid size-7 place-items-center rounded-md text-muted hover:bg-white/[0.06] hover:text-fg">
          <RefreshCw className="size-3.5" />
        </button>
      </div>
      {error ? (
        <p className="text-[11.5px] text-danger">{error}</p>
      ) : !list ? (
        <p className="text-[11.5px] text-subtle">Loading…</p>
      ) : !list.length ? (
        <p className="text-[11.5px] text-subtle">No errors in its output.</p>
      ) : (
        <ul className="overflow-hidden rounded-lg border border-line-2 bg-canvas" aria-label="Errors">
          {list.map((g) => {
            const key = `${g.service}/${g.fingerprint}`;
            const shown = open === key;
            return (
              <li key={key} className="border-b border-line last:border-b-0">
                <button onClick={() => setOpen(shown ? null : key)} aria-expanded={shown} className="flex w-full items-baseline gap-2 px-2.5 py-1.5 text-left hover:bg-fg/[0.03]">
                  <span className="w-9 shrink-0 text-right font-mono text-[10.5px] text-danger tabular-nums" title={`${g.count} since ${new Date(g.first_seen).toLocaleString()}`}>
                    {g.count > 999 ? "999+" : `${g.count}×`}
                  </span>
                  <span className="min-w-0 flex-1 truncate font-mono text-[11px] text-soft" title={g.title}>
                    {g.title}
                  </span>
                  {several && <span className="shrink-0 font-mono text-[10.5px] text-subtle">{g.service}</span>}
                  <span className="shrink-0 text-[10.5px] text-subtle" title={new Date(g.last_seen).toLocaleString()}>
                    {since(g.last_seen)}
                  </span>
                </button>
                {shown && (
                  <pre className="rise max-h-[40vh] overflow-auto border-t border-line px-2.5 py-2 font-mono text-[10.5px] leading-relaxed whitespace-pre-wrap text-soft [scrollbar-width:thin]">{g.sample}</pre>
                )}
              </li>
            );
          })}
        </ul>
      )}
    </>
  );
}

/** A shell in one of the preview's services (Render's shell, `heroku run`): opened on request, its
 * screen in a terminal, closed when it ends or the tab goes. A `roll.toml` service's is previews'
 * (`…/previews/{id}/shell`); a roll apps service's is roll apps' own, on the preview's `pv-`
 * environment, as Promote finds it (the project's roll apps service of that name). */
function Shell({ preview: p }: { preview: Preview }) {
  const [service, setService] = useState(p.services[0]?.name ?? "");
  // The session, and the roll apps service it's on (null: previews' own).
  const [session, setSession] = useState<{ id: string; apps: string | null } | null>(null);
  const [state, setState] = useState<"idle" | "opening" | "ended">("idle");
  const [error, setError] = useState<string | null>(null);
  // The preview's services that roll apps runs: name → roll apps service id.
  const [onApps, setOnApps] = useState<Record<string, string>>({});
  useEffect(() => {
    let live = true;
    appsApi.services(p.project).then(
      (all) => live && setOnApps(Object.fromEntries(all.filter((s) => s.project === p.project).map((s) => [s.name, s.id]))),
      () => live && setOnApps({}),
    );
    return () => {
      live = false;
    };
  }, [p.project]);
  const env = `pv-${p.id.replace(/^pv_/, "")}`;
  if (!p.services.length) return <p className="text-[11.5px] text-subtle">No services, so no shell.</p>;
  const apps = onApps[service] ?? null;
  const open = async () => {
    setError(null);
    setState("opening");
    try {
      const { id } = apps ? await appsApi.openShell(apps, env, { cols: 80, rows: 24 }) : await previewsApi.openShell(p.id, { service, cols: 80, rows: 24 });
      setSession({ id, apps });
      setState("idle");
    } catch (e) {
      setError(errorText(e));
      setState("idle");
    }
  };
  return (
    <>
      <div className="mb-2 flex items-center gap-2">
        <select
          aria-label="Service"
          value={service}
          disabled={!!session}
          onChange={(e) => setService(e.target.value)}
          className="h-7 min-w-0 flex-1 rounded-md border border-line-2 bg-canvas px-2 font-mono text-[11.5px] outline-none focus:border-accent/60"
        >
          {p.services.map((s) => (
            <option key={s.name} value={s.name}>
              {s.name}
            </option>
          ))}
        </select>
        {session ? (
          <button onClick={() => setSession(null)} aria-label="Close the shell" className="flex h-7 items-center gap-1.5 rounded-md border border-line-2 px-2.5 text-[11.5px] text-soft hover:bg-white/[0.06] hover:text-fg">
            <X className="size-3.5" /> Close
          </button>
        ) : (
          <button onClick={() => void open()} disabled={state === "opening"} className="flex h-7 items-center gap-1.5 rounded-md bg-fg px-2.5 text-[11.5px] font-medium text-canvas disabled:opacity-60">
            <SquareTerminal className="size-3.5" /> {state === "opening" ? "Opening…" : state === "ended" ? "Open another" : "Open a shell"}
          </button>
        )}
      </div>
      {session ? (
        <TerminalScreen
          key={session.id}
          session={session.id}
          className="h-[52vh] rounded-lg border border-line-2 bg-canvas p-1.5"
          link={
            session.apps
              ? {
                  follow: (h) => followAppsShell(session.apps!, env, session.id, h),
                  keys: (data) => appsApi.shellKeys(session.apps!, env, session.id, data),
                  resize: (size) => appsApi.resizeShell(session.apps!, env, session.id, size),
                  close: () => appsApi.closeShell(session.apps!, env, session.id),
                }
              : {
                  follow: (h) => followShell(p.id, session.id, h),
                  keys: (data) => previewsApi.shellKeys(p.id, session.id, data),
                  resize: (size) => previewsApi.resizeShell(p.id, session.id, size),
                  close: () => previewsApi.closeShell(p.id, session.id),
                }
          }
          onEnded={() => {
            setSession(null);
            setState("ended");
          }}
        />
      ) : (
        <>
          {error && <p className="mb-1.5 text-[11.5px] leading-relaxed text-danger">{error}</p>}
          <p className="text-[11.5px] leading-relaxed text-subtle">
            {state === "ended" ? "The shell ended." : "bash (or sh) in the running service, with the preview's variables and its branches' data, never production's. A service that's asleep is started and kept awake while the shell is open; the activity says who opened it."}{" "}
            From a terminal: <code className="font-mono text-soft">{apps ? `roll apps ssh -s ${service} -e ${env}` : `roll preview shell ${p.id} -s ${service}`}</code>.
          </p>
        </>
      )}
    </>
  );
}

function Logs({ preview: p }: { preview: Preview }) {
  const [service, setService] = useState(p.services[0]?.name ?? "");
  const [lines, setLines] = useState<string[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [n, setN] = useState(0);
  // A running service's log follows as it comes (pushed) while the page is in view; any other
  // is read once.
  const running = p.services.some((x) => x.name === service && x.state === "running");
  const visible = useVisible();
  useEffect(() => {
    if (!service || !visible) return;
    let live = true;
    setError(null);
    if (!running) {
      previewsApi.logs(p.id, { service, lines: 300 }).then(
        (l) => live && setLines(l.lines),
        (e) => live && setError(errorText(e)),
      );
      return () => {
        live = false;
      };
    }
    const abort = new AbortController();
    let got: string[] = [];
    setLines(null);
    followLogs(
      p.id,
      service,
      300,
      (l) => {
        got = [...got.slice(-(LOG_KEEP - 1)), l];
        if (live) setLines(got);
      },
      abort.signal,
    ).then(
      () => live && setLines(got),
      (e) => live && !abort.signal.aborted && setError(errorText(e)),
    );
    return () => {
      live = false;
      abort.abort();
    };
  }, [p.id, service, running, visible, n]);
  if (!p.services.length) return <p className="text-[11.5px] text-subtle">No services, so no logs.</p>;
  return (
    <>
      <div className="mb-2 flex items-center gap-2">
        <select
          aria-label="Service"
          value={service}
          onChange={(e) => setService(e.target.value)}
          className="h-7 min-w-0 flex-1 rounded-md border border-line-2 bg-canvas px-2 font-mono text-[11.5px] outline-none focus:border-accent/60"
        >
          {p.services.map((s) => (
            <option key={s.name} value={s.name}>
              {s.name}
            </option>
          ))}
        </select>
        <button onClick={() => setN((x) => x + 1)} aria-label="Read the logs again" className="grid size-7 place-items-center rounded-md text-muted hover:bg-white/[0.06] hover:text-fg">
          <RefreshCw className="size-3.5" />
        </button>
      </div>
      {error ? (
        <p className="text-[11.5px] text-danger">{error}</p>
      ) : !lines ? (
        <p className="text-[11.5px] text-subtle">Loading…</p>
      ) : (
        <pre className="max-h-[60vh] overflow-auto rounded-lg border border-line-2 bg-canvas p-2.5 font-mono text-[10.5px] leading-relaxed whitespace-pre-wrap text-soft [scrollbar-width:thin]">
          {lines.length ? lines.join("\n") : "No output yet."}
        </pre>
      )}
    </>
  );
}
