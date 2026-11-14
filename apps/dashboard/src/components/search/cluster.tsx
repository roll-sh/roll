import { HorizonMark } from "@roll/brand";
import { ArrowLeft, FileUp, GitBranch, KeyRound, Moon, Plus, RefreshCw, Trash2 } from "lucide-react";
import { Link } from "@/components/link";
import { useNavigate } from "@tanstack/react-router";
import { useCallback, useEffect, useState } from "react";
import { formatBytes, formatUsd } from "@/lib/roll-api";
import {
  formatCount,
  formatDuration,
  NAME_HINT,
  NAME_RE,
  searchApi,
  type SearchCluster,
  type SearchIndex,
  type SearchInfo,
  type SearchSettings,
  type SearchUsage,
} from "@/lib/search-api";
import { AppShell } from "../app-shell";
import { formatTime, fromLocalInput, now, POLL_MS, Segmented, toLocalInput } from "../redis/shared";
import { ago, btn, Confirm, Connecting, CopyButton, EmptyPanel, ErrorNote, input, Offline, Panel, primary, Stats, useCopy } from "../ui";
import { AnalyzeTab } from "./analyze";
import { ImportPanel } from "./import";
import { NewIndexPanel } from "./new-index";
import { InferenceTab } from "./inference";
import { ApiKeys } from "./api-keys";
import { SearchConsole } from "./console";
import { IndexBrowser } from "./index-browser";
import { CredentialsOnce, SearchStateDot, settingsOf, SettingsFields, stateLabel, useAction, useSearchClusters, type Run } from "./shared";

const TABS = ["overview", "indices", "console", "analyze", "inference", "branches", "usage", "settings"] as const;
type Tab = (typeof TABS)[number];

/** Placeholder while the route's params resolve. */
export function SearchClusterLoading() {
  return (
    <AppShell crumb="search">
      <Connecting />
    </AppShell>
  );
}

