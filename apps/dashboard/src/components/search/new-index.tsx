import { Plus, SquareTerminal, Trash2, X } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { esJson, esRaw, indexPath, type SearchCluster } from "@/lib/search-api";
import { NEW_FIELD_TYPES, newIndexBody, newIndexProblem, type NewField, type NewFieldType } from "@/lib/search-docs";
import { indexNameProblem } from "@/lib/search-import";
import { Field, Segmented } from "../redis/shared";
import { btn, ErrorNote, input, Panel, primary } from "../ui";
import { JsonText } from "./json-view";

const ANALYZERS = ["standard", "english", "simple", "whitespace", "keyword", "french", "german", "spanish"];
const SIMILARITIES = ["cosine", "dot_product", "l2_norm"] as const;

/** The endpoints a `semantic_text` field can embed with (`GET _inference/_all`, loaded when one is
 * chosen): the cluster's own, not the preconfigured ones on Elasticsearch's own services (ELSER,
 * E5), which need machine learning nodes. */
function useEmbeddingEndpoints(cluster: string, wanted: boolean) {
  const [ids, setIds] = useState<string[] | null>(null);
  useEffect(() => {
    if (!wanted || ids !== null) return;
    let alive = true;
    esJson<{ endpoints: { inference_id: string; task_type: string; service: string }[] }>(cluster, "GET", "/_inference/_all").then(
      (r) =>
        alive &&
        setIds(r.endpoints.filter((e) => (e.task_type === "text_embedding" || e.task_type === "sparse_embedding") && e.service !== "elasticsearch" && e.service !== "elser").map((e) => e.inference_id)),
      () => alive && setIds([]),
    );
    return () => {
      alive = false;
    };
  }, [cluster, wanted, ids]);
  return ids;
}

/**
 * A new index with its fields mapped up front (Kibana's index management, without writing JSON):
 * a row per field, the options that matter for its type, `dynamic: strict` to refuse fields not
 * listed, and the request shown as it will be sent (or opened in the console to edit further).
 */
