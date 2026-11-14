import { Play, Plus, Sparkles, Trash2 } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { esJson, type SearchCluster } from "@/lib/search-api";
import { Field, Segmented } from "../redis/shared";
import { btn, ErrorNote, input, Panel, primary } from "../ui";

/** An inference endpoint as the cluster lists it (`GET _inference/_all`; never its API key). */
interface Endpoint {
  inference_id: string;
  task_type: string;
  service: string;
  service_settings?: Record<string, unknown>;
}

type Service = "openai" | "cohere" | "anthropic";

/** What each service offers here, and the model it suggests per task. */
const SERVICES: Record<Service, { label: string; tasks: Record<string, string> }> = {
  openai: { label: "OpenAI or compatible", tasks: { text_embedding: "text-embedding-3-small", completion: "gpt-4o-mini" } },
  cohere: { label: "Cohere", tasks: { text_embedding: "embed-english-v3.0", rerank: "rerank-english-v3.0", completion: "command-r" } },
  anthropic: { label: "Anthropic", tasks: { completion: "claude-sonnet-5-5" } },
};

const ID_RE = /^[a-z0-9][a-z0-9._-]{0,99}$/;

function message(e: unknown) {
  return e instanceof Error ? e.message : String(e);
}

/** One line about what a test call answered. */
function summarize(task: string, v: Record<string, unknown>): string {
  const first = (k: string) => (Array.isArray(v[k]) ? (v[k] as Record<string, unknown>[])[0] : undefined);
  if (task === "text_embedding") {
    const e = first("text_embedding")?.embedding as number[] | undefined;
    return e ? `${e.length} dimensions: [${e.slice(0, 4).map((x) => x.toFixed(4)).join(", ")}, …]` : JSON.stringify(v);
  }
  if (task === "sparse_embedding") {
    const e = (first("sparse_embedding")?.embedding ?? {}) as Record<string, number>;
    const top = Object.entries(e)
      .sort((a, b) => b[1] - a[1])
      .slice(0, 6);
    return `${Object.keys(e).length} tokens, top: ${top.map(([t, w]) => `${t} ${w.toFixed(2)}`).join(", ")}`;
  }
  if (task === "completion") return String(first("completion")?.result ?? JSON.stringify(v));
  if (task === "rerank") {
    const r = (v.rerank ?? []) as { index: number; relevance_score: number }[];
    return r.map((x) => `#${x.index} ${x.relevance_score.toFixed(3)}`).join(" · ");
  }
  return JSON.stringify(v);
}

/**
 * A cluster's inference endpoints (`_inference`): the models `semantic_text` fields embed with,
 * rerankers and completions. A new endpoint's API key goes to the cluster once, where it's
 * sealed; it's never listed or shown again. Listing wakes the cluster (only when this tab opens).
 */