export function SearchClusterPage({ id }: { id: string }) {
  const navigate = useNavigate();
  const { info, clusters, offline, refresh } = useSearchClusters();
  const { busy, error, setError, run } = useAction(refresh);
  const [tab, setTabRaw] = useState<Tab>("overview");
  const [revealed, setRevealed] = useState<SearchCluster | null>(null);
  // A request handed to the console from another tab (the index browser); `n` re-applies it.
  const [prefill, setPrefill] = useState<{ text: string; n: number } | null>(null);

  useEffect(() => {
    const h = window.location.hash.slice(1) as Tab;
    if (TABS.includes(h)) setTabRaw(h);
  }, []);
  const setTab = (t: Tab) => {
    setTabRaw(t);
    setError(null);
    window.history.replaceState(null, "", t === "overview" ? window.location.pathname : `#${t}`);
  };

  const cluster = clusters?.find((c) => c.id === id || c.name === id) ?? null;
  const crumb = (
    <>
      <Link href="/dashboard/search" className="text-muted hover:text-fg">
        search
      </Link>{" "}
      <span className="text-subtle">/</span> {cluster?.name ?? id}
    </>
  );

  if (offline) {
    return (
      <AppShell crumb={crumb}>
        <Offline />
      </AppShell>
    );
  }
  if (!clusters || !info) {
    return (
      <AppShell crumb={crumb}>
        <Connecting />
      </AppShell>
    );
  }
  if (!cluster) {
    return (
      <AppShell crumb={crumb}>
        <EmptyPanel>
          <HorizonMark state="sleep" size={40} />
          <h2 className="text-[15px] font-medium">No search cluster {id}</h2>
          <p className="max-w-md text-[13px] text-muted">It may have been deleted, or the link is wrong.</p>
          <Link href="/dashboard/search" className={btn}>
            <ArrowLeft className="size-3.5" /> All clusters
          </Link>
        </EmptyPanel>
      </AppShell>
    );
  }

  const loaded = cluster.status.state === "loaded";

  return (
    <AppShell
      crumb={crumb}
      actions={
        loaded && (
          <button
            onClick={() => run("unload", () => searchApi.unload(cluster.id))}
            disabled={busy !== null}
            className={`${btn} shrink-0`}
            title="Unload from memory now; the next request wakes it"
          >
            {busy === "unload" ? <HorizonMark state="thinking" size={14} interactive={false} /> : <Moon className="size-3.5" />}
            Sleep now
          </button>
        )
      }
    >
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div className="min-w-0">
          <h1 className="truncate text-2xl font-semibold tracking-[-0.04em]">{cluster.name}</h1>
          <p className="mt-1 flex flex-wrap items-center gap-x-2 gap-y-1 text-[13px] text-muted">
            <SearchStateDot state={cluster.status.state} />
            <span>{stateLabel(cluster.status.state)}</span>
            <span className="text-subtle">·</span>
            <span className="font-mono text-[12px] text-subtle">{cluster.id}</span>
            {cluster.parent_id && (
              <>
                <span className="text-subtle">·</span>
                <span className="flex items-center gap-1 text-[12px] text-subtle">
                  <GitBranch className="size-3" /> branch of{" "}
                  {cluster.parent_name ? (
                    <Link href={`/dashboard/search/${cluster.parent_id}`} className="text-accent hover:underline">
                      {cluster.parent_name}
                    </Link>
                  ) : (
                    <span className="font-mono">{cluster.parent_id} (deleted)</span>
                  )}
                  {cluster.parent_at != null && <> as of {formatTime(cluster.parent_at)}</>}
                </span>
              </>
            )}
          </p>
        </div>
        <p className="font-mono text-[11px] text-subtle">
          {cluster.flavor} {cluster.flavor === "opensearch" ? info.opensearch_version : info.version} · {cluster.region} · created{" "}
          {ago(cluster.created_at)}
        </p>
      </div>

      {/* overflow-x-auto turns overflow-y to auto too: a tab overlapping the hairline (-mb-px)
          would make the bar scroll by a pixel and show a scrollbar on classic-scrollbar systems. */}
      <nav className="mt-6 flex gap-1 overflow-x-auto overflow-y-hidden border-b border-line" aria-label="Cluster sections">
        {TABS.map((t) => (
          <button
            key={t}
            onClick={() => setTab(t)}
            aria-current={t === tab ? "page" : undefined}
            className={`shrink-0 border-b px-3 py-2 text-[13px] capitalize transition ${
              t === tab ? "border-fg text-fg" : "border-transparent text-muted hover:text-fg"
            }`}
          >
            {t}
            {t === "branches" && cluster.branches.length > 0 && (
              <span className="ml-1.5 font-mono text-[11px] text-subtle">{cluster.branches.length}</span>
            )}
          </button>
        ))}
      </nav>

      <ErrorNote error={error} />
      {revealed && <CredentialsOnce cluster={revealed} title={`New password for ${revealed.name}`} onDone={() => setRevealed(null)} />}

      <div className="mt-6">
        {tab === "overview" && <OverviewTab cluster={cluster} info={info} />}
        {tab === "indices" && (
          <IndicesTab
            key={cluster.id}
            cluster={cluster}
            onOpenConsole={(text) => {
              setPrefill((p) => ({ text, n: (p?.n ?? 0) + 1 }));
              setTab("console");
            }}
          />
        )}
        {tab === "console" && <SearchConsole key={cluster.id} cluster={cluster} prefill={prefill} />}
        {tab === "analyze" && <AnalyzeTab key={cluster.id} cluster={cluster} />}
        {tab === "inference" && (
          <InferenceTab
            key={cluster.id}
            cluster={cluster}
            onOpenConsole={(text) => {
              setPrefill((p) => ({ text, n: (p?.n ?? 0) + 1 }));
              setTab("console");
            }}
          />
        )}
        {tab === "branches" && (
          <BranchesTab
            cluster={cluster}
            info={info}
            busy={busy}
            run={run}
            onCreated={setRevealed}
          />
        )}
        {tab === "usage" && <UsageTab cluster={cluster} info={info} />}
        {tab === "settings" && (
          <SettingsTab
            key={cluster.id}
            cluster={cluster}
            busy={busy}
            run={run}
            onRotated={setRevealed}
            onDeleted={() => navigate({ href: "/dashboard/search" })}
          />
        )}
      </div>
    </AppShell>
  );
}

// --- Overview ----------------------------------------------------------------------------------

function OverviewTab({ cluster, info }: { cluster: SearchCluster; info: SearchInfo }) {
  const st = cluster.status;
  const on = st.state === "loaded";
  return (
    <div className="space-y-4">
      <Stats
        items={[
          {
            label: "State",
            value: stateLabel(st.state),
            hint: on
              ? cluster.idle_timeout_s === 0
                ? "always loaded"
                : `sleeps after ${formatDuration(cluster.idle_timeout_s)} without requests`
              : st.wakes_at
                ? `wakes on the next request, or at ${formatTime(st.wakes_at)} for a background job`
                : "wakes on the next request",
          },
          { label: "Indices", value: formatCount(st.indices ?? cluster.indices), hint: `${formatCount(cluster.docs)} documents` },
          {
            label: "Stored",
            value: formatBytes(cluster.stored_bytes),
            hint: cluster.parent_id ? `${formatBytes(cluster.own_bytes ?? 0)} its own, the rest shared with its parent` : "segments in object storage",
          },
          { label: "In flight", value: on ? formatCount(st.in_flight ?? 0) : "0", hint: `limit ${cluster.max_in_flight} · bodies to ${cluster.max_body_mb} MB` },
        ]}
      />
      <div className="grid gap-4 lg:grid-cols-5">
        <Connection cluster={cluster} info={info} />
        <Quickstart cluster={cluster} />
      </div>
    </div>
  );
}

