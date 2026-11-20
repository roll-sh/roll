import { KeyRound, Play, Plus, RefreshCw, Sparkles, Trash2 } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { esJson, searchApi, type InferenceBinding, type InferenceSecrets, type SearchCluster } from "@/lib/search-api";
import { secretsApi, type Environment, type Secret } from "@/lib/secrets-api";
import { Field, Segmented } from "../redis/shared";
import { btn, ErrorNote, input, Panel, primary } from "../ui";

/** An inference endpoint as the cluster lists it (`GET _inference/_all`; never its API key). */
interface Endpoint {
  inference_id: string;
  task_type: string;
  service: string;
  service_settings?: Record<string, unknown>;
}

/** A setting the form asks for: a service setting, or a task setting when `task` is set. */
export interface FieldSpec {
  key: string;
  label: string;
  /** Sent once, sealed in the cluster, never shown again. */
  secret?: boolean;
  multiline?: boolean;
  optional?: boolean;
  numeric?: boolean;
  task?: boolean;
  hint?: string;
  placeholder?: string;
  /** The default value. */
  value?: string;
  /** Only for these task types. */
  tasks?: string[];
}

export interface ServiceSpec {
  label: string;
  /** Task types offered, with the model each suggests. */
  tasks: Record<string, string>;
  /** The setting that names the model (none: the deployment or URL decides). */
  model?: "model_id" | "model";
  fields: FieldSpec[];
}

const apiKey: FieldSpec = { key: "api_key", label: "API key", secret: true, hint: "sent once, sealed in the cluster, never shown again" };

/** What each service offers here (`GET _inference/_services` has the full schemas). */
export const SERVICES: Record<string, ServiceSpec> = {
  openai: {
    label: "OpenAI",
    tasks: { text_embedding: "text-embedding-3-small", completion: "gpt-4o-mini" },
    model: "model_id",
    fields: [
      apiKey,
      { key: "url", label: "URL", optional: true, hint: "OpenAI-compatible: vLLM, Ollama, LiteLLM (public)", placeholder: "https://api.openai.com/v1/embeddings" },
      { key: "dimensions", label: "Dimensions", optional: true, numeric: true, tasks: ["text_embedding"] },
    ],
  },
  azureopenai: {
    label: "Azure OpenAI",
    tasks: { text_embedding: "", completion: "" },
    fields: [
      { key: "resource_name", label: "Resource name", placeholder: "my-resource" },
      { key: "deployment_id", label: "Deployment" },
      { key: "api_version", label: "API version", value: "2024-10-21" },
      { ...apiKey, optional: true, hint: "an API key or an Entra ID token" },
      { key: "entra_id", label: "Entra ID token", secret: true, optional: true },
    ],
  },
  amazonbedrock: {
    label: "Amazon Bedrock",
    tasks: { text_embedding: "amazon.titan-embed-text-v2:0", completion: "anthropic.claude-3-haiku-20240307-v1:0" },
    model: "model",
    fields: [
      { key: "provider", label: "Provider", hint: "amazontitan, cohere, anthropic, ai21labs, meta, mistral", value: "amazontitan" },
      { key: "region", label: "Region", value: "us-east-1" },
      { key: "access_key", label: "Access key", secret: true },
      { key: "secret_key", label: "Secret key", secret: true, hint: "sent once, sealed in the cluster, never shown again" },
    ],
  },
  googlevertexai: {
    label: "Vertex AI",
    tasks: { text_embedding: "text-embedding-005", rerank: "", completion: "gemini-2.5-flash" },
    model: "model_id",
    fields: [
      { key: "project_id", label: "Project" },
      { key: "location", label: "Location", optional: true, value: "us-central1", tasks: ["text_embedding", "completion"] },
      { key: "service_account_json", label: "Service account JSON", secret: true, multiline: true, hint: "sent once, sealed in the cluster, never shown again" },
    ],
  },
  googleaistudio: { label: "Google AI Studio", tasks: { text_embedding: "gemini-embedding-001", completion: "gemini-2.5-flash" }, model: "model_id", fields: [apiKey] },
  cohere: { label: "Cohere", tasks: { text_embedding: "embed-english-v3.0", rerank: "rerank-english-v3.0", completion: "command-r" }, model: "model_id", fields: [apiKey] },
  anthropic: {
    label: "Anthropic",
    tasks: { completion: "claude-sonnet-5-5" },
    model: "model_id",
    fields: [apiKey, { key: "max_tokens", label: "Max tokens", numeric: true, task: true, value: "1024" }],
  },
  mistral: { label: "Mistral", tasks: { text_embedding: "mistral-embed", completion: "mistral-small-latest" }, model: "model", fields: [apiKey] },
  voyageai: { label: "Voyage AI", tasks: { text_embedding: "voyage-3.5", rerank: "rerank-2.5" }, model: "model_id", fields: [apiKey] },
  jinaai: { label: "Jina AI", tasks: { text_embedding: "jina-embeddings-v3", rerank: "jina-reranker-v2-base-multilingual" }, model: "model_id", fields: [apiKey] },
  deepseek: {
    label: "DeepSeek",
    tasks: { completion: "deepseek-chat" },
    model: "model_id",
    fields: [apiKey, { key: "url", label: "URL", optional: true, placeholder: "https://api.deepseek.com/chat/completions" }],
  },
  hugging_face: {
    label: "Hugging Face",
    tasks: { text_embedding: "", sparse_embedding: "", rerank: "", completion: "" },
    fields: [{ key: "url", label: "Endpoint URL", hint: "a public Inference Endpoint" }, apiKey],
  },
};

