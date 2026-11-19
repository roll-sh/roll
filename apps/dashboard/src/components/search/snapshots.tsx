import { Camera, ChevronRight, Plus, RotateCcw, X } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { formatBytes } from "@/lib/roll-api";
import { esJson, searchApi, type SearchCluster } from "@/lib/search-api";
import {
  formatMillis,
  patternList,
  recoveryProgress,
  REPO_TYPES,
  repositoryBody,
  repositoryProblem,
  repositoryWhere,
  RESTORE_DEFAULTS,
  restoredNames,
  restoreProblem,
  restoreRequest,
  slmRows,
  snapshotName,
  snapshotRows,
  type RecoveryProgress,
  type Repository,
  type RepoType,
  type RepoValues,
  type RestoreForm,
  type SlmPolicyInfo,
  type SlmRow,
  type SnapshotInfo,
  type SnapshotRow,
} from "@/lib/search-snapshots";
import { Field, Segmented } from "../redis/shared";
import { ago, btn, ErrorNote, input, Panel, primary } from "../ui";
import { Check, ConsoleButton, DeleteButton, useBusy } from "./actions";
import { RequestPreview } from "./request-preview";
import { Policies } from "./slm-policies";

const enc = encodeURIComponent;
/** roll's default repository: an `s3` repository on roll's side (`Repository::roll_default`). */
const rollManaged = (r: Repository) => r.type === "s3" && r.settings?.client === "roll";

/** A cluster's repositories, snapshots and snapshot lifecycle policies, polled while one runs. */
function useSnapshots(cluster: SearchCluster) {
  const opensearch = cluster.flavor === "opensearch";
  const [repos, setRepos] = useState<Record<string, Repository> | null>(null);
  const [repoErrors, setRepoErrors] = useState<Record<string, string>>({});
  const [snapshots, setSnapshots] = useState<SnapshotRow[]>([]);
  const [policies, setPolicies] = useState<SlmRow[] | null>(null);
  const [slmMode, setSlmMode] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const r = await esJson<Record<string, Repository>>(cluster.id, "GET", "/_snapshot");
      const errors: Record<string, string> = {};
      const lists = await Promise.all(
        Object.keys(r).map(async (name): Promise<[string, SnapshotInfo[]]> => {
          try {
            const q = opensearch ? "" : "?index_details=true";
            return [name, (await esJson<{ snapshots: SnapshotInfo[] }>(cluster.id, "GET", `/_snapshot/${enc(name)}/_all${q}`)).snapshots];
          } catch (e) {
            errors[name] = e instanceof Error ? e.message : String(e);
            return [name, []];
          }
        }),
      );
      setRepos(r);
      setRepoErrors(errors);
      setSnapshots(snapshotRows(Object.fromEntries(lists)));
      if (!opensearch) {
        const [p, s] = await Promise.all([
          esJson<Record<string, SlmPolicyInfo>>(cluster.id, "GET", "/_slm/policy"),
          esJson<{ operation_mode: string }>(cluster.id, "GET", "/_slm/status").catch(() => null),
        ]);
        setPolicies(slmRows(p));
        setSlmMode(s?.operation_mode ?? null);
      }
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setRepos((r) => r ?? {});
    }
  }, [cluster.id, opensearch]);

  useEffect(() => {
    load();
  }, [load]);

  // While a snapshot is being taken (or a policy runs), the list follows it.
  const running = snapshots.some((s) => s.state === "IN_PROGRESS") || !!policies?.some((p) => p.info.in_progress);
  useEffect(() => {
    if (!running) return;
    const t = setInterval(load, 2000);
    return () => clearInterval(t);
  }, [running, load]);

  return { repos, repoErrors, snapshots, policies, slmMode, error, load };
}

/**
 * Snapshot and restore (Kibana's "Snapshot and Restore"): repositories registered with their
 * settings, verified and removed; snapshots with their state, indices, size and failures, taken
 * by hand or by a policy, deleted, and restored (indices, a rename, aliases, global state,
 * partial) with the restored shards' recovery followed to the end; snapshot lifecycle policies
 * with their schedule, retention and last runs, run on demand. Every request goes through the
 * cluster's console route and can be opened in the console.
 */