function Connection({ cluster, info }: { cluster: SearchCluster; info: SearchInfo }) {
  const { copied, copy } = useCopy();
  const rows = [
    { id: "url", label: "URL", value: cluster.url },
    { id: "user", label: "Username", value: cluster.username },
  ];
  return (
    <Panel title="Connect" className="lg:col-span-2">
      <div className="space-y-4 px-5 py-4">
        {rows.map((r) => (
          <div key={r.id} className="space-y-1">
            <p className="text-[12px] text-subtle">{r.label}</p>
            <div className="flex items-center gap-1 rounded-lg border border-line bg-bg px-2.5 py-2">
              <code className="min-w-0 flex-1 truncate font-mono text-[11px]">{r.value}</code>
              <CopyButton id={r.id} text={r.value} copied={copied} onCopy={copy} />
            </div>
          </div>
        ))}
        <div className="space-y-1">
          <p className="text-[12px] text-subtle">Password</p>
          <p className="text-[12px] text-muted">
            Shown once, when the cluster was created
            {cluster.password_rotated_at ? ` (last rotated ${ago(cluster.password_rotated_at)})` : ""}. Lost it? Rotate it
            in Settings.
          </p>
        </div>
        <p className="text-[11px] text-subtle">
          Basic auth with the cluster name as the user{info.domain ? ", or any user at the cluster's own host" : ""}. Also
          accepted: <span className="font-mono">Bearer {cluster.username}_&lt;password&gt;</span> and{" "}
          <span className="font-mono">ApiKey</span> credentials.
        </p>
      </div>
    </Panel>
  );
}

const CLIENTS = ["curl", "python", "javascript", "go", "ruby"] as const;

function snippet(client: (typeof CLIENTS)[number], c: SearchCluster) {
  return c.flavor === "opensearch" ? openSearchSnippet(client, c) : elasticSnippet(client, c);
}

/** Quickstarts with OpenSearch's own clients (Elasticsearch's 8+ clients refuse a server that
 * isn't Elasticsearch). */
function openSearchSnippet(client: (typeof CLIENTS)[number], c: SearchCluster) {
  const url = c.url;
  const user = c.username;
  switch (client) {
    case "curl":
      return `curl -u ${user}:$OPENSEARCH_PASSWORD ${url}/_cluster/health

curl -u ${user}:$OPENSEARCH_PASSWORD -X PUT ${url}/products/_doc/1 \\
  -H 'content-type: application/json' -d '{"title": "Trail running shoes"}'

curl -u ${user}:$OPENSEARCH_PASSWORD "${url}/products/_search?q=title:shoes"`;
    case "python":
      return `# pip install opensearch-py
import os
from opensearchpy import OpenSearch

client = OpenSearch(hosts=["${url}"], http_auth=("${user}", os.environ["OPENSEARCH_PASSWORD"]))
client.index(index="products", id="1", body={"title": "Trail running shoes"}, refresh=True)
print(client.search(index="products", body={"query": {"match": {"title": "shoes"}}})["hits"])`;
    case "javascript":
      return `// npm i @opensearch-project/opensearch
import { Client } from "@opensearch-project/opensearch";

const client = new Client({
  node: "${url}",
  auth: { username: "${user}", password: process.env.OPENSEARCH_PASSWORD },
});
await client.index({ index: "products", id: "1", body: { title: "Trail running shoes" }, refresh: true });
console.log((await client.search({ index: "products", body: { query: { match: { title: "shoes" } } } })).body.hits);`;
    case "go":
      return `// go get github.com/opensearch-project/opensearch-go/v4
client, _ := opensearchapi.NewClient(opensearchapi.Config{
  Client: opensearch.Config{
    Addresses: []string{"${url}"},
    Username:  "${user}",
    Password:  os.Getenv("OPENSEARCH_PASSWORD"),
  },
})
info, _ := client.Info(context.Background(), nil)
fmt.Println(info)`;
    case "ruby":
      return `# gem install opensearch-ruby
require "opensearch"

client = OpenSearch::Client.new(url: "${url}", user: "${user}", password: ENV["OPENSEARCH_PASSWORD"])
client.index(index: "products", id: 1, body: { title: "Trail running shoes" }, refresh: true)
puts client.search(index: "products", q: "title:shoes")["hits"]`;
  }
}

