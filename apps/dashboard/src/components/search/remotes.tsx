import { Link2, Plus } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { esJson, searchApi, type SearchCluster } from "@/lib/search-api";
import { linkBody, linkTargets, parsePatterns, remoteRows, stateLabel, suggestAlias, validAlias, type RemoteInfo, type RemoteLink, type RemoteRow } from "@/lib/search-remotes";
import { Field } from "../redis/shared";
import { btn, ErrorNote, input, Panel, primary } from "../ui";

function StateDot({ state }: { state: RemoteRow["state"] }) {
  const tone = state === "connected" ? "bg-ok" : state === "unknown" ? "border border-line-2 bg-transparent" : "bg-danger";
  return <span className={`inline-block size-2 shrink-0 rounded-full ${tone}`} />;
}

/**
 * Remote clusters (cross-cluster search): another of the person's own clusters, searched from
 * this one as `<alias>:<index>`. Linking makes a cross-cluster API key on the target that roll
 * keeps sealed; unlinking removes the remote and revokes that key. The links come from roll's
 * catalog (no wake); how each is set and whether it connects comes from the cluster's
 * `_remote/info`, read when the cluster is awake or on request.
 */
export function Remotes({ cluster }: { cluster: SearchCluster }) {
  const [links, setLinks] = useState<RemoteLink[] | null>(null);
  const [info, setInfo] = useState<Record<string, RemoteInfo> | null>(null);
  const [targets, setTargets] = useState<SearchCluster[] | null>(null);
  const [adding, setAdding] = useState(false);
  const [alias, setAlias] = useState("");
  const [target, setTarget] = useState("");
  const [patterns, setPatterns] = useState("");
  const [skip, setSkip] = useState(true);
  const [unlinking, setUnlinking] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const awake = cluster.status.state === "loaded";

  const act = async <T,>(label: string, f: () => Promise<T>): Promise<{ value: T } | null> => {
    setBusy(label);
    setError(null);
    try {
      return { value: await f() };
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      return null;
    } finally {
      setBusy(null);
    }
  };
  const load = useCallback(
    async (wake: boolean) => {
      const [l, i] = await Promise.all([searchApi.remotes(cluster.id), wake ? esJson<Record<string, RemoteInfo>>(cluster.id, "GET", "/_remote/info") : Promise.resolve(null)]);
      setLinks(l.remotes);
      if (i) setInfo(i);
    },
    [cluster.id],
  );
  // `_remote/info` only from an awake cluster: waking it is the person's call.
  useEffect(() => {
    load(awake).catch((e) => setError(e instanceof Error ? e.message : String(e)));
  }, [load, awake]);

  const rows = links ? remoteRows(links, info) : null;
  const open = async () => {
    setAdding(true);
    const r = await act("targets", () => searchApi.list());
    if (r) {
      const t = linkTargets(r.value, cluster);
      setTargets(t);
      if (t[0] && !target) {
        setTarget(t[0].name);
        setAlias(suggestAlias(t[0].name, rows?.map((x) => x.alias) ?? []));
      }
    }
  };

  return (
    <Panel
      title="Remote clusters"
      aside={
        <button onClick={() => act("load", () => load(true))} disabled={busy !== null} className={btn} title="Reads the cluster's _remote/info (wakes it)">
          {busy === "load" ? <span className="shimmer">Checking…</span> : "Check connections"}
        </button>
      }
    >
      <div className="space-y-4 px-5 py-4">
        <p className="max-w-[70ch] text-[13px] text-muted">
          Search another of your clusters from this one as <span className="font-mono">alias:index</span>. roll makes a cross-cluster API key on it and keeps it
          sealed; the key&apos;s index patterns are all the link reaches.
        </p>

        {rows && rows.length > 0 && (
          <div className="overflow-x-auto rounded-lg border border-line">
            <table className="w-full text-[13px]">
              <thead className="text-left text-[11px] text-subtle">
                <tr className="border-b border-line">
                  <th className="px-3 py-2 font-normal">Alias</th>
                  <th className="px-3 py-2 font-normal">Cluster</th>
                  <th className="px-3 py-2 font-normal">If unavailable</th>
                  <th className="px-3 py-2 font-normal">State</th>
                  <th className="px-3 py-2" />
                </tr>
              </thead>
              <tbody>
                {rows.map((r) => (
                  <tr key={r.alias} className="border-b border-line last:border-0">
                    <td className="px-3 py-2 font-mono text-[12.5px]">{r.alias}</td>
                    <td className="max-w-56 truncate px-3 py-2">{r.target}</td>
                    <td className="px-3 py-2 text-muted">{r.skipUnavailable === null ? "—" : r.skipUnavailable ? "skipped" : "fails the search"}</td>
                    <td className="whitespace-nowrap px-3 py-2">
                      <span className="inline-flex items-center gap-2 text-muted">
                        <StateDot state={r.state} /> {stateLabel(r.state)}
                      </span>
                    </td>
                    <td className="whitespace-nowrap px-3 py-2 text-right">
                      {!r.linked ? (
                        <span className="text-[12px] text-subtle">set in settings</span>
                      ) : unlinking === r.alias ? (
                        <span className="inline-flex items-center gap-2">
                          <span className="text-[12px] text-muted">{r.minted ? "Its key is revoked." : "Searches of it fail."}</span>
                          <button
                            disabled={busy !== null}
                            onClick={async () => {
                              const ok = await act("unlink", () => searchApi.unlinkRemote(cluster.id, r.alias));
                              setUnlinking(null);
                              if (ok) await act("load", () => load(awake || info !== null));
                            }}
                            className={`${btn} hover:border-danger/50 hover:text-danger`}
                          >
                            {busy === "unlink" ? <span className="shimmer">Unlinking…</span> : "Unlink"}
                          </button>
                          <button onClick={() => setUnlinking(null)} className={btn}>
                            Cancel
                          </button>
                        </span>
                      ) : (
                        <button onClick={() => setUnlinking(r.alias)} className={btn}>
                          Unlink
                        </button>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
        {rows && rows.length === 0 && !adding && <p className="text-[13px] text-subtle">No remote clusters.</p>}

        {adding ? (
          <form
            className="rise grid items-end gap-3 rounded-lg border border-line bg-bg p-4 sm:grid-cols-2"
            onSubmit={async (e) => {
              e.preventDefault();
              const ok = await act("link", () => searchApi.linkRemote(cluster.id, alias, linkBody(target, skip, parsePatterns(patterns))));
              if (ok) {
                setAdding(false);
                setAlias("");
                setTarget("");
                setPatterns("");
                await act("load", () => load(awake || info !== null));
              }
            }}
          >
            <Field label="Cluster">
              <select
                value={target}
                onChange={(e) => {
                  setTarget(e.target.value);
                  setAlias(suggestAlias(e.target.value, rows?.map((x) => x.alias) ?? []));
                }}
                className={`${input} w-full`}
                disabled={!targets?.length}
              >
                {targets === null && <option>Loading…</option>}
                {targets?.length === 0 && <option value="">No other {cluster.flavor === "opensearch" ? "OpenSearch" : "Elasticsearch"} clusters</option>}
                {targets?.map((t) => (
                  <option key={t.id} value={t.name}>
                    {t.name}
                  </option>
                ))}
              </select>
            </Field>
            <Field label="Alias">
              <input value={alias} onChange={(e) => setAlias(e.target.value)} className={`${input} w-full font-mono`} maxLength={64} placeholder="logs" aria-invalid={alias !== "" && !validAlias(alias)} />
            </Field>
            <Field label="Indices it may search">
              <input value={patterns} onChange={(e) => setPatterns(e.target.value)} className={`${input} w-full font-mono`} placeholder="* (every index), or logs-*, metrics-*" />
            </Field>
            <label className="flex h-9 items-center gap-2 text-[13px] text-muted">
              <input type="checkbox" checked={skip} onChange={(e) => setSkip(e.target.checked)} className="accent-accent" />
              Skip it when it can&apos;t be reached
            </label>
            <div className="flex flex-wrap items-center justify-end gap-2 sm:col-span-2">
              <span className="mr-auto text-[12px] text-subtle">{alias && !validAlias(alias) ? "Letters, digits, - and _ (at most 64)." : `Searched as ${alias || "alias"}:index`}</span>
              <button type="button" onClick={() => setAdding(false)} className={btn}>
                Cancel
              </button>
              <button type="submit" disabled={busy !== null || !target || !validAlias(alias)} className={primary}>
                {busy === "link" ? (
                  <span className="shimmer">Linking…</span>
                ) : (
                  <>
                    <Link2 className="size-3.5" /> Link
                  </>
                )}
              </button>
            </div>
          </form>
        ) : (
          <button onClick={open} disabled={busy !== null} className={btn}>
            <Plus className="size-3.5" /> Link a cluster
          </button>
        )}
        <ErrorNote error={error} />
      </div>
    </Panel>
  );
}
