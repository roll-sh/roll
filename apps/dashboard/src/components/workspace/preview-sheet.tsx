import { Boxes, Database, ExternalLink, Eye, Inbox, ListChecks, Moon, Package, Pin, RefreshCw, RotateCcw, Search, Sun, Trash2, Waves, Zap, type LucideIcon } from "lucide-react";
import { useEffect, useState } from "react";
import { Confirm } from "@/components/ui";
import { a11yWords, duration, followLogs, openUrl, previewsApi, screenshotUrl, resourceId, thanMain, type PageCheck, type Preview, type PreviewActivity, type PreviewKind, type PreviewShare } from "@/lib/previews-api";
import { formatBytes } from "@/lib/roll-api";
import { Head, Label, Rows, Stat, Tabs } from "./inspector";
import { FeedbackForm, type PreviewFeedback } from "./preview-feedback";
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
  /** It was posted (the link that brought it is spent). */
  onFeedbackPosted?: () => void;
}) {
  const [tab, setTab] = useState<"Overview" | "Activity" | "Logs">("Overview");
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [deleting, setDeleting] = useState(false);
  const gone = p.desired === "deleted";
  const asleep = p.state === "sleeping";

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
      <Tabs tabs={["Overview", "Activity", "Logs"] as const} value={tab} onChange={setTab} />
      <div key={tab} className="rise flex-1 overflow-y-auto p-3 [scrollbar-width:none]">
        {tab === "Overview" ? (
          <>
            {p.error && <p className="mb-3 rounded-lg border border-danger/40 bg-danger/10 px-2.5 py-2 text-[11.5px] break-words text-danger">{p.error}</p>}
            {feedback && (
              <div className="mb-3">
                <FeedbackForm preview={p} feedback={feedback} onPosted={onFeedbackPosted} />
              </div>
            )}
            {p.state === "awaiting_approval" && (
              <p className="mb-3 rounded-lg border border-accent/40 bg-accent/[0.06] px-2.5 py-2 text-[11.5px] text-muted">
                A pull request from a fork: a writer approves its head (<span className="font-mono text-fg">approve-run</span>) before its services run.
              </p>
            )}
            <Label>Services</Label>
            {p.services.length ? (
              <div className="rounded-lg border border-line-2 bg-raised inset-shadow-highlight">
                {p.services.map((s) => (
                  <div key={s.name} className="border-t border-line px-2.5 py-1.5 first:border-t-0">
                    <div className="flex items-center gap-2 text-[12px]">
                      <StateDot state={s.state} />
                      <span className="font-medium">{s.name}</span>
                      <span className="text-[11px] text-subtle">{s.state}</span>
                      {s.deployed && <span className="ml-auto font-mono text-[10.5px] text-subtle">{s.deployed.slice(0, 8)}</span>}
                    </div>
                    <a href={openUrl(s)} target="_blank" rel="noreferrer" className="mt-0.5 flex items-center gap-1 font-mono text-[11px] text-accent hover:underline">
                      <span className="truncate">{host(s.url)}</span>
                      <ExternalLink className="size-3 shrink-0" />
                    </a>
                    {s.error && <p className="mt-0.5 text-[11px] break-words text-danger">{s.error}</p>}
                  </div>
                ))}
              </div>
            ) : (
              <p className="text-[11.5px] text-subtle">No services: a repo in this project runs here once its roll.toml has a [preview] section.</p>
            )}
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
                          <span className="truncate">{names.get(id) ?? r.parent}</span>
                          <span className="text-subtle">→</span>
                          <span className="truncate font-mono text-[11px]">{r.ref ?? "—"}</span>
                        </div>
                        <div className="mt-0.5 font-mono text-[10.5px] text-subtle">
                          {KIND_NAME[r.kind]} · {r.mode === "schema" ? "schema only" : "branch"} · {formatBytes(r.bytes)}
                          {masked?.get(id) ? ` · masked (${masked.get(id)})` : ""}
                        </div>
                        {r.error && <p className="mt-0.5 text-[11px] break-words text-danger">{r.error}</p>}
                      </div>
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

            {p.schema_changes && p.schema_changes.length > 0 && (
              <>
                <Label>Schema changes</Label>
                <pre className="max-h-48 overflow-auto rounded-lg border border-line-2 bg-raised px-2.5 py-1.5 font-mono text-[11px] leading-relaxed whitespace-pre-wrap [overflow-wrap:anywhere] inset-shadow-highlight">
                  {p.schema_changes.map((l, i) => (
                    <div key={i} className={l.startsWith("- ") ? "text-danger" : l.startsWith("+ ") ? "text-fg" : "text-subtle"}>
                      {l}
                    </div>
                  ))}
                </pre>
              </>
            )}

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
                      onClick={() => run("accept", () => previewsApi.acceptVisual(p.id))}
                      className="ml-auto h-6 rounded-md border border-line-2 px-2 text-[11.5px] text-fg hover:bg-white/[0.05] disabled:opacity-40"
                    >
                      {busy === "accept" ? "Accepting…" : "Accept"}
                    </button>
                  </>
                )}
              </div>
            )}
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
              </div>
            )}
            {p.page_a11y && a11yWords(p.page_a11y).length > 0 && (
              <p className="mt-1.5 text-[11.5px] text-muted" title={p.page_a11y.issues.map((i) => i.first).filter(Boolean).join("\n")}>
                Accessibility: {a11yWords(p.page_a11y).join(", ")}
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
              ]}
            />
          </>
        ) : tab === "Activity" ? (
          <ActivityList id={p.id} updated={p.updated_at} />
        ) : (
          <Logs preview={p} />
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
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let live = true;
    previewsApi.activity(id).then(
      (a) => live && (setItems(a), setError(null)),
      (e) => live && setError(errorText(e)),
    );
    return () => {
      live = false;
    };
  }, [id, updated]);
  if (error) return <p className="text-[11.5px] text-danger">{error}</p>;
  if (!items) return <p className="text-[11.5px] text-subtle">Loading…</p>;
  if (!items.length) return <p className="text-[11.5px] text-subtle">Nothing yet.</p>;
  return (
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
              {x}
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

/** Lines a followed log keeps. */
const LOG_KEEP = 2000;

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