function elasticSnippet(client: (typeof CLIENTS)[number], c: SearchCluster) {
  const url = c.url;
  const user = c.username;
  switch (client) {
    case "curl":
      return `curl -u ${user}:$ELASTICSEARCH_PASSWORD ${url}/_cluster/health

curl -u ${user}:$ELASTICSEARCH_PASSWORD -X PUT ${url}/products/_doc/1 \\
  -H 'content-type: application/json' -d '{"title": "Trail running shoes"}'

curl -u ${user}:$ELASTICSEARCH_PASSWORD "${url}/products/_search?q=title:shoes"`;
    case "python":
      return `# pip install elasticsearch
import os
from elasticsearch import Elasticsearch

es = Elasticsearch("${url}", basic_auth=("${user}", os.environ["ELASTICSEARCH_PASSWORD"]))
es.index(index="products", id="1", document={"title": "Trail running shoes"}, refresh=True)
print(es.search(index="products", query={"match": {"title": "shoes"}})["hits"])`;
    case "javascript":
      return `// npm i @elastic/elasticsearch
import { Client } from "@elastic/elasticsearch";

const es = new Client({
  node: "${url}",
  auth: { username: "${user}", password: process.env.ELASTICSEARCH_PASSWORD },
});
await es.index({ index: "products", id: "1", document: { title: "Trail running shoes" }, refresh: true });
console.log((await es.search({ index: "products", query: { match: { title: "shoes" } } })).hits);`;
    case "go":
      return `// go get github.com/elastic/go-elasticsearch/v9
es, _ := elasticsearch.NewClient(elasticsearch.Config{
  Addresses: []string{"${url}"},
  Username:  "${user}",
  Password:  os.Getenv("ELASTICSEARCH_PASSWORD"),
})
res, _ := es.Info()
fmt.Println(res)`;
    case "ruby":
      return `# gem install elasticsearch (or searchkick: ELASTICSEARCH_URL)
require "elasticsearch"

es = Elasticsearch::Client.new(url: "${url}", user: "${user}", password: ENV["ELASTICSEARCH_PASSWORD"])
es.index(index: "products", id: 1, body: { title: "Trail running shoes" }, refresh: true)
puts es.search(index: "products", q: "title:shoes")["hits"]`;
  }
}

function Quickstart({ cluster }: { cluster: SearchCluster }) {
  const [client, setClient] = useState<(typeof CLIENTS)[number]>("curl");
  const { copied, copy } = useCopy();
  const text = snippet(client, cluster);
  return (
    <Panel title="Quickstart" className="lg:col-span-3" aside={<CopyButton id="snippet" text={text} copied={copied} onCopy={copy} />}>
      <div className="flex gap-1 overflow-x-auto border-b border-line px-4 py-2">
        {CLIENTS.map((c) => (
          <button
            key={c}
            onClick={() => setClient(c)}
            className={`shrink-0 rounded-md px-2 py-1 font-mono text-[11px] transition ${
              c === client ? "bg-panel-2 text-fg" : "text-subtle hover:text-fg"
            }`}
          >
            {c}
          </button>
        ))}
      </div>
      <pre className="min-h-56 flex-1 overflow-x-auto px-5 py-4 font-mono text-[12px] leading-relaxed text-muted">{text}</pre>
    </Panel>
  );
}

// --- Indices -----------------------------------------------------------------------------------

function healthDot(h: string) {
  if (h === "green") return "bg-ok";
  if (h === "red") return "bg-danger";
  return "bg-accent";
}

