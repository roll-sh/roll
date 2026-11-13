import { Play } from "lucide-react";
import { useEffect, useState } from "react";
import { esJson, indexPath, searchApi, type SearchCluster } from "@/lib/search-api";
import { input, Panel, primary } from "../ui";

/** Elasticsearch's built-in analyzers, for `_analyze` without an index. */
const BUILT_IN = ["standard", "simple", "whitespace", "stop", "keyword", "english", "french", "german", "spanish", "cjk"];

interface Token {
  token: string;
  start_offset: number;
  end_offset: number;
  type: string;
  position: number;
}

/**
 * The `_analyze` playground: how an analyzer, or an index's field, turns text into the tokens
 * that are indexed and searched. Analyzing reads no documents; an index's own analyzers and
 * field mappings are listed from its mapping when one is picked.
 */
export function AnalyzeTab({ cluster }: { cluster: SearchCluster }) {
  const [indices, setIndices] = useState<string[] | null>(null);
  const [index, setIndex] = useState("");
  const [fields, setFields] = useState<string[]>([]);
  const [analyzers, setAnalyzers] = useState<string[]>([]);
  // `a:<analyzer>` or `f:<field>`.
  const [using, setUsing] = useState("a:standard");
  const [text, setText] = useState("The quick brown foxes jumped over the lazy dogs");
  const [tokens, setTokens] = useState<Token[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // Index names, once (listing wakes the cluster: only when this tab opens).
  useEffect(() => {
    searchApi.indices(cluster.id).then(
      (rows) => setIndices(rows.map((r) => r.index).filter((n) => !n.startsWith("."))),
      () => setIndices([]),
    );
  }, [cluster.id]);

  // The picked index's text fields and custom analyzers.
  useEffect(() => {
    setFields([]);
    setAnalyzers([]);
    if (!index) return;
    let gone = false;
    esJson<Record<string, { mappings?: unknown; settings?: { index?: { analysis?: { analyzer?: Record<string, unknown> } } } }>>(
      cluster.id,
      "GET",
      `${indexPath(index)}?filter_path=*.mappings,*.settings.index.analysis`,
    ).then(
      (v) => {
        if (gone) return;
        const entry = Object.values(v)[0];
        setFields(textFields(entry?.mappings));
        setAnalyzers(Object.keys(entry?.settings?.index?.analysis?.analyzer ?? {}).sort());
      },
      () => {},
    );
    return () => {
      gone = true;
    };
  }, [cluster.id, index]);

  const run = async () => {
    setBusy(true);
    setError(null);
    try {
      const body = using.startsWith("f:") ? { field: using.slice(2), text } : { analyzer: using.slice(2), text };
      const path = index ? `${indexPath(index)}/_analyze` : "/_analyze";
      const v = await esJson<{ tokens: Token[] }>(cluster.id, "POST", path, JSON.stringify(body));
      setTokens(v.tokens ?? []);
    } catch (e) {
      setTokens(null);
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Panel title="Analyze">
      <div className="space-y-4 px-5 py-4">
        <p className="max-w-2xl text-[13px] text-muted">
          How an analyzer breaks text into the terms that are indexed and matched. Pick an index to try its own analyzers and
          the analyzer of one of its fields.
        </p>
        <div className="flex flex-wrap items-end gap-3">
          <label className="grid gap-1 text-[12px] text-subtle">
            Index
            <select className={input} value={index} onChange={(e) => (setIndex(e.target.value), setUsing("a:standard"))}>
              <option value="">none (built-in analyzers)</option>
              {(indices ?? []).map((n) => (
                <option key={n} value={n}>
                  {n}
                </option>
              ))}
            </select>
          </label>
          <label className="grid gap-1 text-[12px] text-subtle">
            Analyzer
            <select className={input} value={using} onChange={(e) => setUsing(e.target.value)}>
              <optgroup label="Built in">
                {BUILT_IN.map((a) => (
                  <option key={a} value={`a:${a}`}>
                    {a}
                  </option>
                ))}
              </optgroup>
              {analyzers.length > 0 && (
                <optgroup label={`${index}'s analyzers`}>
                  {analyzers.map((a) => (
                    <option key={a} value={`a:${a}`}>
                      {a}
                    </option>
                  ))}
                </optgroup>
              )}
              {fields.length > 0 && (
                <optgroup label={`${index}'s fields`}>
                  {fields.map((f) => (
                    <option key={f} value={`f:${f}`}>
                      field {f}
                    </option>
                  ))}
                </optgroup>
              )}
            </select>
          </label>
        </div>
        <textarea className={`${input} block min-h-20 w-full font-mono`} value={text} onChange={(e) => setText(e.target.value)} />
        <button className={primary} onClick={run} disabled={busy || !text}>
          <Play className="size-3.5" /> Analyze
        </button>
        {error && <p className="text-[13px] text-danger">{error}</p>}
        {tokens && (
          <div className="overflow-x-auto">
            {tokens.length === 0 ? (
              <p className="text-[13px] text-muted">No tokens: the analyzer drops all of this text.</p>
            ) : (
              <table className="w-full text-left text-[13px]">
                <thead className="text-[12px] text-subtle">
                  <tr>
                    <th className="py-1.5 pr-4 font-normal">position</th>
                    <th className="py-1.5 pr-4 font-normal">token</th>
                    <th className="py-1.5 pr-4 font-normal">offsets</th>
                    <th className="py-1.5 font-normal">type</th>
                  </tr>
                </thead>
                <tbody className="font-mono">
                  {tokens.map((t, i) => (
                    <tr key={i} className="border-t border-line">
                      <td className="py-1.5 pr-4 text-subtle">{t.position}</td>
                      <td className="py-1.5 pr-4">{t.token}</td>
                      <td className="py-1.5 pr-4 text-subtle">
                        {t.start_offset}–{t.end_offset}
                      </td>
                      <td className="py-1.5 text-subtle">{t.type}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </div>
        )}
      </div>
    </Panel>
  );
}

/** Paths of the `text` and `match_only_text` fields in a mapping (sub-fields included). */
export function textFields(mappings: unknown): string[] {
  const out: string[] = [];
  const walk = (props: unknown, prefix: string) => {
    if (!props || typeof props !== "object") return;
    for (const [name, def] of Object.entries(props as Record<string, Record<string, unknown>>)) {
      const path = prefix + name;
      if (def.type === "text" || def.type === "match_only_text") out.push(path);
      walk(def.properties, `${path}.`);
      walk(def.fields, `${path}.`);
    }
  };
  walk((mappings as { properties?: unknown } | undefined)?.properties, "");
  return out.sort();
}