export function SnapshotsTab({ cluster, onOpenConsole }: { cluster: SearchCluster; onOpenConsole: (text: string) => void }) {
  const data = useSnapshots(cluster);
  const { repos } = data;
  if (!repos) return <p className="text-[13px] text-subtle"><span className="shimmer">Loading snapshots…</span></p>;
  return (
    <div className="space-y-6">
      <ErrorNote error={data.error} />
      <Repositories cluster={cluster} repos={repos} errors={data.repoErrors} counts={countBy(data.snapshots)} reload={data.load} onOpenConsole={onOpenConsole} />
      <Snapshots cluster={cluster} repos={repos} snapshots={data.snapshots} reload={data.load} onOpenConsole={onOpenConsole} />
      {cluster.flavor === "opensearch" ? (
        <Panel title="Policies">
          <p className="px-5 py-4 text-[13px] text-muted">
            OpenSearch&apos;s snapshot management policies (<span className="font-mono">_plugins/_sm</span>) aren&apos;t served by roll yet. Take snapshots here
            by hand, or from an ISM policy&apos;s <span className="font-mono">snapshot</span> action.
          </p>
        </Panel>
      ) : (
        <Policies cluster={cluster} repos={repos} policies={data.policies ?? []} mode={data.slmMode} reload={data.load} onOpenConsole={onOpenConsole} />
      )}
    </div>
  );
}

function countBy(rows: SnapshotRow[]) {
  const m: Record<string, number> = {};
  for (const r of rows) m[r.repository] = (m[r.repository] ?? 0) + 1;
  return m;
}

// --- Repositories ------------------------------------------------------------------------------