function IndicesTab({ cluster, onOpenConsole }: { cluster: SearchCluster; onOpenConsole: (text: string) => void }) {
  const [rows, setRows] = useState<SearchIndex[] | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [importing, setImporting] = useState(false);
  const [creating, setCreating] = useState(false);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      setRows(await searchApi.indices(cluster.id));
      setErr(null);
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, [cluster.id]);

  // Listing wakes the cluster, so it's loaded once and on demand rather than polled.
  useEffect(() => {
    load();
  }, [load]);

  const hidden = rows?.filter((r) => r.index.startsWith(".")).length ?? 0;
  const open = rows?.find((r) => r.index === selected) ?? null;
  return (
    <div className="space-y-4">
      <Panel
        title="Indices"
        aside={
          <span className="flex gap-2">
            <button onClick={() => setCreating(true)} disabled={creating} className={btn} title="Create an index with its fields mapped" aria-label="New index">
              <Plus className="size-3.5" /> <span className="hidden sm:inline">New index</span>
            </button>
            <button onClick={() => setImporting(true)} disabled={importing} className={btn} title="Index a JSON, NDJSON or CSV file" aria-label="Import">
              <FileUp className="size-3.5" /> <span className="hidden sm:inline">Import</span>
            </button>
            <button onClick={load} disabled={loading} className={btn}>
              <RefreshCw className={`size-3.5 ${loading ? "animate-spin motion-reduce:animate-none" : ""}`} /> Refresh
            </button>
          </span>
        }
      >
        {err && <p className="px-5 py-4 text-[13px] text-danger">{err}</p>}
        {rows === null && !err ? (
          <p className="px-5 py-6 text-[13px] text-subtle">
            <span className="shimmer">{cluster.status.state === "loaded" ? "Listing indices…" : "Waking the cluster…"}</span>
          </p>
        ) : rows && rows.length === 0 ? (
          <p className="px-5 py-6 text-[13px] text-muted">
            No indices yet.{" "}
            <button onClick={() => setImporting(true)} className="text-accent hover:underline">
              Import a file
            </button>
            ,{" "}
            <button onClick={() => setCreating(true)} className="text-accent hover:underline">
              create one with its fields
            </button>
            , or index a document with any client: indices are created on the first write.
          </p>
        ) : (
          rows && (
            <div className="overflow-x-auto">
              <table className="w-full text-[13px]">
                <thead>
                  <tr className="border-b border-line text-left text-[11px] text-subtle">
                    <th className="px-5 py-2 font-normal">Index</th>
                    <th className="px-3 py-2 font-normal">Health</th>
                    <th className="px-3 py-2 text-right font-normal">Shards</th>
                    <th className="px-3 py-2 text-right font-normal">Docs</th>
                    <th className="px-3 py-2 text-right font-normal">Deleted</th>
                    <th className="px-5 py-2 text-right font-normal">Size</th>
                  </tr>
                </thead>
                <tbody>
                  {rows.map((r) => (
                    <tr
                      key={r.uuid || r.index}
                      onClick={() => setSelected(r.index === selected ? null : r.index)}
                      aria-selected={r.index === selected}
                      className={`rise cursor-pointer border-b border-line transition last:border-0 hover:bg-panel-2/60 ${
                        r.index === selected ? "bg-panel-2" : ""
                      }`}
                    >
                      <td className="max-w-80 truncate px-5 py-2.5 font-mono text-[12px]" title={r.uuid}>
                        <button
                          onClick={(e) => {
                            e.stopPropagation();
                            setSelected(r.index === selected ? null : r.index);
                          }}
                          className="max-w-full truncate text-left hover:underline"
                        >
                          {r.index}
                        </button>
                        {r.status === "close" && <span className="ml-2 text-[11px] text-subtle">closed</span>}
                      </td>
                      <td className="px-3 py-2.5">
                        <span className="flex items-center gap-1.5 text-[12px] text-muted">
                          <span className={`inline-block size-2 rounded-full ${healthDot(r.health)}`} />
                          {r.health}
                        </span>
                      </td>
                      <td className="px-3 py-2.5 text-right font-mono text-[12px] tabular-nums">
                        {r.primaries ?? "—"}
                        <span className="text-subtle"> · {r.replicas ?? "—"}r</span>
                      </td>
                      <td className="px-3 py-2.5 text-right font-mono text-[12px] tabular-nums">{formatCount(r.docs)}</td>
                      <td className="px-3 py-2.5 text-right font-mono text-[12px] text-subtle tabular-nums">{formatCount(r.deleted_docs)}</td>
                      <td className="px-5 py-2.5 text-right font-mono text-[12px] tabular-nums">{formatBytes(r.size_bytes)}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )
        )}
        <p className="border-t border-line px-5 py-3 text-[12px] text-subtle">
          Replicas are reported as Elasticsearch reports them on one node; durability comes from object storage, so yellow
          is expected.{hidden > 0 && ` ${hidden} hidden or system ${hidden === 1 ? "index" : "indices"} included.`} Select an
          index to browse its documents, mapping and settings.
        </p>
      </Panel>
      {creating && (
        <NewIndexPanel
          cluster={cluster}
          existing={rows?.map((r) => r.index) ?? []}
          onClose={() => setCreating(false)}
          onOpenConsole={onOpenConsole}
          onCreated={async (index) => {
            setCreating(false);
            await load();
            setSelected(index);
          }}
        />
      )}
      {importing && (
        <ImportPanel
          cluster={cluster}
          existing={rows?.map((r) => r.index) ?? []}
          onClose={() => setImporting(false)}
          onImported={async (index) => {
            await load();
            setSelected(index);
          }}
        />
      )}
      {open && (
        <IndexBrowser
          key={open.index}
          cluster={cluster}
          index={open}
          onClose={() => setSelected(null)}
          onDeleted={() => {
            setSelected(null);
            load();
          }}
          onChanged={load}
          onOpenConsole={onOpenConsole}
        />
      )}
    </div>
  );
}

// --- Branches ------------------------------------------------------------------------------------

function BranchesTab({
  cluster,
  info,
  busy,
  run,
  onCreated,
}: {
  cluster: SearchCluster;
  info: SearchInfo;
  busy: string | null;
  run: Run;
  onCreated: (b: SearchCluster) => void;
}) {
  const [name, setName] = useState(`${cluster.name}-dev`.slice(0, 63));
  const [mode, setMode] = useState<"now" | "past">("now");
  const history = info.history_retention_s;
  const earliest = Math.max(cluster.created_at, now() - history);
  const [at, setAt] = useState(() => Math.max(earliest, now() - 900));
  const valid = NAME_RE.test(name);

  return (
    <div className="grid gap-4 lg:grid-cols-5">
      <Panel title="Branches" aside={<span className="font-mono text-[12px] text-subtle">{cluster.branches.length}</span>} className="lg:col-span-3">
        {cluster.branches.length === 0 ? (
          <p className="px-5 py-6 text-[13px] text-muted">
            No branches yet. A branch is a new cluster with this one&apos;s indices as of now or a point in time — instant,
            because segments are shared, not copied. Use one per preview environment.
          </p>
        ) : (
          <ul>
            {cluster.branches.map((b) => (
              <li key={b.id} className="rise border-b border-line last:border-0">
                <Link href={`/dashboard/search/${b.id}`} className="flex items-center gap-3 px-5 py-3 transition hover:bg-panel-2/60">
                  <GitBranch className="size-3.5 text-subtle" />
                  <div className="min-w-0 flex-1">
                    <p className="truncate text-[13px]">{b.name}</p>
                    <p className="font-mono text-[11px] text-subtle">
                      {b.id} · data as of {b.parent_at != null ? formatTime(b.parent_at) : "—"}
                    </p>
                  </div>
                  <span className="text-[12px] text-subtle">{ago(b.created_at)}</span>
                </Link>
              </li>
            ))}
          </ul>
        )}
      </Panel>

      <Panel title="New branch" className="lg:col-span-2">
        <form
          className="space-y-4 px-5 py-4"
          onSubmit={async (e) => {
            e.preventDefault();
            if (!valid) return;
            const b = await run("branch", () => searchApi.branch(cluster.id, name, mode === "past" ? at : undefined));
            if (b) onCreated(b);
          }}
        >
          <label className="block space-y-1">
            <span className="block text-[11px] text-subtle">Name</span>
            <input
              value={name}
              onChange={(e) => setName(e.target.value.toLowerCase())}
              spellCheck={false}
              className={`${input} w-full font-mono text-[12px]`}
            />
            {name && !valid && <span className="block text-[11px] text-danger">{NAME_HINT}</span>}
          </label>
          <div className="space-y-2">
            <span className="block text-[11px] text-subtle">Data from</span>
            <Segmented
              value={mode}
              options={[
                { label: "now", value: "now" },
                { label: "a point in time", value: "past" },
              ]}
              onChange={setMode}
            />
          </div>
          {mode === "past" &&
            (history === 0 ? (
              <p className="text-[12px] text-muted">This node keeps no history (rolld --search-history 0).</p>
            ) : (
              <div className="space-y-2">
                <input
                  type="datetime-local"
                  step={1}
                  value={toLocalInput(at)}
                  min={toLocalInput(earliest)}
                  max={toLocalInput(now())}
                  onChange={(e) => {
                    const t = fromLocalInput(e.target.value);
                    if (t != null) setAt(t);
                  }}
                  className={`${input} w-full font-mono text-[12px]`}
                />
                <p className="text-[11px] text-subtle">
                  Window: {formatTime(earliest)} → now ({formatDuration(history)} of history)
                </p>
              </div>
            ))}
          <button type="submit" disabled={busy !== null || !valid || (mode === "past" && history === 0)} className={primary}>
            {busy === "branch" ? (
              <span className="shimmer">Branching…</span>
            ) : (
              <>
                <GitBranch className="size-3.5" /> Create branch
              </>
            )}
          </button>
          <p className="text-[11px] text-subtle">The branch gets its own password, shown once.</p>
        </form>
      </Panel>
    </div>
  );
}

// --- Usage ---------------------------------------------------------------------------------------

/** Dollars, keeping fractions of a cent visible: scale-to-zero bills are often tiny. */
function usd(n: number) {
  if (n === 0) return "$0";
  if (n < 0.0001) return "<$0.0001";
  return formatUsd(n);
}

function UsageTab({ cluster, info }: { cluster: SearchCluster; info: SearchInfo }) {
  const [days, setDays] = useState(30);
  const [usage, setUsage] = useState<SearchUsage | null>(null);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    const load = () =>
      searchApi.usage(cluster.id, days).then(
        (u) => {
          if (!alive) return;
          setUsage(u);
          setErr(null);
        },
        (e) => alive && setErr(e instanceof Error ? e.message : String(e)),
      );
    load();
    const t = setInterval(load, POLL_MS * 4);
    return () => {
      alive = false;
      clearInterval(t);
    };
  }, [cluster.id, days]);

  const u = usage?.days === days ? usage : null;
  const c = u?.cost;
  const p = info.pricing;
  const cheapest = c ? Math.min(c.elastic_cloud_usd, c.opensearch_service_usd, c.opensearch_serverless_usd) : 0;
  const savings = c && cheapest > 0 ? 1 - c.total_usd / cheapest : null;

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <p className="text-[13px] text-muted">
          Metered every minute while loaded, including the time background jobs (watches, transforms, ILM, rollups) keep it
          awake; storage accrues while asleep too.
        </p>
        <div className="w-48">
          <Segmented
            value={days}
            options={[
              { label: "24h", value: 1 },
              { label: "7d", value: 7 },
              { label: "30d", value: 30 },
            ]}
            onChange={setDays}
          />
        </div>
      </div>
      {err && <p className="text-[13px] text-danger">{err}</p>}
      <Stats
        items={[
          {
            label: "Loaded",
            value: u ? `${(u.loaded_seconds / 3600).toFixed(1)} h` : "—",
            hint: u && u.hours > 0 ? `${Math.min(100, (u.loaded_seconds / (u.hours * 3600)) * 100).toFixed(1)}% of the time` : undefined,
          },
          { label: "Memory", value: u ? `${u.loaded_gib_hours.toFixed(3)} GiB-h` : "—", hint: u ? `peak ${formatBytes(u.peak_loaded_bytes)}` : undefined },
          { label: "Requests", value: u ? formatCount(u.requests) : "—", hint: "every request the cluster served" },
          { label: "Storage", value: u ? `${u.stored_gib_months.toFixed(4)} GiB-mo` : "—", hint: u ? `${formatBytes(u.stored_bytes)} now` : undefined },
        ]}
      />
      <div className="grid gap-4 lg:grid-cols-5">
        <Panel
          title="AWS cost estimate"
          aside={<span className="font-mono text-[11px] text-subtle">last {days === 1 ? "24 hours" : `${days} days`}</span>}
          className="lg:col-span-3"
        >
          <div className="px-5 py-4">
            {c ? (
              <table className="w-full text-[13px]">
                <tbody>
                  {[
                    { label: "Memory", detail: `${p.usd_per_gib_hour} USD / GiB-hour loaded`, v: c.memory_usd },
                    { label: "Requests", detail: `${p.usd_per_million_requests} USD / million`, v: c.requests_usd },
                    { label: "Storage", detail: `S3 at ${p.usd_per_gb_month_s3} USD / GB-month`, v: c.storage_usd },
                  ].map((r) => (
                    <tr key={r.label} className="border-b border-line">
                      <td className="py-2.5 pr-3 align-top">
                        <p>{r.label}</p>
                        <p className="text-[11px] text-subtle">{r.detail}</p>
                      </td>
                      <td className="py-2.5 text-right align-top font-mono tabular-nums">{usd(r.v)}</td>
                    </tr>
                  ))}
                  <tr>
                    <td className="pt-3 font-medium">Total</td>
                    <td className="pt-3 text-right font-mono font-semibold tabular-nums">{usd(c.total_usd)}</td>
                  </tr>
                </tbody>
              </table>
            ) : (
              <p className="text-[13px] text-subtle">Loading…</p>
            )}
          </div>
        </Panel>
        <Panel title="Same workload elsewhere" className="lg:col-span-2">
          <div className="space-y-4 px-5 py-4">
            <CompareBars
              rows={[
                { label: "roll", usd: c?.total_usd ?? 0, accent: true },
                { label: "Elastic Cloud", usd: c?.elastic_cloud_usd ?? 0 },
                { label: "Amazon OpenSearch Service", usd: c?.opensearch_service_usd ?? 0 },
                { label: "OpenSearch Serverless", usd: c?.opensearch_serverless_usd ?? 0 },
              ]}
            />
            <p className="text-[13px]">
              {savings == null ? (
                <span className="text-subtle">—</span>
              ) : savings > 0 ? (
                <>
                  <span className="font-mono font-semibold">{savings > 0.99 ? ">99" : Math.round(savings * 100)}%</span>{" "}
                  <span className="text-muted">less than the cheapest alternative</span>
                </>
              ) : (
                <span className="text-muted">About the same as the alternatives for this workload.</span>
              )}
            </p>
            <p className="text-[11px] text-subtle">
              Always-on list prices for the hours the cluster existed: Elastic Cloud Hosted from {p.elastic_cloud_usd_per_month} USD a
              month per {p.elastic_cloud_gb_per_block} GB; an m7g.medium.search domain at {p.opensearch_instance_usd_per_hour} USD an hour
              plus gp3 storage; OpenSearch Serverless at {p.opensearch_serverless_min_ocu} OCU × {p.opensearch_serverless_usd_per_ocu_hour} USD
              an hour. us-east-1.
            </p>
          </div>
        </Panel>
      </div>
    </div>
  );
}

