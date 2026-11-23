import { ArrowUp, Check, Code2, Square } from "lucide-react";
import { useEffect, useRef, useState, type ReactNode } from "react";
import { esJson, esStream, type Method, type SearchCluster } from "@/lib/search-api";
import {
  answerParts,
  buildPrompt,
  chatCode,
  chatEvents,
  chatRequest,
  citations,
  MAX_SOURCES,
  newBoundary,
  sourcesOf,
  sseReader,
  type ChatHit,
  type Message,
  type Source,
  type SourceFields,
  type Turn,
} from "@/lib/search-chat";
import { buildRequest, LANGUAGES, needsEmbedding, type Config, type Language } from "@/lib/search-playground";
import { useMarkState } from "../mark-state";
import { btn, input, Panel, primary, useCopy } from "../ui";

/**
 * The Playground's Chat tab (docs/search/rag.md, phase 1): each question retrieves with the
 * tab's retrieval config, shows its sources at once, then streams an answer from the picked
 * `chat_completion` endpoint, cited as `[n]`. The conversation lives in this page only.
 * Answers are untrusted text: rendered as text nodes, links only from the sources' own fields.
 * The thinking pattern is `components/ask.tsx`'s: the mark thinks, steps shimmer, the answer
 * streams with a caret, the mark plays success.
 */

type Phase = "searching" | "writing" | "done" | "error" | "stopped";

interface ChatTurn {
  question: string;
  phase: Phase;
  sources: Source[];
  omitted: number;
  answer: string;
  error: string | null;
  finish: string | null;
  usage: { prompt: number; completion: number } | null;
  /** The messages sent, for "view code". */
  prompt: Message[] | null;
}

/** How many hits a question retrieves before the budget picks the sources. */
const RETRIEVE = 8;

