import { Play, RotateCcw } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { parseConsole, searchApi, type ConsoleResponse, type SearchCluster } from "@/lib/search-api";
import { formatJson } from "@/lib/search-docs";
import { useMarkState } from "../mark-state";
import { btn, CopyButton, Panel, primary, useCopy } from "../ui";
import { JsonText } from "./json-view";
import { RequestEditor, type RequestEditorHandle } from "./request-editor";

/** Requests to start from, in Kibana Dev Tools syntax. */
const EXAMPLES: { label: string; text: string }[] = [
  { label: "Cluster info", text: "GET /" },
  { label: "Health", text: "GET /_cluster/health" },
  { label: "Indices", text: "GET /_cat/indices?v&s=index" },
  {
    label: "Create an index",
    text: `PUT /products
{
  "settings": { "number_of_shards": 1 },
  "mappings": {
    "properties": {
      "title": { "type": "text" },
      "price": { "type": "float" },
      "tags":  { "type": "keyword" }
    }
  }
}`,
  },
  {
    label: "Bulk index",
    text: `POST /products/_bulk?refresh=true
{ "index": { "_id": "1" } }
{ "title": "Trail running shoes", "price": 129.0, "tags": ["outdoor"] }
{ "index": { "_id": "2" } }
{ "title": "Road running shoes", "price": 99.5, "tags": ["road"] }`,
  },
  {
    label: "Search",
    text: `GET /products/_search
{
  "query": { "match": { "title": "running shoes" } },
  "aggs": { "tags": { "terms": { "field": "tags" } } }
}`,
  },
  { label: "Mappings", text: "GET /products/_mapping" },
  { label: "Analyze", text: `POST /_analyze\n{\n  "analyzer": "standard",\n  "text": "Quick brown foxes"\n}` },
];

interface Entry {
  text: string;
  response: ConsoleResponse | null;
  error: string | null;
}

const HISTORY_KEY = (id: string) => `roll.search.console.${id}`;

/** JSON re-indented token by token (numbers and key order exactly as sent), anything else as is. */
function pretty(r: ConsoleResponse) {
  if (!r.json || !r.body) return r.body;
  return formatJson(r.body);
}

/** The request as a curl command (the password stays a variable: the dashboard never has it). */
export function curlOf(text: string, cluster: SearchCluster) {
  let parsed: ReturnType<typeof parseConsole>;
  try {
    parsed = parseConsole(text);
  } catch {
    return text;
  }
  const url = `${cluster.url}${parsed.path}`;
  const head = parsed.method === "HEAD" ? "-I" : `-X ${parsed.method}`;
  let cmd = `curl -u ${cluster.username}:$ELASTICSEARCH_PASSWORD ${head} '${url.replace(/'/g, "'\\''")}'`;
  if (parsed.body !== undefined) {
    const data = parsed.body;
    cmd += ` \\\n  -H 'content-type: application/${parsed.ndjson ? "x-ndjson" : "json"}' --data-binary '${data.replace(/'/g, "'\\''")}'`;
  }
  return cmd;
}

function statusTone(status: number) {
  if (status >= 500) return "text-danger";
  if (status >= 400) return "text-danger";
  return "text-ok";
}