/** The body of `PUT _inference/{task}/{id}` for a service, task and the form's values. */
export function endpointBody(service: string, task: string, model: string, values: Record<string, string>): Record<string, unknown> {
  const spec = SERVICES[service];
  const settings: Record<string, unknown> = {};
  const taskSettings: Record<string, unknown> = {};
  if (spec.model && model.trim()) settings[spec.model] = model.trim();
  for (const f of spec.fields) {
    if (f.tasks && !f.tasks.includes(task)) continue;
    const v = (values[f.key] ?? f.value ?? "").trim();
    if (!v) continue;
    (f.task ? taskSettings : settings)[f.key] = f.numeric ? Number(v) : v;
  }
  const body: Record<string, unknown> = { service, service_settings: settings };
  if (Object.keys(taskSettings).length) body.task_settings = taskSettings;
  return body;
}

/** Short labels for the task picker. */
const TASK_LABELS: Record<string, string> = { text_embedding: "embeddings", sparse_embedding: "sparse", completion: "completion", rerank: "rerank" };

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

/** Where a new endpoint's key comes from: typed here, or a roll secret of the cluster's project. */
type KeySource = "key" | "secret";

/** A key that can key an endpoint: shared, not sensitive (a key no person reads), not short-lived. */
const usable = (k: Secret) => !k.tags.includes("sensitive") && k.link?.mode !== "dynamic";

/** The settings a roll secret stands in for: the API key, and Azure's alternative to it. */
const KEYED = ["api_key", "entra_id"];

/**
 * A cluster's inference endpoints (`_inference`): the models `semantic_text` fields embed with,
 * rerankers and completions. A new endpoint's credentials go to the cluster once, where they're
 * sealed; they're never listed or shown again. Or, for a service keyed by an API key, the key is
 * a roll secret of the project the cluster is on (picked by name, never shown): roll secrets
 * pushes it again whenever it changes. Listing wakes the cluster (only when this tab opens).
 */
