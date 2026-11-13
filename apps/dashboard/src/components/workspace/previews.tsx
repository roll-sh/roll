import { ExternalLink, GitBranch, Settings2 } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { openUrl, previewsApi, screenshotUrl, type Preview, type PreviewSettings, type PreviewUsage } from "@/lib/previews-api";
import type { WorkspaceProject } from "@/lib/workspace-api";
import { resourceKey, type ResourceView } from "@/lib/workspace-resources";
import { Inspector } from "./inspector";
import type { PreviewFeedback } from "./preview-feedback";
import { PreviewSettingsDialog } from "./preview-settings";
import { PreviewSheet } from "./preview-sheet";
import { errorText, host, money, previewOf, since, STATE_LABEL, StateDot, usePreviewMark, usePreviews } from "./previews-common";

const btn =
  "flex h-7 items-center gap-1.5 rounded-md border border-line-2 bg-raised px-2.5 text-[12px] font-medium whitespace-nowrap inset-shadow-highlight transition-colors hover:border-line-3 disabled:opacity-50";
const field = "h-8 rounded-lg border border-line-2 bg-canvas px-2.5 text-[12.5px] outline-none transition-colors focus:border-accent/60";

/** A parent resource's name by `"<kind>:<ref>"`, from the canvas's inventory. */
export function resourceNames(project: WorkspaceProject, inventory: Map<string, ResourceView>) {
  return new Map(project.resources.map((r) => [resourceKey(r), inventory.get(resourceKey(r))?.name ?? r.ref]));
}

/**
 * `/dashboard/projects/$id/previews`: the project's previews (a copy of the project per change),
 * what they cost, the rules that make them, and a preview of any branch.
 */