/** A Dev Tools-style console: one request, its response, and a short history kept in the browser. */
export function SearchConsole({ cluster, prefill }: { cluster: SearchCluster; prefill?: { text: string; n: number } | null }) {
  const { setState, play } = useMarkState();
  const { copied, copy } = useCopy();
  const [text, setText] = useState(prefill?.text ?? EXAMPLES[2].text);
  const [running, setRunning] = useState(false);
  const [current, setCurrent] = useState<Entry | null>(null);
  const [history, setHistory] = useState<string[]>([]);
  const area = useRef<RequestEditorHandle>(null);

  useEffect(() => {
    if (prefill) {
      setText(prefill.text);
      area.current?.focus();
    }
  }, [prefill]);

  useEffect(() => {
    try {
      const saved = JSON.parse(localStorage.getItem(HISTORY_KEY(cluster.id)) ?? "[]");
      if (Array.isArray(saved)) setHistory(saved.filter((s) => typeof s === "string").slice(0, 20));
    } catch {
      // A corrupt entry: start over.
    }
  }, [cluster.id]);

  const remember = (t: string) => {
    setHistory((h) => {
      const next = [t, ...h.filter((x) => x !== t)].slice(0, 20);
      try {
        localStorage.setItem(HISTORY_KEY(cluster.id), JSON.stringify(next));
      } catch {
        // Storage full or disabled: history stays in memory.
      }
      return next;
    });
  };

  const send = async () => {
    if (running) return;
    let parsed: ReturnType<typeof parseConsole>;
    try {
      parsed = parseConsole(text);
    } catch (e) {
      setCurrent({ text, response: null, error: e instanceof Error ? e.message : String(e) });
      play([["error", 1200]]);
      return;
    }
    setRunning(true);
    setState("thinking");
    try {
      const r = await searchApi.request(cluster.id, parsed.method, parsed.path, parsed.body);
      setCurrent({ text, response: r, error: null });
      remember(text);
      play([[r.status < 400 ? "success" : "error", 1200]]);
    } catch (e) {
      setCurrent({ text, response: null, error: e instanceof Error ? e.message : String(e) });
      play([["error", 1400]]);
    } finally {
      setRunning(false);
    }
  };

  const r = current?.response;
  const out = r ? pretty(r) : "";

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap gap-1">
        {EXAMPLES.map((e) => (
          <button
            key={e.label}
            onClick={() => {
              setText(e.text);
              area.current?.focus();
            }}
            className="rounded-full border border-line px-2.5 py-0.5 text-[11px] text-muted transition hover:border-line-2 hover:text-fg"
          >
            {e.label}
          </button>
        ))}
      </div>
      <div className="grid gap-4 lg:grid-cols-2">
        <Panel
          title="Request"
          aside={
            <span className="flex items-center gap-2">
              <span className="hidden text-[11px] text-subtle sm:inline">Ctrl + Space suggests · ⌘/Ctrl + Enter sends</span>
              <button onClick={send} disabled={running} className={primary}>
                {running ? (
                  <span className="shimmer">Running…</span>
                ) : (
                  <>
                    <Play className="size-3.5" /> Send
                  </>
                )}
              </button>
            </span>
          }
        >
          <RequestEditor ref={area} cluster={cluster} value={text} onChange={setText} onSubmit={send} />
          {history.length > 0 && (
            <div className="border-t border-line px-5 py-3">
              <p className="mb-1.5 flex items-center gap-1.5 text-[11px] text-subtle">
                <RotateCcw className="size-3" /> Recent
              </p>
              <ul className="space-y-0.5">
                {history.slice(0, 6).map((h) => (
                  <li key={h}>
                    <button onClick={() => setText(h)} className="w-full truncate text-left font-mono text-[11px] text-muted hover:text-fg">
                      {h.split("\n")[0]}
                    </button>
                  </li>
                ))}
              </ul>
            </div>
          )}
        </Panel>
        <Panel
          title="Response"
          aside={
            r ? (
              <span className="flex items-center gap-3 font-mono text-[12px]">
                <span className={statusTone(r.status)}>{r.status}</span>
                <span className="text-subtle">{r.took_ms} ms</span>
                <CopyButton id="resp" text={out} copied={copied} onCopy={copy} />
                {current && (
                  <button
                    onClick={() => copy("curl", curlOf(current.text, cluster))}
                    className="font-sans text-[11px] text-subtle transition hover:text-fg"
                    title="Copy the request as a curl command"
                  >
                    {copied === "curl" ? "copied" : "curl"}
                  </button>
                )}
              </span>
            ) : null
          }
        >
          {current?.error ? (
            <p className="rise px-5 py-4 text-[13px] text-danger">{current.error}</p>
          ) : r ? (
            <>
              <pre className="rise max-h-[32rem] min-h-80 flex-1 overflow-auto px-5 py-4 font-mono text-[12px] leading-relaxed text-muted">
                {out ? <JsonText text={out} json={r.json} /> : <span className="text-subtle">(no body)</span>}
              </pre>
              {r.truncated && <p className="border-t border-line px-5 py-2 text-[11px] text-subtle">Cut at 4 MiB.</p>}
            </>
          ) : (
            <p className="px-5 py-4 text-[13px] text-subtle">
              {running ? <span className="shimmer">Waking the cluster and running the request…</span> : "Send a request to see the response."}
            </p>
          )}
        </Panel>
      </div>
      <p className="text-[12px] text-subtle">
        Requests run as the cluster&apos;s owner through the same limits as any client, and wake{" "}
        <span className="text-muted">{cluster.name}</span> if it&apos;s asleep. The body is JSON, or one JSON document
        per line for <span className="font-mono">_bulk</span> and <span className="font-mono">_msearch</span>.
      </p>
      {current && (
        <button onClick={() => setCurrent(null)} className={btn}>
          Clear response
        </button>
      )}
    </div>
  );
}
