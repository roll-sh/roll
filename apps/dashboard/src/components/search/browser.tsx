import { Globe, KeyRound, Plus } from "lucide-react";
import { useEffect, useState } from "react";
import { limitOf, parseOrigins, quickstart, reachOf, type Quickstart } from "@/lib/search-browser";
import { searchApi, type BrowserKey, type NewBrowserKey, type SearchCluster } from "@/lib/search-api";
import { Field, Segmented, type Run } from "../redis/shared";
import { btn, CopyButton, ErrorNote, input, Panel, primary, SecretField, useCopy } from "../ui";

const EXPIRY = [
  { label: "Never", value: 0 },
  { label: "1 day", value: 86_400 },
  { label: "30 days", value: 30 * 86_400 },
  { label: "90 days", value: 90 * 86_400 },
  { label: "1 year", value: 365 * 86_400 },
];

const SNIPPETS: { label: string; value: Quickstart }[] = [
  { label: "Search application", value: "search-application" },
  { label: "Search UI", value: "search-ui" },
  { label: "InstantSearch (Searchkit)", value: "instantsearch" },
];

function day(s: number) {
  return new Date(s * 1000).toLocaleDateString("en-US", { month: "short", day: "numeric", year: "numeric" });
}

/**
 * Browser search (Settings): which web pages may call the cluster (`http.cors.*`), the keys those
 * pages ship (read-only, one search application or named indices, an optional filter, an expiry
 * and a rate limit), and the page code to start from.
 */
export function BrowserSearch({ cluster, busy, run }: { cluster: SearchCluster; busy: string | null; run: Run }) {
  // A preflight carries no credentials: only the cluster's own host name tells the node which
  // cluster it's for.
  const ownHost = cluster.host.startsWith(`${cluster.name}.`);
  return (
    <Panel title="Browser search" aside={<span className="text-[12px] text-subtle">no backend in between</span>}>
      <div className="divide-y divide-line">
        <p className="px-5 py-4 text-[13px] text-muted">
          A web page can search this cluster directly: allow its origin, give it a browser key, and it calls{" "}
          <span className="font-mono text-fg">{cluster.url}</span> from the visitor&apos;s browser. Browser traffic is metered like any other, and a sleeping cluster
          wakes for it; each key&apos;s rate limit is the guard.
          {!ownHost && (
            <span className="mt-2 block text-[12px] text-danger">
              This node serves every cluster at one address (no <span className="font-mono">--search-domain</span>), so a browser&apos;s preflight can&apos;t name the
              cluster: pages can&apos;t call it until clusters have host names of their own.
            </span>
          )}
          {cluster.ip_allowlist.length > 0 && (
            <span className="mt-2 block text-[12px] text-subtle">
              The network allowlist above still applies: visitors outside it are refused before anything else, preflights included.
            </span>
          )}
        </p>
        <Origins cluster={cluster} busy={busy} run={run} />
        <Keys cluster={cluster} />
      </div>
    </Panel>
  );
}

function Origins({ cluster, busy, run }: { cluster: SearchCluster; busy: string | null; run: Run }) {
  const saved = (cluster.cors?.allow_origin ?? []).join("\n");
  const [text, setText] = useState(saved);
  useEffect(() => setText(saved), [saved]);
  const { origins, errors } = parseOrigins(text);
  return (
    <div className="space-y-3 px-5 py-4">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h3 className="text-[13px] font-medium">Allowed origins</h3>
          <p className="mt-1 max-w-[60ch] text-[12px] text-muted">
            {origins.length
              ? `Pages on ${origins.length === 1 ? "this origin" : `these ${origins.length} origins`} may call the cluster; any other gets a 403. Without credentials a page still can't read anything.`
              : "Off: browsers refuse to let other sites' pages read the cluster's answers, as on a fresh Elasticsearch."}
          </p>
        </div>
        <button
          type="button"
          className={btn}
          disabled={busy !== null || text === saved || errors.length > 0}
          onClick={() => run("cors", () => searchApi.update(cluster.id, { cors: origins.length ? { ...(cluster.cors ?? {}), allow_origin: origins } : null }))}
        >
          {busy === "cors" ? <span className="shimmer">Saving…</span> : "Save"}
        </button>
      </div>
      <textarea
        value={text}
        onChange={(e) => setText(e.target.value)}
        rows={Math.min(6, Math.max(2, text.split("\n").length))}
        placeholder={"https://shop.example.com\nhttp://localhost:3000\n/https://.*\\.preview\\.example\\.com/"}
        spellCheck={false}
        aria-label="Allowed origins, one per line"
        className={`${input} w-full font-mono text-[12.5px]`}
      />
      {errors.length > 0 ? (
        <p className="text-[12px] text-danger">{errors[0]}</p>
      ) : (
        <p className="text-[12px] text-subtle">
          <span className="font-mono">scheme://host[:port]</span>, <span className="font-mono">/regex/</span> (matching the whole origin) or{" "}
          <span className="font-mono">*</span>, one a line. Shown as Elasticsearch&apos;s <span className="font-mono">http.cors.*</span> settings; cookies are never allowed.
        </p>
      )}
    </div>
  );
}