export function InferenceTab({ cluster, onOpenConsole }: { cluster: SearchCluster; onOpenConsole: (text: string) => void }) {
  const [endpoints, setEndpoints] = useState<Endpoint[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [service, setService] = useState<Service>("openai");
  const [task, setTask] = useState("text_embedding");
  const [id, setId] = useState("");
  const [model, setModel] = useState(SERVICES.openai.tasks.text_embedding);
  const [apiKey, setApiKey] = useState("");
  const [url, setUrl] = useState("");
  const [dims, setDims] = useState("");
  const [maxTokens, setMaxTokens] = useState("1024");
  const [testing, setTesting] = useState<Endpoint | null>(null);
  const [sample, setSample] = useState("How do I reset my password?");
  const [docs, setDocs] = useState("Reset it from the sign-in page\nPasswords expire every 90 days");
  const [answer, setAnswer] = useState<string | null>(null);
  const [deleting, setDeleting] = useState<string | null>(null);

  const load = useCallback(() => {
    esJson<{ endpoints: Endpoint[] }>(cluster.id, "GET", "/_inference/_all").then(
      (r) => setEndpoints(r.endpoints.sort((a, b) => a.inference_id.localeCompare(b.inference_id))),
      (e) => setError(message(e)),
    );
  }, [cluster.id]);
  useEffect(() => {
    if (cluster.flavor !== "opensearch") load();
  }, [cluster.flavor, load]);

  const act = async (label: string, f: () => Promise<void>) => {
    setBusy(label);
    setError(null);
    try {
      await f();
    } catch (e) {
      setError(message(e));
    } finally {
      setBusy(null);
    }
  };

  const pickService = (s: Service) => {
    setService(s);
    const tasks = SERVICES[s].tasks;
    const t = task in tasks ? task : Object.keys(tasks)[0];
    setTask(t);
    setModel(tasks[t]);
  };
  const pickTask = (t: string) => {
    setTask(t);
    setModel(SERVICES[service].tasks[t]);
  };

  const create = () =>
    act("create", async () => {
      const settings: Record<string, unknown> = { api_key: apiKey, model_id: model };
      if (service === "openai" && url.trim()) settings.url = url.trim();
      if (service === "openai" && task === "text_embedding" && dims.trim()) settings.dimensions = Number(dims);
      const body: Record<string, unknown> = { service, service_settings: settings };
      if (service === "anthropic") body.task_settings = { max_tokens: Number(maxTokens) || 1024 };
      await esJson(cluster.id, "PUT", `/_inference/${task}/${encodeURIComponent(id)}`, body);
      setApiKey("");
      setId("");
      load();
    });

  const test = (e: Endpoint) =>
    act("test", async () => {
      setAnswer(null);
      const body = e.task_type === "rerank" ? { query: sample, input: docs.split("\n").filter(Boolean) } : { input: sample };
      const r = await esJson<Record<string, unknown>>(cluster.id, "POST", `/_inference/${e.task_type}/${encodeURIComponent(e.inference_id)}`, body);
      setAnswer(summarize(e.task_type, r));
    });

  const remove = (e: Endpoint) =>
    act("delete", async () => {
      await esJson(cluster.id, "DELETE", `/_inference/${e.task_type}/${encodeURIComponent(e.inference_id)}`);
      setDeleting(null);
      if (testing?.inference_id === e.inference_id) setTesting(null);
      load();
    });

  if (cluster.flavor === "opensearch") {
    return (
      <Panel title="Inference">
        <p className="px-5 py-4 text-[13px] text-muted">
          The inference API and <span className="font-mono">semantic_text</span> are Elasticsearch&apos;s. OpenSearch-flavor
          clusters do semantic search with ML Commons connectors and neural queries instead.
        </p>
      </Panel>
    );
  }

  const quickstart = (e: Endpoint) =>
    `PUT /articles\n${JSON.stringify({ mappings: { properties: { body: { type: "semantic_text", inference_id: e.inference_id } } } }, null, 2)}\n\nPOST /articles/_doc?refresh=true\n${JSON.stringify({ body: "Passwords can be reset from the sign-in page." }, null, 2)}\n\nPOST /articles/_search\n${JSON.stringify({ query: { semantic: { field: "body", query: "forgot my password" } } }, null, 2)}`;

  return (
    <div className="space-y-4">
      <ErrorNote error={error} />
      <Panel title="Inference endpoints" aside={<span className="font-mono text-[12px] text-subtle">{endpoints?.length ?? "…"}</span>}>
        <div className="px-5 py-4">
          <p className="mb-3 max-w-2xl text-[13px] text-muted">
            The models <span className="font-mono">semantic_text</span> fields embed with, rerankers and completions. Calls go out
            from the cluster to the provider; keys stay sealed in the cluster.
          </p>
          {!endpoints ? (
            <p className="text-[13px] text-subtle">
              <span className="shimmer">Loading…</span>
            </p>
          ) : (
            <div className="overflow-x-auto rounded-lg border border-line">
              <table className="w-full text-[13px]">
                <thead className="text-left text-[11px] text-subtle">
                  <tr className="border-b border-line">
                    <th className="px-3 py-2 font-normal">Endpoint</th>
                    <th className="px-3 py-2 font-normal">Task</th>
                    <th className="px-3 py-2 font-normal">Service</th>
                    <th className="px-3 py-2 font-normal">Model</th>
                    <th className="px-3 py-2" />
                  </tr>
                </thead>
                <tbody>
                  {endpoints.map((e) => {
                    const builtIn = e.inference_id.startsWith(".");
                    const s = e.service_settings ?? {};
                    return (
                      <tr key={e.inference_id} className="border-b border-line last:border-0">
                        <td className="max-w-64 px-3 py-2">
                          <div className="truncate font-mono text-[12px]">{e.inference_id}</div>
                          {(builtIn || typeof s.url === "string") && (
                            <div className="truncate text-[11px] text-subtle">{builtIn ? "built in · needs ML nodes" : String(s.url)}</div>
                          )}
                        </td>
                        <td className="px-3 py-2 text-muted">{e.task_type}</td>
                        <td className="px-3 py-2 text-muted">{e.service}</td>
                        <td className="max-w-48 truncate px-3 py-2 font-mono text-[12px] text-muted">{String(s.model_id ?? "")}</td>
                        <td className="whitespace-nowrap px-3 py-2 text-right">
                          {deleting === e.inference_id ? (
                            <span className="inline-flex items-center gap-2">
                              <span className="text-[12px] text-muted">Fields using it stop embedding.</span>
                              <button disabled={busy !== null} onClick={() => remove(e)} className={`${btn} hover:border-danger/50 hover:text-danger`}>
                                {busy === "delete" ? <span className="shimmer">Deleting…</span> : "Delete"}
                              </button>
                              <button onClick={() => setDeleting(null)} className={btn}>
                                Cancel
                              </button>
                            </span>
                          ) : (
                            <span className="inline-flex gap-2">
                              {!builtIn && e.task_type !== "chat_completion" && (
                                <button onClick={() => (setTesting(e), setAnswer(null))} className={btn}>
                                  <Play className="size-3.5" /> Test
                                </button>
                              )}
                              {!builtIn && (e.task_type === "text_embedding" || e.task_type === "sparse_embedding") && (
                                <button onClick={() => onOpenConsole(quickstart(e))} className={btn} title="A semantic_text index on this endpoint, in the console">
                                  <Sparkles className="size-3.5" /> Use
                                </button>
                              )}
                              {!builtIn && (
                                <button onClick={() => setDeleting(e.inference_id)} className={btn} aria-label={`Delete ${e.inference_id}`}>
                                  <Trash2 className="size-3.5" />
                                </button>
                              )}
                            </span>
                          )}
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          )}
          {testing && (
            <div className="rise mt-4 space-y-3 rounded-lg border border-line bg-bg p-4">
              <p className="text-[13px]">
                Test <span className="font-mono">{testing.inference_id}</span>
              </p>
              <textarea className={`${input} block min-h-14 w-full`} value={sample} onChange={(e) => setSample(e.target.value)} />
              {testing.task_type === "rerank" && (
                <Field label="Documents to rank, one per line">
                  <textarea className={`${input} block min-h-16 w-full`} value={docs} onChange={(e) => setDocs(e.target.value)} />
                </Field>
              )}
              <div className="flex flex-wrap items-center gap-2">
                <button className={primary} disabled={busy !== null || !sample.trim()} onClick={() => test(testing)}>
                  {busy === "test" ? <span className="shimmer">Calling the model…</span> : "Run"}
                </button>
                <button className={btn} onClick={() => setTesting(null)}>
                  Close
                </button>
              </div>
              {answer && <p className="rise break-words font-mono text-[12px] text-muted">{answer}</p>}
            </div>
          )}
        </div>
      </Panel>

      <Panel title="New endpoint">
        <form
          className="space-y-4 px-5 py-4"
          onSubmit={(e) => {
            e.preventDefault();
            create();
          }}
        >
          <div className="flex flex-wrap gap-3">
            <Segmented value={service} options={(Object.keys(SERVICES) as Service[]).map((s) => ({ label: SERVICES[s].label, value: s }))} onChange={pickService} />
            <Segmented value={task} options={Object.keys(SERVICES[service].tasks).map((t) => ({ label: t.replace("_", " "), value: t }))} onChange={pickTask} />
          </div>
          <div className="grid gap-3 sm:grid-cols-2">
            <Field label="Endpoint id" hint="lowercase letters, digits, . _ -">
              <input className={`${input} w-full font-mono text-[12px]`} value={id} onChange={(e) => setId(e.target.value.toLowerCase())} placeholder="my-embeddings" />
            </Field>
            <Field label="Model">
              <input className={`${input} w-full font-mono text-[12px]`} value={model} onChange={(e) => setModel(e.target.value)} />
            </Field>
            <Field label="API key" hint="sent once, sealed in the cluster, never shown again">
              <input
                className={`${input} w-full font-mono text-[12px]`}
                type="password"
                autoComplete="off"
                value={apiKey}
                onChange={(e) => setApiKey(e.target.value)}
              />
            </Field>
            {service === "openai" && (
              <Field label="URL (optional)" hint="any OpenAI-compatible endpoint: vLLM, Ollama, LiteLLM; public addresses only">
                <input className={`${input} w-full font-mono text-[12px]`} value={url} onChange={(e) => setUrl(e.target.value)} placeholder="https://api.openai.com/v1/embeddings" />
              </Field>
            )}
            {service === "openai" && task === "text_embedding" && (
              <Field label="Dimensions (optional)">
                <input className={`${input} w-full`} inputMode="numeric" value={dims} onChange={(e) => setDims(e.target.value.replace(/\D/g, ""))} />
              </Field>
            )}
            {service === "anthropic" && (
              <Field label="Max tokens">
                <input className={`${input} w-full`} inputMode="numeric" value={maxTokens} onChange={(e) => setMaxTokens(e.target.value.replace(/\D/g, ""))} />
              </Field>
            )}
          </div>
          <button type="submit" className={primary} disabled={busy !== null || !ID_RE.test(id) || !apiKey || !model.trim()}>
            {busy === "create" ? (
              <span className="shimmer">Checking the model…</span>
            ) : (
              <>
                <Plus className="size-3.5" /> Create endpoint
              </>
            )}
          </button>
        </form>
      </Panel>
    </div>
  );
}