export function NewIndexPanel({ cluster, existing, onClose, onCreated, onOpenConsole }: { cluster: SearchCluster; existing: string[]; onClose: () => void; onCreated: (index: string) => void; onOpenConsole: (text: string) => void }) {
  const opensearch = cluster.flavor === "opensearch";
  const [name, setName] = useState("");
  const [fields, setFields] = useState<NewField[]>([
    { name: "title", type: "text", keyword: true },
    { name: "", type: "keyword" },
  ]);
  const [dynamic, setDynamic] = useState<"true" | "strict">("true");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const endpoints = useEmbeddingEndpoints(cluster.id, fields.some((f) => f.type === "semantic_text"));
  const types = NEW_FIELD_TYPES.filter((t) => !(opensearch && t === "semantic_text"));
  // semantic_text fields start on the cluster's first embeddings endpoint.
  useEffect(() => {
    const first = endpoints?.[0];
    if (first) setFields((fs) => (fs.some((f) => f.type === "semantic_text" && !f.inferenceId) ? fs.map((f) => (f.type === "semantic_text" && !f.inferenceId ? { ...f, inferenceId: first } : f)) : fs));
  }, [endpoints, fields]);

  const body = useMemo(() => newIndexBody(fields, dynamic, opensearch), [fields, dynamic, opensearch]);
  const request = `PUT ${indexPath(name.trim() || "my-index")}\n${JSON.stringify(body, null, 2)}`;
  const trimmed = name.trim();
  const problem = (trimmed ? indexNameProblem(trimmed) : null) ?? (existing.includes(trimmed) ? `${trimmed} exists already.` : null) ?? newIndexProblem(fields, opensearch);

  const set = (i: number, patch: Partial<NewField>) => setFields((fs) => fs.map((f, j) => (j === i ? { ...f, ...patch } : f)));
  const setType = (i: number, type: NewFieldType) =>
    set(i, type === "dense_vector" ? { type, dims: fields[i].dims ?? 384, similarity: fields[i].similarity ?? "cosine" } : type === "text" ? { type, keyword: fields[i].keyword ?? true } : { type });

  return (
    <Panel
      title="New index"
      className="rise"
      aside={
        <button onClick={onClose} className={btn} aria-label="Close new index">
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
            await esRaw(cluster.id, "PUT", indexPath(trimmed), body);
            onCreated(trimmed);
          } catch (err) {
            setError(err instanceof Error ? err.message : String(err));
          } finally {
            setBusy(false);
          }
        }}
      >
        <div className="grid items-end gap-3 sm:grid-cols-[minmax(0,1fr)_auto]">
          <Field label="Name">
            <input value={name} onChange={(e) => setName(e.target.value)} placeholder="products" className={`${input} w-full font-mono`} spellCheck={false} autoFocus />
          </Field>
          <Field label="Fields not listed">
            <Segmented
              value={dynamic}
              options={[
                { label: "mapped", value: "true" },
                { label: "refused", value: "strict" },
              ]}
              onChange={setDynamic}
            />
          </Field>
        </div>

        <div className="space-y-2">
          {fields.map((f, i) => (
            <div key={i} className="grid items-center gap-2 sm:grid-cols-[minmax(0,14rem)_9.5rem_minmax(0,1fr)_auto]">
              <input value={f.name} onChange={(e) => set(i, { name: e.target.value })} placeholder={i === 0 ? "title" : "field (user.name for an object)"} aria-label="Field name" className={`${input} font-mono`} spellCheck={false} />
              <select value={f.type} onChange={(e) => setType(i, e.target.value as NewFieldType)} aria-label="Field type" className={`${input} font-mono`}>
                {types.map((t) => (
                  <option key={t} value={t}>
                    {t === "dense_vector" && opensearch ? "knn_vector" : t}
                  </option>
                ))}
              </select>
              <div className="flex min-w-0 flex-wrap items-center gap-2 text-[12px] text-muted">
                {f.type === "text" && (
                  <>
                    <select value={f.analyzer ?? "standard"} onChange={(e) => set(i, { analyzer: e.target.value })} aria-label="Analyzer" className={input}>
                      {ANALYZERS.map((a) => (
                        <option key={a} value={a}>
                          {a}
                        </option>
                      ))}
                    </select>
                    <label className="flex items-center gap-1.5">
                      <input type="checkbox" checked={!!f.keyword} onChange={(e) => set(i, { keyword: e.target.checked })} className="accent-accent" />
                      .keyword for sorting
                    </label>
                  </>
                )}
                {f.type === "semantic_text" && (
                  <>
                    <select value={f.inferenceId ?? ""} onChange={(e) => set(i, { inferenceId: e.target.value || undefined })} aria-label="Inference endpoint" className={`${input} min-w-0 font-mono`}>
                      <option value="" disabled>
                        pick an endpoint
                      </option>
                      {(endpoints ?? []).map((id) => (
                        <option key={id} value={id}>
                          {id}
                        </option>
                      ))}
                    </select>
                    {endpoints?.length === 0 && <span className="text-subtle">Add an embeddings endpoint in the Inference tab.</span>}
                  </>
                )}
                {f.type === "dense_vector" && (
                  <>
                    <input type="number" min={1} value={f.dims ?? ""} onChange={(e) => set(i, { dims: e.target.value === "" ? undefined : Number(e.target.value) })} aria-label="Dimensions" className={`${input} w-24 font-mono`} />
                    <span>dims</span>
                    <select value={f.similarity ?? "cosine"} onChange={(e) => set(i, { similarity: e.target.value as NewField["similarity"] })} aria-label="Similarity" className={input}>
                      {SIMILARITIES.map((s) => (
                        <option key={s} value={s}>
                          {s}
                        </option>
                      ))}
                    </select>
                  </>
                )}
              </div>
              <button type="button" onClick={() => setFields((fs) => fs.filter((_, j) => j !== i))} className="justify-self-end rounded-md p-1.5 text-subtle transition hover:text-danger" aria-label={`Remove ${f.name || "field"}`}>
                <Trash2 className="size-3.5" />
              </button>
            </div>
          ))}
          <button type="button" onClick={() => setFields((fs) => [...fs, { name: "", type: "keyword" }])} className={btn}>
            <Plus className="size-3.5" /> Field
          </button>
        </div>

        <details className="rounded-lg border border-line">
          <summary className="cursor-pointer px-3 py-2 text-[12px] text-muted">Request</summary>
          <div className="relative border-t border-line">
            <button type="button" onClick={() => onOpenConsole(request)} className="absolute top-2 right-2 rounded-md p-1 text-subtle transition hover:text-fg" title="Open in console">
              <SquareTerminal className="size-3.5" />
            </button>
            <pre className="max-h-72 overflow-auto px-3 py-2 font-mono text-[12px] leading-relaxed">
              <span className="text-muted">PUT {indexPath(trimmed || "my-index")}</span>
              {"\n"}
              <JsonText text={JSON.stringify(body, null, 2)} />
            </pre>
          </div>
        </details>

        <div className="flex flex-wrap items-center gap-3">
          <button type="submit" disabled={busy || !trimmed || !!problem} className={primary}>
            {busy ? <span className="shimmer">Creating…</span> : "Create index"}
          </button>
          {problem && <span className="text-[12px] text-danger">{problem}</span>}
        </div>
        <ErrorNote error={error} />
      </form>
    </Panel>
  );
}
