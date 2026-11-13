import { useCallback, useEffect, useState } from "react";
import { accessApi, type AuditSink } from "@/lib/access-api";
import { btn, CopyButton, ErrorNote, input, Panel, primary, useCopy } from "../ui";
import { relative } from "./shell";

/**
 * The audit log streamed to a SIEM (docs/permissions.md, "Audit sinks"): the operator's sinks, each
 * with how far it got; a new one shows its signing secret once. Hidden when the API says no (a
 * token signed in, or streaming off without ROLL_SECRET_KEY).
 */
export function Sinks() {
  const [sinks, setSinks] = useState<AuditSink[] | null>(null);
  const [url, setUrl] = useState("");
  const [fromStart, setFromStart] = useState(false);
  const [made, setMade] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const { copied, copy } = useCopy();

  const load = useCallback(async () => {
    try {
      setSinks(await accessApi.sinks());
    } catch {
      setSinks(null);
    }
  }, []);
  useEffect(() => {
    void load();
  }, [load]);

  const act = async (f: () => Promise<unknown>) => {
    setError(null);
    try {
      await f();
      await load();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };
  if (sinks === null) return null;
  return (
    <Panel title="Stream to a SIEM">
      <div className="space-y-3 p-5 text-[12px]">
        <p className="text-muted">
          New rows go to each URL in order, in signed batches (<code className="font-mono">X-Roll-Signature</code>, as roll's
          webhooks), and a failed batch is sent again later.
        </p>
        {sinks.length > 0 && (
          <ul className="divide-y divide-line rounded-lg border border-line">
            {sinks.map((s) => (
              <li key={s.id} className="flex flex-wrap items-center gap-3 px-3 py-2">
                <span className={`w-12 shrink-0 ${s.status === "error" ? "text-danger" : s.status === "ok" ? "text-ok" : "text-subtle"}`}>{s.status}</span>
                <span className="min-w-0 flex-1 truncate font-mono">{s.url}</span>
                <span className="shrink-0 text-subtle">{s.delivered_at ? `sent ${relative(s.delivered_at)}` : "nothing sent yet"}</span>
                {s.status === "error" && (
                  <button onClick={() => void act(() => accessApi.retrySink(s.id))} className={btn}>
                    Retry now
                  </button>
                )}
                <button onClick={() => void act(() => accessApi.removeSink(s.id))} className={btn}>
                  Remove
                </button>
                {s.error && <p className="basis-full text-danger">{s.error}</p>}
              </li>
            ))}
          </ul>
        )}
        {made && (
          <div className="rise space-y-1 rounded-lg border border-line bg-panel-2 p-3">
            <p className="text-muted">The signing secret, shown this once:</p>
            <div className="flex items-center gap-2">
              <code className="min-w-0 flex-1 truncate font-mono">{made}</code>
              <CopyButton id="sink-secret" text={made} copied={copied} onCopy={copy} />
            </div>
          </div>
        )}
        <form
          className="flex flex-wrap items-center gap-2"
          onSubmit={(e) => {
            e.preventDefault();
            void act(async () => {
              const r = await accessApi.addSink(url.trim(), fromStart);
              setMade(r.secret);
              setUrl("");
            });
          }}
        >
          <input value={url} onChange={(e) => setUrl(e.target.value)} placeholder="https://siem.example/roll" className={`${input} min-w-64 flex-1 font-mono text-[12px]`} aria-label="Sink URL" />
          <label className="flex items-center gap-1.5 text-muted">
            <input type="checkbox" checked={fromStart} onChange={(e) => setFromStart(e.target.checked)} />
            send what the log still has
          </label>
          <button type="submit" disabled={!url.trim().startsWith("https://")} className={primary}>
            Add
          </button>
        </form>
        <ErrorNote error={error} />
      </div>
    </Panel>
  );
}