export function Chat({
  cluster,
  index,
  cfg,
  fields,
  settings,
  endpoints,
  embed,
  onOpenInference,
}: {
  cluster: SearchCluster;
  index: string;
  cfg: Config;
  fields: SourceFields;
  /** The retrieval settings (the Playground's mode settings for `cfg`). */
  settings: ReactNode;
  /** The cluster's `chat_completion` endpoints. */
  endpoints: string[];
  embed: (id: string, q: string) => Promise<number[]>;
  onOpenInference: () => void;
}) {
  const { setState, play } = useMarkState();
  const [model, setModel] = useState(endpoints[0] ?? "");
  const [instructions, setInstructions] = useState("");
  const [question, setQuestion] = useState("");
  const [turns, setTurns] = useState<ChatTurn[]>([]);
  const [code, setCode] = useState<Language | null>(null);
  const abort = useRef<AbortController | null>(null);

  useEffect(() => {
    if (!endpoints.includes(model)) setModel(endpoints[0] ?? "");
  }, [endpoints, model]);
  useEffect(() => () => abort.current?.abort(), []);

  const busy = turns.some((t) => t.phase === "searching" || t.phase === "writing");
  const patch = (i: number, p: Partial<ChatTurn> | ((t: ChatTurn) => Partial<ChatTurn>)) =>
    setTurns((ts) => ts.map((t, j) => (j === i ? { ...t, ...(typeof p === "function" ? p(t) : p) } : t)));

  const ask = async () => {
    const q = question.trim();
    if (!q || !model || busy) return;
    const history: Turn[] = turns.filter((t) => t.phase === "done" && t.answer).map((t) => ({ question: t.question, answer: t.answer }));
    const i = turns.length;
    const ctl = new AbortController();
    abort.current = ctl;
    setQuestion("");
    setTurns((ts) => [...ts, { question: q, phase: "searching", sources: [], omitted: 0, answer: "", error: null, finish: null, usage: null, prompt: null }]);
    setState("thinking");
    try {
      const id = needsEmbedding(cfg);
      const vectors = id ? { [id]: await embed(id, q) } : {};
      const built = buildRequest(cfg, index, { query: q, size: RETRIEVE, vectors });
      if (!built.ok) throw new Error(built.reason);
      const r = await esJson<{ hits?: { hits?: ChatHit[] } }>(cluster.id, built.request.method as Method, built.request.path, built.request.body);
      const { sources, omitted } = sourcesOf(r.hits?.hits ?? [], fields);
      // The sources show before generation starts.
      patch(i, { sources, omitted: omitted.length, phase: "writing" });
      if (!sources.length) {
        patch(i, { phase: "done", finish: "no_sources" });
        play([["success", 1200]]);
        return;
      }
      const prompt = buildPrompt({ instructions, history, sources, question: q, boundary: newBoundary() });
      patch(i, { prompt });
      const read = sseReader();
      const take = (events: ReturnType<typeof read>) => {
        for (const e of events) {
          for (const c of chatEvents(e)) {
            if (c.kind === "delta") patch(i, (t) => ({ answer: t.answer + c.text }));
            else if (c.kind === "finish") patch(i, { finish: c.reason });
            else if (c.kind === "usage") patch(i, { usage: { prompt: c.prompt, completion: c.completion } });
            else if (c.kind === "error") throw new Error(c.reason);
          }
        }
      };
      await esStream(cluster.id, chatRequest(model, prompt), (text) => take(read(text)), ctl.signal);
      take(read("", true));
      patch(i, { phase: "done" });
      play([["success", 1600]]);
    } catch (e) {
      const stopped = ctl.signal.aborted;
      // Whatever failed, the stream (and the cluster's upstream call) closes.
      ctl.abort();
      if (stopped) {
        patch(i, { phase: "stopped" });
        setState("idle");
        return;
      }
      patch(i, { phase: "error", error: e instanceof Error ? e.message : String(e) });
      play([["error", 1200]]);
    } finally {
      if (abort.current === ctl) abort.current = null;
    }
  };

  const last = turns[turns.length - 1];
  const searchFor = buildRequest(cfg, index, { query: last?.question ?? question, size: RETRIEVE, vectors: {} });

  return (
    <div className="grid gap-4 lg:grid-cols-[minmax(0,20rem)_minmax(0,1fr)]">
      <Panel title="Setup">
        <div className="space-y-4 px-5 py-4">
          <label className="block space-y-1.5">
            <span className="block text-[11px] text-subtle">Model</span>
            {endpoints.length ? (
              <select value={model} onChange={(e) => setModel(e.target.value)} className={`${input} w-full font-mono text-[12px]`} aria-label="Model">
                {endpoints.map((e) => (
                  <option key={e} value={e}>
                    {e}
                  </option>
                ))}
              </select>
            ) : (
              <span className="block text-[12px] leading-relaxed text-muted">
                This cluster has no <span className="font-mono">chat_completion</span> endpoint. Add one in Inference: pick the provider and the task type{" "}
                <span className="font-mono">chat_completion</span>, and take its key from a roll secret, so the key is never typed or shown.{" "}
                <button type="button" onClick={onOpenInference} className="text-accent hover:underline">
                  Open Inference
                </button>
              </span>
            )}
          </label>
          <div className="space-y-1.5">
            <p className="text-[11px] text-subtle">Retrieval</p>
            {settings}
          </div>
          <label className="block space-y-1.5">
            <span className="block text-[11px] text-subtle">Instructions</span>
            <textarea
              value={instructions}
              onChange={(e) => setInstructions(e.target.value)}
              rows={3}
              placeholder="Optional: tone, audience, length"
              className={`${input} w-full resize-y text-[12px]`}
              aria-label="Instructions"
            />
            <span className="block text-[11px] leading-relaxed text-subtle">
              Sent after roll&apos;s own frame, which has the model answer only from the sources and treat them as quoted data.
            </span>
          </label>
        </div>
      </Panel>

      <Panel
        title="Chat"
        aside={
          <span className="flex items-center gap-2 text-[11px] text-subtle">
            <Code2 className="hidden size-3.5 sm:inline" strokeWidth={1.5} />
            <span className="hidden sm:inline">View code</span>
            {LANGUAGES.map((lang) => (
              <button
                key={lang}
                type="button"
                disabled={!searchFor.ok || !model}
                aria-pressed={code === lang}
                onClick={() => setCode((c) => (c === lang ? null : lang))}
                className={`font-mono transition hover:text-fg disabled:opacity-40 ${code === lang ? "text-fg" : ""}`}
              >
                {lang === "javascript" ? "js" : lang}
              </button>
            ))}
            {turns.length > 0 && !busy && (
              <button type="button" onClick={() => setTurns([])} className="ml-1 transition hover:text-fg">
                Clear
              </button>
            )}
          </span>
        }
      >
        {code && searchFor.ok && model && (
          <CodeView
            lang={code}
            text={chatCode(
              code,
              {
                search: searchFor.request,
                question: last?.question ?? question,
                inferenceId: model,
                instructions,
                fields,
                embed: needsEmbedding(cfg),
                prompt: last?.prompt ?? undefined,
              },
              { url: cluster.url, flavor: cluster.flavor },
            )}
          />
        )}
        {turns.length === 0 ? (
          <p className="px-5 py-6 text-[13px] text-subtle">Ask a question about {index}. Its hits become the sources the answer cites.</p>
        ) : (
          <ol>
            {turns.map((t, i) => (
              <TurnView key={i} turn={t} id={`chat-${i}`} />
            ))}
          </ol>
        )}
        <form
          className="flex items-center gap-2 border-t border-line px-5 py-3"
          onSubmit={(e) => {
            e.preventDefault();
            ask();
          }}
        >
          <input
            value={question}
            onChange={(e) => setQuestion(e.target.value)}
            placeholder={model ? "Ask about the documents" : "Add a chat_completion endpoint first"}
            aria-label="Question"
            disabled={!model}
            className={`${input} min-w-0 flex-1`}
          />
          {busy ? (
            <button type="button" onClick={() => abort.current?.abort()} className={btn} aria-label="Stop">
              <Square className="size-3.5" strokeWidth={1.5} /> Stop
            </button>
          ) : (
            <button type="submit" className={primary} disabled={!question.trim() || !model} aria-label="Ask">
              <ArrowUp className="size-3.5" strokeWidth={2} /> Ask
            </button>
          )}
        </form>
      </Panel>
    </div>
  );
}