function CompareBars({ rows }: { rows: { label: string; usd: number; accent?: boolean }[] }) {
  const max = Math.max(...rows.map((r) => r.usd), 1e-9);
  return (
    <div className="space-y-3">
      {rows.map((r) => (
        <div key={r.label} title={`${r.label}: $${r.usd.toPrecision(3)}`}>
          <div className="flex items-baseline justify-between text-[12px]">
            <span className="text-muted">{r.label}</span>
            <span className="font-mono tabular-nums">{usd(r.usd)}</span>
          </div>
          <div className="mt-1.5 h-2 rounded-full bg-line">
            <div
              className={`h-full rounded-full transition-[width] duration-300 motion-reduce:transition-none ${r.accent ? "bg-accent" : "bg-fg/30"}`}
              style={{ width: `${r.usd > 0 ? Math.max(1.5, (r.usd / max) * 100) : 0}%` }}
            />
          </div>
        </div>
      ))}
    </div>
  );
}

// --- Settings, credentials, danger zone ----------------------------------------------------------

function SettingsTab({
  cluster,
  busy,
  run,
  onRotated,
  onDeleted,
}: {
  cluster: SearchCluster;
  busy: string | null;
  run: Run;
  onRotated: (c: SearchCluster) => void;
  onDeleted: () => void;
}) {
  const saved = settingsOf(cluster);
  const [draft, setDraft] = useState<SearchSettings>(saved);
  const [rotating, setRotating] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const changed = (Object.keys(saved) as (keyof SearchSettings)[]).filter((k) => draft[k] !== saved[k]);
  const reload = changed.includes("flavor") || changed.includes("destructive_requires_name");

  return (
    <div className="space-y-4">
      <Panel title="Settings" aside={<span className="text-[12px] text-subtle">limits apply live</span>}>
        <form
          className="space-y-5 px-5 py-4"
          onSubmit={(e) => {
            e.preventDefault();
            const patch: Partial<SearchSettings> = {};
            for (const k of changed) Object.assign(patch, { [k]: draft[k] });
            run("settings", () => searchApi.update(cluster.id, patch));
          }}
        >
          <SettingsFields value={draft} onChange={setDraft} />
          <div className="flex flex-wrap items-center justify-between gap-3 border-t border-line pt-4">
            <span className="text-[12px] text-subtle">
              {changed.length
                ? `${changed.length} unsaved change${changed.length === 1 ? "" : "s"}${reload ? " — the cluster reloads (it sleeps now and wakes on the next request)" : ""}`
                : "Up to date"}
            </span>
            <div className="flex gap-2">
              {changed.length > 0 && (
                <button type="button" onClick={() => setDraft(saved)} className={btn}>
                  Reset
                </button>
              )}
              <button type="submit" disabled={busy !== null || !changed.length} className={primary}>
                {busy === "settings" ? <span className="shimmer">Applying…</span> : "Save"}
              </button>
            </div>
          </div>
        </form>
      </Panel>

      <ApiKeys cluster={cluster} />

      <div className="grid gap-4 lg:grid-cols-2">
        <Panel title="Password">
          <div className="space-y-3 px-5 py-4">
            <p className="text-[13px] text-muted">
              Rotate the password. Requests with the old one fail from the next request on; the new one is shown once.
            </p>
            {rotating ? (
              <div className="flex gap-2">
                <button
                  disabled={busy !== null}
                  onClick={async () => {
                    const r = await run("rotate", () => searchApi.rotatePassword(cluster.id));
                    setRotating(false);
                    if (r) onRotated(r);
                  }}
                  className={primary}
                >
                  {busy === "rotate" ? <span className="shimmer">Rotating…</span> : "Rotate now"}
                </button>
                <button onClick={() => setRotating(false)} className={btn}>
                  Cancel
                </button>
              </div>
            ) : (
              <button onClick={() => setRotating(true)} className={btn}>
                <KeyRound className="size-3.5" /> Rotate password
              </button>
            )}
          </div>
        </Panel>

        <Panel title="Delete cluster">
          <div className="space-y-3 px-5 py-4">
            <p className="text-[13px] text-muted">
              Removes the cluster, its indices and its history. Branches keep working: segments they share stay until
              nothing uses them.
            </p>
            {deleting ? (
              <Confirm
                word={cluster.name}
                action="Delete cluster"
                busy={busy === "delete"}
                onCancel={() => setDeleting(false)}
                onConfirm={async () => {
                  const r = await run("delete", () => searchApi.remove(cluster.id));
                  if (r) onDeleted();
                }}
              >
                This can&apos;t be undone.
              </Confirm>
            ) : (
              <button onClick={() => setDeleting(true)} className={`${btn} hover:border-danger/50 hover:text-danger`}>
                <Trash2 className="size-3.5" /> Delete {cluster.name}
              </button>
            )}
          </div>
        </Panel>
      </div>
    </div>
  );
}