export function InferenceTab({ cluster, onOpenConsole }: { cluster: SearchCluster; onOpenConsole: (text: string) => void }) {
  const [endpoints, setEndpoints] = useState<Endpoint[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [service, setService] = useState("openai");
  const [task, setTask] = useState("text_embedding");
  const [id, setId] = useState("");
  const [model, setModel] = useState(SERVICES.openai.tasks.text_embedding);
  const [values, setValues] = useState<Record<string, string>>({});
  const [testing, setTesting] = useState<Endpoint | null>(null);
  const [sample, setSample] = useState("How do I reset my password?");
  const [docs, setDocs] = useState("Reset it from the sign-in page\nPasswords expire every 90 days");
  const [answer, setAnswer] = useState<string | null>(null);
  const [deleting, setDeleting] = useState<string | null>(null);
  const [source, setSource] = useState<KeySource>("key");
  const [linked, setLinked] = useState<InferenceSecrets | null>(null);
  const [project, setProject] = useState("");
  const [envs, setEnvs] = useState<Environment[] | null>(null);
  const [env, setEnv] = useState("production");
  const [keys, setKeys] = useState<Secret[] | null>(null);
  const [secretKey, setSecretKey] = useState("");

  const load = useCallback(() => {
    esJson<{ endpoints: Endpoint[] }>(cluster.id, "GET", "/_inference/_all").then(
      (r) => setEndpoints(r.endpoints.sort((a, b) => a.inference_id.localeCompare(b.inference_id))),
      (e) => setError(message(e)),
    );
    // Which endpoints follow a secret; nothing when roll secrets can't say (no project, no access).
    searchApi.inferenceSecrets(cluster.id).then(
      (r) => {
        setLinked(r);
        setProject((p) => (p && r.projects.some((x) => x.id === p) ? p : (r.projects[0]?.id ?? "")));
      },
      () => setLinked(null),
    );
  }, [cluster.id]);
  const bindingOf = (id: string): InferenceBinding | undefined => linked?.bindings.find((b) => b.endpoint === id);

  // The project's environments, then the environment's keys (names only).
  useEffect(() => {
    setEnvs(null);
    if (source !== "secret" || !project) return;
    secretsApi(project)
      .environments()
      .then(
        (l) => {
          const standard = l.filter((e) => e.kind === "standard");
          setEnvs(standard);
          setEnv((cur) => (standard.some((e) => e.slug === cur) ? cur : (standard[0]?.slug ?? "")));
        },
        (e) => setError(message(e)),
      );
  }, [source, project]);
  useEffect(() => {
    setKeys(null);
    setSecretKey("");
    if (source !== "secret" || !project || !env) return;
    secretsApi(project)
      .secrets(env)
      .then(
        (l) => setKeys(l.filter((k) => k.type === "shared").sort((a, b) => a.key.localeCompare(b.key))),
        (e) => setError(message(e)),
      );
  }, [source, project, env]);
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

  const pickService = (s: string) => {
    setService(s);
    setValues({});
    const tasks = SERVICES[s].tasks;
    const t = task in tasks ? task : Object.keys(tasks)[0];
    setTask(t);
    setModel(tasks[t]);
  };
  const pickTask = (t: string) => {
    setTask(t);
    setModel(SERVICES[service].tasks[t]);
  };

  const spec = SERVICES[service];
  // A roll secret keys only services that take an API key (`search-inference` pushes `api_key`).
  const keyed = spec.fields.some((f) => f.key === "api_key");
  const fromSecret = source === "secret" && keyed;
  const fields = spec.fields.filter((f) => (!f.tasks || f.tasks.includes(task)) && !(fromSecret && KEYED.includes(f.key)));
  // Azure's two credentials are alternatives: one of them must be set (or the secret is it).
  const missing =
    fields.some((f) => !f.optional && !(values[f.key] ?? f.value ?? "").trim()) ||
    (fromSecret ? !secretKey : service === "azureopenai" && !values.api_key?.trim() && !values.entra_id?.trim());
  // Vertex AI's rerank picks Google's default ranker without a model.
  const optionalModel = service === "googlevertexai" && task === "rerank";

  const create = () =>
    act("create", async () => {
      if (fromSecret) {
        // The secret is the key: whatever was typed for one before switching stays here.
        const rest = Object.fromEntries(Object.entries(values).filter(([k]) => !KEYED.includes(k)));
        const config = endpointBody(service, task, model, rest);
        await searchApi.createInferenceFromSecret(cluster.id, { inference_id: id, task_type: task, config, secret: { project, environment: env, key: secretKey } });
      } else {
        await esJson(cluster.id, "PUT", `/_inference/${task}/${encodeURIComponent(id)}`, endpointBody(service, task, model, values));
      }
      setValues({});
      setId("");
      load();
    });

  /** Push the secret's current value to the endpoint now (its sync, run). */
  const refresh = (b: InferenceBinding) =>
    act(`refresh:${b.endpoint}`, async () => {
      await secretsApi(b.project).runSync(b.sync);
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
      // Nothing follows the secret for an endpoint that's gone.
      const b = bindingOf(e.inference_id);
      if (b) await secretsApi(b.project).deleteSync(b.sync);
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
                    const b = bindingOf(e.inference_id);
                    return (
                      <tr key={e.inference_id} className="border-b border-line last:border-0">
                        <td className="max-w-64 px-3 py-2">
                          <div className="truncate font-mono text-[12px]">{e.inference_id}</div>
                          {(builtIn || typeof s.url === "string") && (
                            <div className="truncate text-[11px] text-subtle">{builtIn ? "built in · needs ML nodes" : String(s.url)}</div>
                          )}
                          {b && (
                            <div className="flex items-center gap-1 truncate text-[11px] text-subtle" title="The key follows this roll secret: a new value is pushed to the endpoint">
                              <KeyRound className="size-3 shrink-0" strokeWidth={1.5} />
                              <span className="truncate font-mono">
                                {b.environment}
                                {b.path === "/" ? "" : b.path}/{b.key}
                              </span>
                            </div>
                          )}
                          {b?.status === "error" && b.error && <div className="truncate text-[11px] text-danger" title={b.error}>{b.error}</div>}
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
                              {b && (
                                <button
                                  disabled={busy !== null}
                                  onClick={() => refresh(b)}
                                  className={btn}
                                  title={`Push ${b.key}'s current value to the endpoint now`}
                                  aria-label={`Refresh ${e.inference_id} from ${b.key}`}
                                >
                                  <RefreshCw className={`size-3.5 ${busy === `refresh:${b.endpoint}` ? "animate-spin motion-reduce:animate-none" : ""}`} />
                                </button>
                              )}
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
            <select className={`${input} text-[13px]`} aria-label="Service" value={service} onChange={(e) => pickService(e.target.value)}>
              {Object.keys(SERVICES).map((s) => (
                <option key={s} value={s}>
                  {SERVICES[s].label}
                </option>
              ))}
            </select>
            <Segmented value={task} options={Object.keys(SERVICES[service].tasks).map((t) => ({ label: TASK_LABELS[t] ?? t, value: t }))} onChange={pickTask} />
            {keyed && (
              <Segmented
                value={source}
                options={[
                  { label: "A key", value: "key" },
                  { label: "A roll secret", value: "secret" },
                ]}
                onChange={setSource}
              />
            )}
          </div>
          <div className="grid gap-3 sm:grid-cols-2">
            <Field label="Endpoint id" hint="lowercase letters, digits, . _ -">
              <input className={`${input} w-full font-mono text-[12px]`} value={id} onChange={(e) => setId(e.target.value.toLowerCase())} placeholder="my-embeddings" />
            </Field>
            {spec.model && (
              <Field label="Model">
                <input className={`${input} w-full font-mono text-[12px]`} value={model} onChange={(e) => setModel(e.target.value)} />
              </Field>
            )}
            {fields.map((f) => (
              <Field key={f.key} label={f.optional ? `${f.label} (optional)` : f.label} hint={f.hint}>
                {f.multiline ? (
                  <textarea
                    className={`${input} h-20 w-full font-mono text-[12px]`}
                    autoComplete="off"
                    spellCheck={false}
                    value={values[f.key] ?? f.value ?? ""}
                    onChange={(e) => setValues((v) => ({ ...v, [f.key]: e.target.value }))}
                  />
                ) : (
                  <input
                    className={`${input} w-full font-mono text-[12px]`}
                    type={f.secret ? "password" : "text"}
                    autoComplete="off"
                    inputMode={f.numeric ? "numeric" : undefined}
                    placeholder={f.placeholder}
                    value={values[f.key] ?? f.value ?? ""}
                    onChange={(e) => setValues((v) => ({ ...v, [f.key]: f.numeric ? e.target.value.replace(/\D/g, "") : e.target.value }))}
                  />
                )}
              </Field>
            ))}
            {fromSecret && (
              <SecretPicker
                linked={linked}
                project={project}
                setProject={setProject}
                envs={envs}
                env={env}
                setEnv={setEnv}
                keys={keys}
                secretKey={secretKey}
                setSecretKey={setSecretKey}
              />
            )}
          </div>
          <button type="submit" className={primary} disabled={busy !== null || !ID_RE.test(id) || missing || (!!spec.model && !model.trim() && !optionalModel)}>
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

/**
 * The secret a new endpoint's key comes from: the cluster's project (when it's on more than one
 * canvas), an environment and a key, by name. You need reveal on it; the value never comes here.
 */
function SecretPicker(p: {
  linked: InferenceSecrets | null;
  project: string;
  setProject: (v: string) => void;
  envs: Environment[] | null;
  env: string;
  setEnv: (v: string) => void;
  keys: Secret[] | null;
  secretKey: string;
  setSecretKey: (v: string) => void;
}) {
  if (!p.linked || p.linked.projects.length === 0) {
    return (
      <Field label="Roll secret">
        <p className="py-1.5 text-[12px] text-muted">
          {p.linked ? "Put this cluster on a project's canvas first: that project's secrets can key it." : <span className="shimmer">Loading…</span>}
        </p>
      </Field>
    );
  }
  const choices = (p.keys ?? []).filter(usable);
  return (
    <Field label="Roll secret" hint="its value is pushed again whenever it changes">
      <div className="flex gap-2">
        {p.linked.projects.length > 1 && (
          <select className={`${input} min-w-0 flex-1`} value={p.project} onChange={(e) => p.setProject(e.target.value)} aria-label="Project">
            {p.linked.projects.map((x) => (
              <option key={x.id} value={x.id}>
                {x.name}
              </option>
            ))}
          </select>
        )}
        <select className={`${input} min-w-0 flex-1`} value={p.env} onChange={(e) => p.setEnv(e.target.value)} aria-label="Environment" disabled={!p.envs}>
          {(p.envs ?? []).map((e) => (
            <option key={e.slug} value={e.slug}>
              {e.name}
            </option>
          ))}
        </select>
        <select
          className={`${input} min-w-0 flex-[2] font-mono text-[12px]`}
          value={p.secretKey}
          onChange={(e) => p.setSecretKey(e.target.value)}
          aria-label="Key"
          disabled={!p.keys}
        >
          <option value="">{p.keys ? (choices.length ? "Pick a key" : "No keys here") : "…"}</option>
          {choices.map((k) => (
            <option key={k.id} value={k.key}>
              {k.key}
            </option>
          ))}
        </select>
      </div>
    </Field>
  );
}