function CodeView({ lang, text }: { lang: Language; text: string }) {
  const { copied, copy } = useCopy();
  return (
    <div className="rise border-b border-line">
      <div className="flex items-center justify-between px-5 pt-3 text-[11px] text-subtle">
        <span>The composed calls ({lang}): search, then the chat completion. They run on Elasticsearch 9.5.5 too.</span>
        <button type="button" onClick={() => copy(lang, text)} className="font-mono transition hover:text-fg">
          {copied === lang ? "copied" : "copy"}
        </button>
      </div>
      <pre className="max-h-80 overflow-auto px-5 py-3 font-mono text-[11px] leading-relaxed text-muted" aria-label={`${lang} code`}>
        {text}
      </pre>
    </div>
  );
}

const STEPS: { phase: Phase; label: string }[] = [
  { phase: "searching", label: "Searching…" },
  { phase: "writing", label: "Writing…" },
];

function TurnView({ turn, id }: { turn: ChatTurn; id: string }) {
  const at = turn.phase === "searching" ? 0 : turn.phase === "writing" ? 1 : 2;
  const cited = turn.phase === "done" ? citations(turn.answer, turn.sources.length) : [];
  const focus = (n: number) => {
    const el = document.getElementById(`${id}-src-${n}`);
    el?.scrollIntoView({ block: "nearest", behavior: "smooth" });
    el?.focus({ preventScroll: true });
  };
  return (
    <li className="space-y-3 border-b border-line px-5 py-4 last:border-0">
      <p className="text-[13px] text-subtle">{turn.question}</p>
      {(turn.phase === "searching" || turn.phase === "writing") && !turn.answer && (
        <ol className="space-y-1.5">
          {STEPS.map((s, i) =>
            i > at ? null : (
              <li key={s.phase} className="rise flex items-center gap-2 text-[13px]">
                {i < at ? <Check className="size-3.5 text-ok" strokeWidth={3} /> : <span className="size-1.5 rounded-full bg-accent" />}
                <span className={i < at ? "text-muted" : "shimmer"}>{s.label}</span>
              </li>
            ),
          )}
        </ol>
      )}
      {turn.sources.length > 0 && (
        <ol className="rise grid gap-2 sm:grid-cols-2" aria-label="Sources">
          {turn.sources.map((s) => (
            <SourceCard key={s.n} source={s} id={`${id}-src-${s.n}`} cited={cited.includes(s.n)} />
          ))}
        </ol>
      )}
      {turn.omitted > 0 && (
        <p className="text-[11px] text-subtle">
          {turn.omitted} more {turn.omitted === 1 ? "hit wasn't" : "hits weren't"} sent: past {MAX_SOURCES} sources or the context budget.
        </p>
      )}
      {turn.answer && (
        <p className={`whitespace-pre-wrap text-[14px] leading-relaxed ${turn.phase === "writing" ? "caret" : ""}`} data-testid="answer">
          {answerParts(turn.answer, turn.sources.length).map((p, i) =>
            p.kind === "text" ? (
              <span key={i}>{p.text}</span>
            ) : (
              <button
                key={i}
                type="button"
                onClick={() => focus(p.n)}
                className="mx-px align-baseline font-mono text-[11px] text-accent hover:underline"
                aria-label={`Source ${p.n}`}
              >
                [{p.n}]
              </button>
            ),
          )}
        </p>
      )}
      {turn.phase === "done" && turn.finish === "no_sources" && (
        <p className="text-[13px] text-muted">No hits for this question, so the model wasn&apos;t asked. Try other words, or another retrieval mode.</p>
      )}
      {turn.phase === "error" && <p className="rise break-words text-[13px] text-danger">{turn.error}</p>}
      {turn.phase === "stopped" && <p className="text-[12px] text-subtle">Stopped.</p>}
      {turn.phase === "done" && turn.answer && (
        <p className="font-mono text-[11px] text-subtle">
          {cited.length ? `cites ${cited.map((n) => `[${n}]`).join(" ")}` : "cites no source"}
          {turn.finish && turn.finish !== "stop" && ` · ${turn.finish}`}
          {turn.usage && ` · ${turn.usage.prompt} + ${turn.usage.completion} tokens`}
        </p>
      )}
    </li>
  );
}

function SourceCard({ source, id, cited }: { source: Source; id: string; cited: boolean }) {
  return (
    <li id={id} tabIndex={-1} className={`rounded-lg border px-3 py-2 outline-none focus:border-line-2 ${cited ? "border-line-2" : "border-line"}`}>
      <div className="flex items-baseline gap-2">
        <span className="font-mono text-[11px] text-subtle">[{source.n}]</span>
        <p className="min-w-0 flex-1 truncate text-[12px]">
          {source.url ? (
            <a href={source.url} target="_blank" rel="noopener noreferrer nofollow" className="hover:underline">
              {source.title ?? source.id}
            </a>
          ) : (
            (source.title ?? <span className="font-mono">{source.id}</span>)
          )}
        </p>
      </div>
      <p className="truncate font-mono text-[10px] text-subtle">
        {source.index} · {source.id}
      </p>
      {source.passages[0] && <p className="mt-1 line-clamp-3 text-[11px] leading-relaxed text-muted">{source.passages[0]}</p>}
    </li>
  );
}