function Keys({ cluster }: { cluster: SearchCluster }) {
  const { copied, copy } = useCopy();
  const [keys, setKeys] = useState<BrowserKey[] | null>(null);
  const [name, setName] = useState("");
  const [kind, setKind] = useState<"application" | "indices">("application");
  const [target, setTarget] = useState("");
  const [filter, setFilter] = useState("");
  const [expiry, setExpiry] = useState(0);
  const [perMinute, setPerMinute] = useState("600");
  const [perIp, setPerIp] = useState("");
  const [made, setMade] = useState<NewBrowserKey | null>(null);
  const [revoking, setRevoking] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [snippet, setSnippet] = useState<Quickstart>("search-application");
  const opensearch = cluster.flavor === "opensearch";

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
  const load = () => act("load", async () => setKeys((await searchApi.browserKeys(cluster.id)).keys));

  const create = async () => {
    let parsed: Record<string, unknown> | undefined;
    if (filter.trim()) {
      try {
        parsed = JSON.parse(filter);
      } catch {
        setError("The filter isn't JSON: write one query, e.g. {\"term\": {\"public\": true}}");
        return;
      }
    }
    const names = target
      .split(",")
      .map((s) => s.trim())
      .filter(Boolean);
    const r = await act("create", () =>
      searchApi.createBrowserKey(cluster.id, {
        name: name.trim(),
        ...(kind === "application" ? { application: names[0] } : { indices: names }),
        filter: parsed,
        expires_in_s: expiry || undefined,
        rate_limit: { per_minute: Number(perMinute) || undefined, per_ip_per_minute: Number(perIp) || undefined },
      }),
    );
    if (r) {
      setMade(r.value);
      setSnippet(r.value.application ? "search-application" : "instantsearch");
      setName("");
      if (keys) await load();
    }
  };

  const snippetTarget = made?.application ?? made?.indices?.[0] ?? (target.split(",")[0]?.trim() || (kind === "application" ? "my-app" : "products"));
  const code = quickstart(snippet, cluster.url, snippetTarget, made?.api_key);

  if (opensearch) {
    return (
      <p className="px-5 py-4 text-[13px] text-muted">
        Browser keys are API keys, which OpenSearch clients don&apos;t send: make them on an Elasticsearch-flavor cluster.
      </p>
    );
  }

  return (
    <>
      <div className="space-y-4 px-5 py-4">
        <div className="flex flex-wrap items-start justify-between gap-3">
          <div>
            <h3 className="text-[13px] font-medium">Browser keys</h3>
            <p className="mt-1 max-w-[60ch] text-[12px] text-muted">
              Safe to put in a page: they only search, never write, manage or see other keys. One limited to a search application can only run its template.
            </p>
          </div>
          <button onClick={load} disabled={busy !== null} className={btn} title="Lists the cluster's browser keys (wakes it)">
            {busy === "load" ? <span className="shimmer">Loading…</span> : keys ? "Refresh" : "Show keys"}
          </button>
        </div>
        <form
          className="grid gap-3 sm:grid-cols-2"
          onSubmit={(e) => {
            e.preventDefault();
            void create();
          }}
        >
          <Field label="Name">
            <input value={name} onChange={(e) => setName(e.target.value)} placeholder="shop search" className={`${input} w-full`} maxLength={1024} />
          </Field>
          <Field label="Reaches">
            <div className="flex min-w-0 gap-2">
              <Segmented
                value={kind}
                options={[
                  { label: "Application", value: "application" },
                  { label: "Indices", value: "indices" },
                ]}
                onChange={setKind}
              />
              <input
                value={target}
                onChange={(e) => setTarget(e.target.value)}
                placeholder={kind === "application" ? "shop-search" : "products, offers"}
                aria-label={kind === "application" ? "Search application" : "Indices"}
                className={`${input} min-w-0 flex-1 font-mono text-[12.5px]`}
              />
            </div>
          </Field>
          <Field label="Filter (optional)" hint="A query every search is filtered by, as a secured key's filters">
            <input
              value={filter}
              onChange={(e) => setFilter(e.target.value)}
              placeholder='{"term": {"public": true}}'
              spellCheck={false}
              className={`${input} w-full font-mono text-[12.5px]`}
            />
          </Field>
          <div className="grid grid-cols-3 gap-3">
            <Field label="Per minute">
              <input value={perMinute} onChange={(e) => setPerMinute(e.target.value)} inputMode="numeric" className={`${input} w-full font-mono`} />
            </Field>
            <Field label="Per address">
              <input value={perIp} onChange={(e) => setPerIp(e.target.value)} inputMode="numeric" placeholder="any" className={`${input} w-full font-mono`} />
            </Field>
            <Field label="Expires">
              <select value={expiry} onChange={(e) => setExpiry(Number(e.target.value))} className={`${input} w-full`}>
                {EXPIRY.map((o) => (
                  <option key={o.value} value={o.value}>
                    {o.label}
                  </option>
                ))}
              </select>
            </Field>
          </div>
          <div className="flex justify-end sm:col-span-2">
            <button type="submit" disabled={busy !== null || !name.trim() || !target.trim()} className={primary}>
              {busy === "create" ? (
                <span className="shimmer">Creating…</span>
              ) : (
                <>
                  <Plus className="size-3.5" /> Create browser key
                </>
              )}
            </button>
          </div>
        </form>
        <ErrorNote error={error} />

        {made && (
          <div className="rise space-y-3 rounded-lg border border-line bg-bg p-4">
            <div className="flex items-center justify-between gap-3">
              <span className="flex items-center gap-2 text-[13px] font-medium">
                <KeyRound className="size-4 text-accent" /> {made.name}
              </span>
              <span className="text-[12px] text-subtle">shown once</span>
            </div>
            <SecretField id="browser-key" value={made.api_key} masked="••••••••••••••••••••••••" copied={copied} onCopy={copy} />
            <div className="flex flex-wrap items-center gap-2 text-[12px] text-muted">
              <span>{reachOf(made)}</span>
              <span>· {limitOf(made)}</span>
              {made.expires_at && <span>· expires {day(made.expires_at)}</span>}
              <button onClick={() => setMade(null)} className={`${primary} ml-auto`}>
                I&apos;ve saved it
              </button>
            </div>
          </div>
        )}

        {keys && (
          <div className="overflow-x-auto rounded-lg border border-line">
            <table className="w-full text-[13px]">
              <thead className="text-left text-[11px] text-subtle">
                <tr className="border-b border-line">
                  <th className="px-3 py-2 font-normal">Name</th>
                  <th className="px-3 py-2 font-normal">Reaches</th>
                  <th className="px-3 py-2 font-normal">Filter</th>
                  <th className="px-3 py-2 font-normal">Limit</th>
                  <th className="px-3 py-2 font-normal">Expires</th>
                  <th className="px-3 py-2" />
                </tr>
              </thead>
              <tbody>
                {keys.length === 0 && (
                  <tr>
                    <td colSpan={6} className="px-3 py-3 text-muted">
                      No browser keys yet.
                    </td>
                  </tr>
                )}
                {keys.map((k) => (
                  <tr key={k.id} className="border-b border-line last:border-0">
                    <td className="max-w-56 px-3 py-2">
                      <div className="truncate">{k.name ?? <span className="text-subtle">unnamed</span>}</div>
                      <div className="truncate font-mono text-[11px] text-subtle">{k.id}</div>
                    </td>
                    <td className="px-3 py-2 text-muted">{reachOf(k)}</td>
                    <td className="max-w-56 truncate px-3 py-2 font-mono text-[11px] text-muted" title={k.filter ? JSON.stringify(k.filter) : undefined}>
                      {k.filter ? JSON.stringify(k.filter) : "—"}
                    </td>
                    <td className="whitespace-nowrap px-3 py-2 font-mono text-[12px] text-muted">{limitOf(k)}</td>
                    <td className="whitespace-nowrap px-3 py-2 font-mono text-[12px] text-muted">{k.expires_at ? day(k.expires_at) : "never"}</td>
                    <td className="whitespace-nowrap px-3 py-2 text-right">
                      {revoking === k.id ? (
                        <span className="inline-flex items-center gap-2">
                          <span className="text-[12px] text-muted">Pages using it fail at once.</span>
                          <button
                            disabled={busy !== null}
                            onClick={async () => {
                              const r = await act("revoke", () => searchApi.revokeBrowserKey(cluster.id, k.id));
                              setRevoking(null);
                              if (r) await load();
                            }}
                            className={`${btn} hover:border-danger/50 hover:text-danger`}
                          >
                            {busy === "revoke" ? <span className="shimmer">Revoking…</span> : "Revoke"}
                          </button>
                          <button onClick={() => setRevoking(null)} className={btn}>
                            Cancel
                          </button>
                        </span>
                      ) : (
                        <button onClick={() => setRevoking(k.id)} className={btn}>
                          Revoke
                        </button>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>

      <div>
        <div className="flex items-center justify-between gap-3 px-5 pt-4">
          <h3 className="flex items-center gap-2 text-[13px] font-medium">
            <Globe className="size-3.5 text-muted" strokeWidth={1.5} /> Quickstart
          </h3>
          <CopyButton id="browser-snippet" text={code} copied={copied} onCopy={copy} />
        </div>
        <div className="flex gap-1 overflow-x-auto px-4 py-2">
          {SNIPPETS.map((s) => (
            <button
              key={s.value}
              onClick={() => setSnippet(s.value)}
              className={`shrink-0 rounded-md px-2 py-1 font-mono text-[11px] transition ${s.value === snippet ? "bg-panel-2 text-fg" : "text-subtle hover:text-fg"}`}
            >
              {s.label}
            </button>
          ))}
        </div>
        <pre className="overflow-x-auto px-5 pb-4 font-mono text-[12px] leading-relaxed text-muted">{code}</pre>
        <p className="px-5 pb-4 text-[12px] text-subtle">
          {snippet === "search-application"
            ? "Needs a key limited to the application: it runs only the application's search template."
            : "Search UI and Searchkit send whole queries, so they need a key that reads indices (a search application's name is an alias of its indices)."}
        </p>
      </div>
    </>
  );
}