export function ProjectPreviews({
  project,
  inventory,
  open: initial,
  go,
  feedback,
  onFeedbackPosted,
}: {
  project: WorkspaceProject;
  inventory: Map<string, ResourceView>;
  open?: string;
  go?: string;
  /** A note from a preview page's feedback button, for the preview `open` names. */
  feedback?: PreviewFeedback;
  /** The note was posted: drop it from the address, so it isn't offered again. */
  onFeedbackPosted?: () => void;
}) {
  const { list, error, reload, put } = usePreviews(project.id);
  const [openId, setOpenId] = useState<string | null>(initial ?? null);
  const [settings, setSettings] = useState<PreviewSettings | null>(null);
  const [settingsError, setSettingsError] = useState<string | null>(null);
  const [rules, setRules] = useState(false);
  const [branching, setBranching] = useState(false);
  const [usage, setUsage] = useState<PreviewUsage | null>(null);

  const repos = useMemo(() => project.resources.filter((r) => r.kind === "repo").map((r) => r.ref), [project.resources]);
  const data = useMemo(() => project.resources.filter((r) => r.kind !== "repo"), [project.resources]);
  const names = useMemo(() => resourceNames(project, inventory), [project, inventory]);
  const opened = list?.find((p) => p.id === openId) ?? null;
  const act = usePreviewMark(opened ? [opened] : list);

  useEffect(() => setOpenId(initial ?? null), [initial]);
  // `&go=<service>`: on to that service's link, once (the preview's own 401 page sends here).
  const went = useRef(false);
  useEffect(() => {
    if (!go || went.current) return;
    const svc = opened?.services.find((s) => s.name === go);
    const url = svc && openUrl(svc);
    if (url) {
      went.current = true;
      window.location.assign(url);
    }
  }, [go, opened]);
  useEffect(() => {
    previewsApi.settings(project.id).then(setSettings, (e) => setSettingsError(errorText(e)));
  }, [project.id]);
  // The total follows the list (a new, slept or deleted preview changes it).
  useEffect(() => {
    if (list) previewsApi.usage(project.id).then(setUsage, () => {});
  }, [project.id, list]);

  const turnOn = async () => {
    try {
      setSettings(await previewsApi.putSettings(project.id, { enabled: true }));
    } catch (e) {
      setSettingsError(errorText(e));
    }
  };

  return (
    <>
      <div className="absolute inset-0 overflow-y-auto [scrollbar-width:thin]">
        <div className="mx-auto w-full max-w-[960px] px-4 pt-5 pb-10">
          <header className="flex flex-wrap items-end gap-x-4 gap-y-3">
            <div className="min-w-0 flex-1">
              <h1 className="text-[18px] font-semibold tracking-[-0.045em]">Previews</h1>
              <p className="mt-0.5 font-mono text-[11px] text-subtle tabular-nums">
                {usage ? (
                  <>
                    {usage.previews} {usage.previews === 1 ? "preview" : "previews"} · {money(usage.month_to_date_usd)} this month · {money(usage.sleeping_usd_month)}/mo asleep ·{" "}
                    {money(usage.active_usd_hour)}/h active
                    {!!usage.max_usd_month && (
                      <span className={(usage.spent_usd_month ?? 0) >= usage.max_usd_month * 0.8 ? "text-danger" : undefined}>
                        {" "}
                        · {money(usage.spent_usd_month ?? 0)} of the {money(usage.max_usd_month)} budget
                        {(usage.spent_usd_month ?? 0) >= usage.max_usd_month ? ": new previews wait" : ""}
                      </span>
                    )}
                  </>
                ) : (
                  " "
                )}
              </p>
            </div>
            <div className="flex gap-1.5">
              <button className={btn} onClick={() => setBranching((b) => !b)} aria-expanded={branching} disabled={!repos.length}>
                <GitBranch className="size-3.5" strokeWidth={1.5} />
                Preview a branch
              </button>
              <button className={btn} onClick={() => setRules(true)}>
                <Settings2 className="size-3.5" strokeWidth={1.5} />
                Rules
              </button>
            </div>
          </header>

          {settingsError && <p className="mt-4 rounded-lg border border-danger/40 bg-danger/10 px-3 py-2 text-[12px] text-danger">{settingsError}</p>}
          {settings && !settings.enabled && (
            <div className="rise mt-4 flex flex-wrap items-center gap-3 rounded-xl border border-line-2 bg-panel px-3 py-2.5 inset-shadow-highlight">
              <p className="min-w-0 flex-1 text-[12.5px] text-muted">
                Previews are off for {project.name}. Changes and pull requests get none until they're on; you can still preview a branch.
              </p>
              <button onClick={turnOn} className="h-7 rounded-md bg-fg px-2.5 text-[12px] font-medium text-bg transition-opacity hover:opacity-90">
                Turn on
              </button>
            </div>
          )}

          {branching && (
            <BranchForm
              projectId={project.id}
              repos={repos}
              act={act}
              onDone={(p) => {
                setBranching(false);
                put(p);
                setOpenId(p.id);
              }}
              onCancel={() => setBranching(false)}
            />
          )}

          {error && !list && <p className="mt-4 rounded-lg border border-danger/40 bg-danger/10 px-3 py-2 text-[12px] text-danger">Previews didn't load: {error}</p>}
          {list && list.length > 0 && (
            <ul className="mt-4 overflow-hidden rounded-xl border border-line-2 bg-panel inset-shadow-highlight" aria-label="Previews">
              {list.map((p) => (
                <Row key={p.id} preview={p} open={p.id === openId} onOpen={() => setOpenId(p.id)} />
              ))}
            </ul>
          )}
          {list && list.length === 0 && <Empty repos={repos} onBranch={() => setBranching(true)} />}
          {!list && !error && <p className="mt-6 text-[12.5px] text-subtle">Loading…</p>}
        </div>
      </div>

      <Inspector open={!!opened} onClose={() => setOpenId(null)}>
        {opened && (
          <PreviewSheet
            key={opened.id}
            preview={opened}
            names={names}
            masked={new Map(Object.entries(settings?.masking ?? {}).map(([k, cols]) => [k, Object.keys(cols).length]))}
            act={act}
            onChange={put}
            onClose={() => setOpenId(null)}
            feedback={opened.id === initial ? feedback : undefined}
            onFeedbackPosted={onFeedbackPosted}
          />
        )}
      </Inspector>

      <PreviewSettingsDialog
        projectId={project.id}
        resources={data}
        names={names}
        settings={settings}
        open={rules}
        onClose={() => setRules(false)}
        onSaved={(s) => {
          setSettings(s);
          void reload();
        }}
      />
    </>
  );
}

function Row({ preview: p, open, onOpen }: { preview: Preview; open: boolean; onOpen: () => void }) {
  const what = previewOf(p);
  return (
    <li className={`relative border-t border-line first:border-t-0 transition-colors ${open ? "bg-fg/[0.06]" : "hover:bg-fg/[0.03]"}`}>
      <button onClick={onOpen} aria-label={`${p.title}: ${STATE_LABEL[p.state]}`} className="absolute inset-0 outline-none focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-accent" />
      <div className="pointer-events-none relative px-3 py-2.5">
        <div className="flex items-center gap-2 text-[13px]">
          <StateDot state={p.state} />
          <span className="min-w-0 truncate font-medium text-soft">{p.title}</span>
          {what && <span className="shrink-0 font-mono text-[11px] text-muted">{what}</span>}
          {p.baseline && (
            <span className="shrink-0 rounded border border-line-2 px-1 text-[10.5px] text-muted" title="roll keeps it to compare the project's changes with">
              baseline
            </span>
          )}
          <span className="min-w-0 truncate font-mono text-[11px] text-subtle max-sm:hidden">{p.repo}</span>
          <span className="ml-auto shrink-0 text-[11px] text-subtle" title={new Date(p.last_active_at).toLocaleString()}>
            {p.state === "ready" || p.state === "sleeping" ? `${STATE_LABEL[p.state]} · ${since(p.last_active_at)}` : STATE_LABEL[p.state]}
          </span>
          {p.screenshot_at ? <Thumb id={p.id} at={p.screenshot_at} /> : null}
        </div>
        <div className="mt-1.5 flex flex-wrap items-center gap-x-3 gap-y-1 pl-3.5">
          {p.services.map((s) => (
            <a
              key={s.name}
              href={openUrl(s)}
              target="_blank"
              rel="noreferrer"
              className="pointer-events-auto flex min-w-0 items-center gap-1 font-mono text-[11px] text-accent hover:underline"
              aria-label={`Open ${s.name}`}
            >
              <span className="text-muted">{s.name}</span>
              <span className="max-w-[300px] truncate">{host(s.url)}</span>
              <ExternalLink className="size-3 shrink-0" strokeWidth={1.5} />
            </a>
          ))}
          {!p.services.length && <span className="text-[11px] text-subtle">No services</span>}
          <span className="ml-auto font-mono text-[10.5px] text-subtle tabular-nums">
            {money(p.cost.sleeping_usd_month)}/mo asleep · {money(p.cost.active_usd_hour)}/h active · {money(p.cost.month_to_date_usd)} so far
          </span>
        </div>
        {p.error && <p className="mt-1 truncate pl-3.5 text-[11.5px] text-danger">{p.error}</p>}
      </div>
    </li>
  );
}

/** The first service's page, small, beside the row (the preview's screenshot). */
function Thumb({ id, at }: { id: string; at: number }) {
  const [src, setSrc] = useState<string | null>(null);
  useEffect(() => {
    let url: string | null = null;
    let live = true;
    screenshotUrl(id).then(
      (u) => {
        url = u;
        if (live) setSrc(u);
        else URL.revokeObjectURL(u);
      },
      () => {},
    );
    return () => {
      live = false;
      if (url) URL.revokeObjectURL(url);
    };
  }, [id, at]);
  if (!src) return null;
  return <img src={src} alt="" className="h-9 w-14 shrink-0 rounded border border-line-2 object-cover object-top max-sm:hidden" />;
}

function Empty({ repos, onBranch }: { repos: string[]; onBranch: () => void }) {
  return (
    <div className="rise mt-10 grid place-items-center px-4 text-center">
      <span className="grid size-9 place-items-center rounded-lg border border-line-2 bg-raised inset-shadow-highlight">
        <GitBranch className="size-4" strokeWidth={1.5} />
      </span>
      <h2 className="mt-3 text-[15px] font-semibold tracking-[-0.03em]">No previews yet</h2>
      <p className="mt-1 max-w-[440px] text-[12.5px] text-muted">
        {repos.length ? (
          <>
            A change or pull request in <span className="font-mono text-soft">{repos.join(", ")}</span> gets its own copy of this project: every service at its head and a branch of
            every database, asleep when nobody looks.
          </>
        ) : (
          "Add a repository to this project: each change or pull request in it gets its own copy of the project, with a branch of every database."
        )}
      </p>
      {repos.length > 0 && (
        <button onClick={onBranch} className={`${btn} mt-4`}>
          <GitBranch className="size-3.5" strokeWidth={1.5} />
          Preview a branch
        </button>
      )}
    </div>
  );
}

function BranchForm({
  projectId,
  repos,
  act,
  onDone,
  onCancel,
}: {
  projectId: string;
  repos: string[];
  act: <T>(fn: () => Promise<T>) => Promise<T>;
  onDone: (p: Preview) => void;
  onCancel: () => void;
}) {
  const [repo, setRepo] = useState(repos[0] ?? "");
  const [branch, setBranch] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  return (
    <form
      className="rise mt-4 rounded-xl border border-line-2 bg-panel p-3 inset-shadow-highlight"
      onSubmit={async (e) => {
        e.preventDefault();
        if (!repo || !branch.trim() || busy) return;
        setBusy(true);
        setError(null);
        try {
          onDone(await act(() => previewsApi.create(projectId, repo, { branch: branch.trim() })));
        } catch (err) {
          setError(errorText(err));
        } finally {
          setBusy(false);
        }
      }}
      onKeyDown={(e) => e.key === "Escape" && onCancel()}
    >
      <div className="flex flex-wrap gap-2">
        <select aria-label="Repository" value={repo} onChange={(e) => setRepo(e.target.value)} className={`${field} min-w-0 font-mono max-sm:w-full`}>
          {repos.map((r) => (
            <option key={r} value={r}>
              {r}
            </option>
          ))}
        </select>
        <input
          autoFocus
          aria-label="Branch"
          placeholder="branch"
          value={branch}
          onChange={(e) => setBranch(e.target.value)}
          className={`${field} min-w-[140px] flex-1 font-mono`}
          spellCheck={false}
        />
        <button type="submit" disabled={!repo || !branch.trim() || busy} className="h-8 rounded-lg bg-fg px-3 text-[12.5px] font-medium text-bg transition-opacity hover:opacity-90 disabled:opacity-40">
          {busy ? "Creating…" : "Create preview"}
        </button>
        <button type="button" onClick={onCancel} className="h-8 rounded-lg px-2.5 text-[12.5px] text-muted hover:bg-white/[0.05] hover:text-fg">
          Cancel
        </button>
      </div>
      <p className="mt-2 text-[11px] text-subtle">It runs until you delete it or its time without a push or visit runs out.</p>
      {error && <p className="mt-2 rounded-lg border border-danger/40 bg-danger/10 px-2.5 py-1.5 text-[12px] text-danger">{error}</p>}
    </form>
  );
}