function Repositories({
  cluster,
  repos,
  errors,
  counts,
  reload,
  onOpenConsole,
}: {
  cluster: SearchCluster;
  repos: Record<string, Repository>;
  errors: Record<string, string>;
  counts: Record<string, number>;
  reload: () => Promise<void>;
  onOpenConsole: (text: string) => void;
}) {
  const { busy, error, act } = useBusy(reload);
  const [adding, setAdding] = useState(false);
  const [verified, setVerified] = useState<Record<string, string>>({});
  const names = Object.keys(repos).sort();

  const verify = (name: string) =>
    act(
      `verify:${name}`,
      async () => {
        const r = await esJson<{ nodes: Record<string, unknown> }>(cluster.id, "POST", `/_snapshot/${enc(name)}/_verify`);
        const n = Object.keys(r.nodes ?? {}).length;
        setVerified((v) => ({ ...v, [name]: `verified on ${n} node${n === 1 ? "" : "s"}` }));
      },
      false,
    );

  return (
    <>
      <Panel
        title="Repositories"
        aside={
          !adding && (
            <button onClick={() => setAdding(true)} className={btn}>
              <Plus className="size-3.5" /> Register
            </button>
          )
        }
      >
        {names.length === 0 ? (
          <p className="px-5 py-4 text-[13px] text-muted">No repositories. Register one to keep snapshots in a bucket or file system of your own.</p>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full text-[13px]">
              <thead>
                <tr className="border-b border-line text-left text-[11px] text-subtle">
                  <th className="px-5 py-2 font-normal">Repository</th>
                  <th className="px-3 py-2 font-normal">Type</th>
                  <th className="px-3 py-2 font-normal">Where</th>
                  <th className="px-3 py-2 text-right font-normal">Snapshots</th>
                  <th className="px-5 py-2" />
                </tr>
              </thead>
              <tbody>
                {names.map((name) => {
                  const r = repos[name];
                  const managed = rollManaged(r);
                  return (
                    <tr key={name} className="border-b border-line align-top last:border-0">
                      <td className="max-w-64 px-5 py-2.5">
                        <div className="truncate font-mono text-[12px]">{name}</div>
                        {managed && <div className="text-[11px] text-subtle">managed by roll</div>}
                        {r.settings?.readonly === "true" || r.settings?.readonly === true ? <div className="text-[11px] text-subtle">read-only</div> : null}
                        {errors[name] && <div className="mt-1 max-w-md text-[12px] whitespace-normal text-danger">{errors[name]}</div>}
                        {verified[name] && !errors[name] && <div className="text-[11px] text-ok">{verified[name]}</div>}
                      </td>
                      <td className="px-3 py-2.5 font-mono text-[12px] text-muted">{r.type}</td>
                      <td className="max-w-72 truncate px-3 py-2.5 font-mono text-[12px] text-muted">{managed ? "roll's storage" : repositoryWhere(r) || "—"}</td>
                      <td className="px-3 py-2.5 text-right font-mono text-[12px] tabular-nums">{errors[name] ? "—" : (counts[name] ?? 0)}</td>
                      <td className="px-5 py-2 text-right whitespace-nowrap">
                        <span className="inline-flex items-center gap-1">
                          <button onClick={() => verify(name)} disabled={busy !== null} className="rounded-md px-1.5 py-0.5 text-[12px] text-muted transition hover:text-fg disabled:opacity-40">
                            {busy === `verify:${name}` ? <span className="shimmer">Verifying…</span> : "Verify"}
                          </button>
                          <ConsoleButton text={`GET /_snapshot/${enc(name)}`} onOpenConsole={onOpenConsole} />
                          {!managed && <DeleteButton label={name} busy={busy === `delete:${name}`} onDelete={() => act(`delete:${name}`, () => esJson(cluster.id, "DELETE", `/_snapshot/${enc(name)}`))} />}
                        </span>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
        {error && (
          <div className="px-5 pb-4">
            <ErrorNote error={error} />
          </div>
        )}
        <p className="border-t border-line px-5 py-3 text-[12px] text-subtle">
          Unregistering a repository leaves its snapshots where they are: register it again to see them.
        </p>
      </Panel>
      {adding && <RegisterRepository cluster={cluster} existing={names} onClose={() => setAdding(false)} onDone={async () => (setAdding(false), await reload())} onOpenConsole={onOpenConsole} />}
    </>
  );
}

function RegisterRepository({ cluster, existing, onClose, onDone, onOpenConsole }: { cluster: SearchCluster; existing: string[]; onClose: () => void; onDone: () => Promise<void>; onOpenConsole: (text: string) => void }) {
  const [name, setName] = useState("");
  const [type, setType] = useState<RepoType>("s3");
  const [values, setValues] = useState<RepoValues>({});
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const spec = REPO_TYPES.find((t) => t.type === type)!;
  const body = repositoryBody(type, values);
  const problem = repositoryProblem(name, type, values, existing);
  const path = `/_snapshot/${enc(name.trim() || "my-repository")}`;

  return (
    <Panel
      title="Register a repository"
      className="rise"
      aside={
        <button onClick={onClose} className={btn} aria-label="Close">
          <X className="size-3.5" />
        </button>
      }
    >
      <form
        className="space-y-4 px-5 py-4"
        onSubmit={async (e) => {
          e.preventDefault();
          setBusy(true);
          setError(null);
          try {
            // Elasticsearch verifies a repository as it registers it (`verify` defaults to true).
            await esJson(cluster.id, "PUT", path, body);
            await onDone();
          } catch (err) {
            setError(err instanceof Error ? err.message : String(err));
          } finally {
            setBusy(false);
          }
        }}
      >
        <div className="grid gap-3 sm:grid-cols-[minmax(0,16rem)_minmax(0,1fr)]">
          <Field label="Name">
            <input value={name} onChange={(e) => setName(e.target.value)} placeholder="backups" className={`${input} w-full font-mono`} spellCheck={false} autoFocus />
          </Field>
          <Field label="Type">
            <select value={type} onChange={(e) => setType(e.target.value as RepoType)} className={`${input} w-full`}>
              {REPO_TYPES.map((t) => (
                <option key={t.type} value={t.type}>
                  {t.label} ({t.type})
                </option>
              ))}
            </select>
          </Field>
        </div>
        <div className="grid gap-3 sm:grid-cols-2">
          {spec.settings
            .filter((s) => s.kind === "text")
            .map((s) => (
              <Field key={s.key} label={s.required ? s.label : `${s.label} (optional)`}>
                <input value={String(values[s.key] ?? "")} onChange={(e) => setValues((v) => ({ ...v, [s.key]: e.target.value }))} placeholder={s.placeholder} className={`${input} w-full font-mono`} spellCheck={false} />
              </Field>
            ))}
        </div>
        <div className="flex flex-wrap gap-x-5 gap-y-2">
          {spec.settings
            .filter((s) => s.kind === "bool")
            .map((s) => (
              <Check key={s.key} checked={typeof values[s.key] === "boolean" ? (values[s.key] as boolean) : !!s.default} onChange={(v) => setValues((vs) => ({ ...vs, [s.key]: v }))}>
                {s.label}
              </Check>
            ))}
        </div>
        {type === "s3" && (
          <p className="text-[12px] text-subtle">
            The bucket&apos;s credentials are the client&apos;s secure settings (<span className="font-mono">s3.client.&lt;client&gt;.access_key</span>), set through the keystore
            API, never in the repository&apos;s settings.
          </p>
        )}
        <RequestPreview method="PUT" path={path} body={body} onOpenConsole={onOpenConsole} />
        <div className="flex flex-wrap items-center gap-3">
          <button type="submit" disabled={busy || !!problem} className={primary}>
            {busy ? <span className="shimmer">Registering…</span> : "Register"}
          </button>
          {problem && name.trim() && <span className="text-[12px] text-danger">{problem}</span>}
        </div>
        <ErrorNote error={error} />
      </form>
    </Panel>
  );
}

// --- Snapshots ---------------------------------------------------------------------------------

function StateLabel({ state }: { state: string }) {
  const tone = state === "SUCCESS" ? "text-ok" : state === "IN_PROGRESS" ? "text-accent shimmer" : state === "FAILED" || state === "PARTIAL" ? "text-danger" : "text-muted";
  return <span className={`text-[12px] ${tone}`}>{state.toLowerCase().replace("_", " ")}</span>;
}

function Snapshots({ cluster, repos, snapshots, reload, onOpenConsole }: { cluster: SearchCluster; repos: Record<string, Repository>; snapshots: SnapshotRow[]; reload: () => Promise<void>; onOpenConsole: (text: string) => void }) {
  const { busy, error, act } = useBusy(reload);
  const [taking, setTaking] = useState(false);
  const [open, setOpen] = useState<string | null>(null);
  const [repoFilter, setRepoFilter] = useState("");
  const writable = Object.keys(repos)
    .filter((n) => repos[n].type !== "url" && String(repos[n].settings?.readonly ?? "false") !== "true")
    .sort();
  const shown = repoFilter ? snapshots.filter((s) => s.repository === repoFilter) : snapshots;
  const key = (s: SnapshotRow) => `${s.repository}/${s.name}`;
  const selected = shown.find((s) => key(s) === open) ?? null;

  return (
    <>
      <Panel
        title="Snapshots"
        aside={
          <span className="flex items-center gap-2">
            {Object.keys(repos).length > 1 && (
              <select value={repoFilter} onChange={(e) => setRepoFilter(e.target.value)} aria-label="Repository" className={`${input} py-1 text-[12px]`}>
                <option value="">Every repository</option>
                {Object.keys(repos)
                  .sort()
                  .map((n) => (
                    <option key={n} value={n}>
                      {n}
                    </option>
                  ))}
              </select>
            )}
            {!taking && writable.length > 0 && (
              <button onClick={() => setTaking(true)} className={btn}>
                <Camera className="size-3.5" /> Take snapshot
              </button>
            )}
          </span>
        }
      >
        {shown.length === 0 ? (
          <p className="px-5 py-4 text-[13px] text-muted">
            {Object.keys(repos).length === 0 ? "Register a repository first: snapshots live in one." : "No snapshots yet. Take one, or add a policy that takes them on a schedule."}
          </p>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full text-[13px]">
              <thead>
                <tr className="border-b border-line text-left text-[11px] text-subtle">
                  <th className="px-5 py-2 font-normal">Snapshot</th>
                  <th className="px-3 py-2 font-normal">State</th>
                  <th className="px-3 py-2 text-right font-normal">Indices</th>
                  <th className="px-3 py-2 text-right font-normal">Size</th>
                  <th className="px-3 py-2 text-right font-normal">Taken</th>
                  <th className="px-3 py-2 text-right font-normal">Duration</th>
                  <th className="px-5 py-2" />
                </tr>
              </thead>
              <tbody>
                {shown.map((s) => {
                  const k = key(s);
                  const expanded = open === k;
                  return (
                    <SnapshotRowView
                      key={k}
                      s={s}
                      expanded={expanded}
                      onToggle={() => setOpen(expanded ? null : k)}
                      deleting={busy === `delete:${k}`}
                      onDelete={() => act(`delete:${k}`, () => esJson(cluster.id, "DELETE", `/_snapshot/${enc(s.repository)}/${enc(s.name)}`))}
                      onOpenConsole={onOpenConsole}
                    />
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
        {selected && (
          <div className="rise border-t border-line px-5 py-4">
            <SnapshotDetails key={key(selected)} s={selected} cluster={cluster} onClose={() => setOpen(null)} onOpenConsole={onOpenConsole} />
          </div>
        )}
        {error && (
          <div className="px-5 pb-4">
            <ErrorNote error={error} />
          </div>
        )}
      </Panel>
      {taking && (
        <TakeSnapshot
          cluster={cluster}
          repos={writable}
          onClose={() => setTaking(false)}
          onDone={async () => {
            setTaking(false);
            await reload();
          }}
          onOpenConsole={onOpenConsole}
        />
      )}
    </>
  );
}

function SnapshotRowView({
  s,
  expanded,
  onToggle,
  deleting,
  onDelete,
  onOpenConsole,
}: {
  s: SnapshotRow;
  expanded: boolean;
  onToggle: () => void;
  deleting: boolean;
  onDelete: () => void;
  onOpenConsole: (text: string) => void;
}) {
  const failed = s.shards.failed;
  return (
    <tr className={`border-b border-line last:border-0 ${expanded ? "bg-panel-2" : ""}`}>
      <td className="max-w-80 px-5 py-2.5">
        <button onClick={onToggle} className="flex min-w-0 items-center gap-1.5 text-left" aria-expanded={expanded}>
          <ChevronRight className={`size-3.5 shrink-0 text-subtle transition-transform duration-200 motion-reduce:transition-none ${expanded ? "rotate-90" : ""}`} />
          <span className="truncate font-mono text-[12px]">{s.name}</span>
        </button>
        <div className="truncate pl-5 font-mono text-[11px] text-subtle">
          {s.repository}
          {s.policy && <> · policy {s.policy}</>}
        </div>
      </td>
      <td className="px-3 py-2.5 whitespace-nowrap">
        <StateLabel state={s.state} />
        {failed > 0 && <div className="font-mono text-[11px] text-danger">{failed} shard{failed === 1 ? "" : "s"} failed</div>}
      </td>
      <td className="px-3 py-2.5 text-right font-mono text-[12px] tabular-nums">{s.indices.length}</td>
      <td className="px-3 py-2.5 text-right font-mono text-[12px] whitespace-nowrap tabular-nums">{s.sizeBytes === null ? "—" : formatBytes(s.sizeBytes)}</td>
      <td className="px-3 py-2.5 text-right text-[12px] whitespace-nowrap text-muted" title={s.startMs ? new Date(s.startMs).toISOString() : undefined}>
        {s.startMs ? ago(Math.floor(s.startMs / 1000)) : "—"}
      </td>
      <td className="px-3 py-2.5 text-right font-mono text-[12px] whitespace-nowrap tabular-nums">{formatMillis(s.durationMs)}</td>
      <td className="px-5 py-2 text-right whitespace-nowrap">
        <span className="inline-flex items-center gap-1">
          <ConsoleButton text={`GET /_snapshot/${enc(s.repository)}/${enc(s.name)}`} onOpenConsole={onOpenConsole} />
          {s.state !== "IN_PROGRESS" && <DeleteButton label={s.name} busy={deleting} onDelete={onDelete} />}
        </span>
      </td>
    </tr>
  );
}

function SnapshotDetails({ s, cluster, onClose, onOpenConsole }: { s: SnapshotRow; cluster: SearchCluster; onClose: () => void; onOpenConsole: (text: string) => void }) {
  const [restoring, setRestoring] = useState(false);
  return (
    <div className="space-y-3">
      <div className="flex items-center justify-between gap-3">
        <h3 className="min-w-0 truncate font-mono text-[13px] font-medium">{s.name}</h3>
        <button onClick={onClose} className="rounded-md p-1 text-subtle transition hover:text-fg" aria-label="Close snapshot">
          <X className="size-3.5" />
        </button>
      </div>
      <dl className="grid gap-x-6 gap-y-2 text-[12px] sm:grid-cols-[auto_minmax(0,1fr)]">
        <dt className="text-subtle">Indices</dt>
        <dd className="font-mono break-all text-muted">{s.indices.length ? s.indices.join(", ") : "none"}</dd>
        {s.dataStreams.length > 0 && (
          <>
            <dt className="text-subtle">Data streams</dt>
            <dd className="font-mono break-all text-muted">{s.dataStreams.join(", ")}</dd>
          </>
        )}
        <dt className="text-subtle">Shards</dt>
        <dd className="font-mono text-muted">
          {s.shards.successful} of {s.shards.total} successful
        </dd>
        <dt className="text-subtle">Global state</dt>
        <dd className="text-muted">{s.globalState ? "included (templates, pipelines, persistent settings)" : "not included"}</dd>
        {s.failures.length > 0 && (
          <>
            <dt className="text-subtle">Failures</dt>
            <dd className="space-y-1 font-mono text-danger">
              {s.failures.map((f, i) => (
                <div key={i} className="break-all">
                  {f}
                </div>
              ))}
            </dd>
          </>
        )}
      </dl>
      {(s.state === "SUCCESS" || s.state === "PARTIAL") &&
        (restoring ? (
          <RestorePanel s={s} cluster={cluster} onClose={() => setRestoring(false)} onOpenConsole={onOpenConsole} />
        ) : (
          <button onClick={() => setRestoring(true)} className={btn}>
            <RotateCcw className="size-3.5" /> Restore
          </button>
        ))}
    </div>
  );
}

function TakeSnapshot({ cluster, repos, onClose, onDone, onOpenConsole }: { cluster: SearchCluster; repos: string[]; onClose: () => void; onDone: () => Promise<void>; onOpenConsole: (text: string) => void }) {
  const [repo, setRepo] = useState(repos[0] ?? "");
  const [name, setName] = useState(() => snapshotName(new Date()));
  const [indices, setIndices] = useState("");
  const [globalState, setGlobalState] = useState(true);
  const [partial, setPartial] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const list = patternList(indices);
  const body = { ...(list.length ? { indices: list.join(",") } : {}), include_global_state: globalState, ...(partial ? { partial: true } : {}) };
  const path = `/_snapshot/${enc(repo)}/${enc(name.trim())}`;

  return (
    <Panel
      title="Take a snapshot"
      className="rise"
      aside={
        <button onClick={onClose} className={btn} aria-label="Close">
          <X className="size-3.5" />
        </button>
      }
    >
      <form
        className="space-y-4 px-5 py-4"
        onSubmit={async (e) => {
          e.preventDefault();
          setBusy(true);
          setError(null);
          try {
            await esJson(cluster.id, "PUT", path, body);
            await onDone();
          } catch (err) {
            setError(err instanceof Error ? err.message : String(err));
          } finally {
            setBusy(false);
          }
        }}
      >
        <div className="grid gap-3 sm:grid-cols-3">
          <Field label="Repository">
            <select value={repo} onChange={(e) => setRepo(e.target.value)} className={`${input} w-full font-mono text-[12px]`}>
              {repos.map((r) => (
                <option key={r} value={r}>
                  {r}
                </option>
              ))}
            </select>
          </Field>
          <Field label="Name">
            <input value={name} onChange={(e) => setName(e.target.value)} className={`${input} w-full font-mono`} spellCheck={false} />
          </Field>
          <Field label="Indices" hint="patterns; empty for every index and data stream">
            <input value={indices} onChange={(e) => setIndices(e.target.value)} placeholder="logs-*, products" className={`${input} w-full font-mono`} spellCheck={false} />
          </Field>
        </div>
        <div className="flex flex-wrap gap-x-5 gap-y-2">
          <Check checked={globalState} onChange={setGlobalState}>
            Include global state
          </Check>
          <Check checked={partial} onChange={setPartial}>
            Allow partial (skip unavailable shards)
          </Check>
        </div>
        <RequestPreview method="PUT" path={path} body={body} onOpenConsole={onOpenConsole} />
        <div className="flex items-center gap-3">
          <button type="submit" disabled={busy || !repo || !name.trim()} className={primary}>
            {busy ? <span className="shimmer">Starting…</span> : "Take snapshot"}
          </button>
        </div>
        <ErrorNote error={error} />
      </form>
    </Panel>
  );
}

// --- Restore -----------------------------------------------------------------------------------

function RestorePanel({ s, cluster, onClose, onOpenConsole }: { s: SnapshotRow; cluster: SearchCluster; onClose: () => void; onOpenConsole: (text: string) => void }) {
  const [form, setForm] = useState<RestoreForm>(RESTORE_DEFAULTS);
  const [open, setOpen] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [restored, setRestored] = useState<string[] | null>(null);
  const set = (patch: Partial<RestoreForm>) => setForm((f) => ({ ...f, ...patch }));

  useEffect(() => {
    searchApi.indices(cluster.id).then(
      (rows) => setOpen(rows.filter((r) => r.status === "open").map((r) => r.index)),
      () => setOpen([]),
    );
  }, [cluster.id]);

  const req = restoreRequest(s.repository, s.name, form);
  const problem = restoreProblem(form, s.indices, open);
  const picked = form.indices ?? [];
  const toggle = (i: string) => set({ indices: picked.includes(i) ? picked.filter((x) => x !== i) : [...picked, i] });

  if (restored) return <RestoreProgress cluster={cluster} indices={restored} onClose={onClose} />;

  return (
    <form
      className="rise space-y-4 rounded-lg border border-line bg-panel p-4"
      onSubmit={async (e) => {
        e.preventDefault();
        setBusy(true);
        setError(null);
        try {
          await esJson(cluster.id, "POST", req.path, req.body);
          setRestored(restoredNames(form.indices ?? s.indices.filter((i) => !i.startsWith(".")), form.rename));
        } catch (err) {
          setError(err instanceof Error ? err.message : String(err));
        } finally {
          setBusy(false);
        }
      }}
    >
      <div className="flex items-center justify-between gap-3">
        <h3 className="text-[13px] font-medium">Restore</h3>
        <button type="button" onClick={onClose} className="rounded-md p-1 text-subtle transition hover:text-fg" aria-label="Close restore">
          <X className="size-3.5" />
        </button>
      </div>
      <Field label="Indices">
        <Segmented
          value={form.indices ? "some" : "all"}
          options={[
            { label: "all", value: "all" },
            { label: "pick", value: "some" },
          ]}
          onChange={(v) => set({ indices: v === "all" ? null : [] })}
        />
      </Field>
      {form.indices && (
        <div className="flex max-h-48 flex-wrap gap-x-4 gap-y-1.5 overflow-y-auto">
          {s.indices.map((i) => (
            <Check key={i} checked={picked.includes(i)} onChange={() => toggle(i)}>
              <span className="font-mono text-[12px]">{i}</span>
            </Check>
          ))}
        </div>
      )}
      <Check checked={!!form.rename} onChange={(v) => set({ rename: v ? { pattern: "(.+)", replacement: "restored-$1" } : null })}>
        Rename on restore
      </Check>
      {form.rename && (
        <div className="grid gap-3 sm:grid-cols-2">
          <Field label="Pattern" hint="a regular expression over each name">
            <input value={form.rename.pattern} onChange={(e) => set({ rename: { ...form.rename!, pattern: e.target.value } })} className={`${input} w-full font-mono`} spellCheck={false} />
          </Field>
          <Field label="Replacement" hint={`${s.indices[0] ?? "index"} → ${restoredNames(s.indices.slice(0, 1), form.rename)[0] ?? ""}`}>
            <input value={form.rename.replacement} onChange={(e) => set({ rename: { ...form.rename!, replacement: e.target.value } })} className={`${input} w-full font-mono`} spellCheck={false} />
          </Field>
        </div>
      )}
      <div className="flex flex-wrap gap-x-5 gap-y-2">
        <Check checked={form.includeAliases} onChange={(v) => set({ includeAliases: v })}>
          Include aliases
        </Check>
        <Check checked={form.includeGlobalState} onChange={(v) => set({ includeGlobalState: v })}>
          Include global state
        </Check>
        <Check checked={form.partial} onChange={(v) => set({ partial: v })}>
          Partial (restore what was snapshotted)
        </Check>
      </div>
      <RequestPreview method="POST" path={req.path} body={req.body} onOpenConsole={onOpenConsole} />
      <div className="flex flex-wrap items-center gap-3">
        <button type="submit" disabled={busy || !!problem} className={primary}>
          {busy ? <span className="shimmer">Starting…</span> : "Restore"}
        </button>
        {problem && <span className="text-[12px] text-danger">{problem}</span>}
      </div>
      <ErrorNote error={error} />
    </form>
  );
}

/** The restored indices' shard recovery (`GET <indices>/_recovery`), followed until every one is done. */
function RestoreProgress({ cluster, indices, onClose }: { cluster: SearchCluster; indices: string[]; onClose: () => void }) {
  const [rows, setRows] = useState<RecoveryProgress[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const path = `/${indices.map(enc).join(",")}/_recovery`;
  const done = rows !== null && rows.length >= indices.length && rows.every((r) => r.done);

  useEffect(() => {
    if (done) return;
    let alive = true;
    const poll = async () => {
      try {
        const r = await esJson<Record<string, { shards?: [] }>>(cluster.id, "GET", `${path}?ignore_unavailable=true`);
        if (alive) setRows(recoveryProgress(r));
      } catch (e) {
        if (alive) setError(e instanceof Error ? e.message : String(e));
      }
    };
    poll();
    const t = setInterval(poll, 1000);
    return () => {
      alive = false;
      clearInterval(t);
    };
  }, [cluster.id, path, done]);

  const byIndex = new Map((rows ?? []).map((r) => [r.index, r]));
  return (
    <div className="rise space-y-3 rounded-lg border border-line bg-panel p-4">
      <div className="flex items-center justify-between gap-3">
        <h3 className="text-[13px] font-medium">{done ? "Restored" : <span className="shimmer">Restoring…</span>}</h3>
        <button type="button" onClick={onClose} className={btn}>
          {done ? "Done" : "Hide"}
        </button>
      </div>
      <div className="space-y-2">
        {indices.map((i) => {
          const p = byIndex.get(i);
          return (
            <div key={i} className="grid grid-cols-[minmax(0,1fr)_4rem] items-center gap-x-3 gap-y-1">
              <span className="truncate font-mono text-[12px]">{i}</span>
              <span className="text-right font-mono text-[11px] text-muted tabular-nums">{p ? (p.done ? "done" : `${p.percent.toFixed(0)}%`) : "waiting"}</span>
              <div className="col-span-2 h-1 overflow-hidden rounded-full bg-line">
                <div className={`h-full rounded-full transition-[width] duration-300 motion-reduce:transition-none ${p?.done ? "bg-ok" : "bg-accent"}`} style={{ width: `${p?.percent ?? 0}%` }} />
              </div>
            </div>
          );
        })}
      </div>
      <ErrorNote error={error} />
    </div>
  );
}
