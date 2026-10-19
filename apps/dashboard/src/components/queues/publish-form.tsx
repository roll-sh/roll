"use client";

import { Send } from "lucide-react";
import { useState } from "react";
import { exchangeLabel, type PublishResult, qapi } from "@/lib/queues-api";
import { JsonEditor, parseObject } from "./args-editor";
import { useAction } from "./hooks";
import { ErrorNote, Field, ICON, input, Pending, primary, Segmented, Toggle } from "./ui";

const CONTENT_TYPES = ["application/json", "text/plain", "application/octet-stream"];

function routedText({ routed, queues }: PublishResult) {
  const n = typeof routed === "number" ? routed : queues;
  if (n !== undefined) return n > 0 ? `Routed to ${n} queue${n === 1 ? "" : "s"}` : null;
  return routed ? "Routed" : null;
}

/** Publish a message through an exchange (the default exchange routes straight to a queue). */
export function PublishForm({
  vhost,
  exchange: fixedExchange,
  exchanges,
  routingKey: initialKey = "",
  onPublished,
}: {
  vhost: string;
  /** Publish to this exchange only. */
  exchange?: string;
  /** Otherwise let the user pick one of these. */
  exchanges?: string[];
  routingKey?: string;
  onPublished?: () => void;
}) {
  const { busy, error, setError, run } = useAction();
  const [exchange, setExchange] = useState(fixedExchange ?? exchanges?.[0] ?? "");
  const [routingKey, setRoutingKey] = useState(initialKey);
  const [body, setBody] = useState('{\n  "hello": "roll"\n}');
  const [encoding, setEncoding] = useState<"utf8" | "base64">("utf8");
  const [contentType, setContentType] = useState("application/json");
  const [persistent, setPersistent] = useState(true);
  const [more, setMore] = useState(false);
  const [headers, setHeaders] = useState("{}");
  const [messageId, setMessageId] = useState("");
  const [expiration, setExpiration] = useState("");
  const [result, setResult] = useState<{ ok: boolean; text: string } | null>(null);

  const headersParsed = parseObject(headers);
  const ex = fixedExchange ?? exchange;

  const submit = async () => {
    setResult(null);
    if (headersParsed.error) return setError(`Headers: ${headersParsed.error}`);
    if (encoding === "base64" && !/^[A-Za-z0-9+/=\s]*$/.test(body)) return setError("Body isn't valid base64");
    if (expiration && !/^\d+$/.test(expiration)) return setError("Expiration is a TTL in milliseconds");
    const properties: Record<string, unknown> = { delivery_mode: persistent ? 2 : 1 };
    if (contentType) properties.content_type = contentType;
    if (headersParsed.value && Object.keys(headersParsed.value).length) properties.headers = headersParsed.value;
    if (messageId) properties.message_id = messageId;
    if (expiration) properties.expiration = expiration;
    const out = await run("publish", () =>
      qapi.publish(vhost, { exchange: ex, routing_key: routingKey, body: encoding === "base64" ? body.replace(/\s+/g, "") : body, body_encoding: encoding, properties }),
    );
    if (out) {
      const text = routedText(out);
      setResult(text ? { ok: true, text } : { ok: false, text: "Published, but no binding matched, so the message was dropped" });
      onPublished?.();
    }
  };

  const prettify = () => {
    try {
      setBody(JSON.stringify(JSON.parse(body), null, 2));
    } catch {
      setError("Body isn't valid JSON");
    }
  };

  return (
    <form
      className="space-y-3 px-5 py-4"
      onSubmit={(e) => {
        e.preventDefault();
        submit();
      }}
    >
      <div className="grid gap-2 sm:grid-cols-2">
        {fixedExchange === undefined && (
          <Field label="Exchange">
            <select value={exchange} onChange={(e) => setExchange(e.target.value)} className={`${input} w-full font-mono text-[12px]`}>
              {(exchanges ?? [""]).map((x) => (
                <option key={x} value={x}>
                  {exchangeLabel(x)}
                </option>
              ))}
            </select>
          </Field>
        )}
        <Field label="Routing key" className={fixedExchange === undefined ? "" : "sm:col-span-2"}>
          <input
            value={routingKey}
            onChange={(e) => setRoutingKey(e.target.value)}
            placeholder="orders.created"
            spellCheck={false}
            className={`${input} w-full font-mono text-[12px]`}
          />
        </Field>
      </div>

      <div className="space-y-1">
        <div className="flex items-center justify-between">
          <span className="text-[11px] text-subtle">Payload</span>
          <div className="flex items-center gap-2">
            {encoding === "utf8" && (
              <button type="button" onClick={prettify} className="text-[11px] text-subtle hover:text-fg">
                Format JSON
              </button>
            )}
            <Segmented value={encoding} options={[{ value: "utf8", label: "text" }, { value: "base64", label: "base64" }]} onChange={setEncoding} />
          </div>
        </div>
        <textarea
          value={body}
          onChange={(e) => setBody(e.target.value)}
          onKeyDown={(e) => {
            if ((e.metaKey || e.ctrlKey) && e.key === "Enter") {
              e.preventDefault();
              submit();
            }
          }}
          rows={5}
          spellCheck={false}
          className={`${input} block w-full resize-y font-mono text-[12px] leading-relaxed`}
        />
      </div>

      <div className="flex flex-wrap items-center gap-3">
        <select value={contentType} onChange={(e) => setContentType(e.target.value)} className={`${input} font-mono text-[12px]`} aria-label="Content type">
          <option value="">no content_type</option>
          {CONTENT_TYPES.map((c) => (
            <option key={c} value={c}>
              {c}
            </option>
          ))}
        </select>
        <Toggle checked={persistent} onChange={setPersistent} label="Persistent" />
        <button type="button" onClick={() => setMore((m) => !m)} className="text-[12px] text-subtle hover:text-fg">
          {more ? "Fewer properties" : "More properties"}
        </button>
      </div>

      {more && (
        <div className="rise grid gap-3 sm:grid-cols-2">
          <Field label="message_id">
            <input value={messageId} onChange={(e) => setMessageId(e.target.value)} className={`${input} w-full font-mono text-[12px]`} />
          </Field>
          <Field label="expiration (ms)">
            <input value={expiration} onChange={(e) => setExpiration(e.target.value)} inputMode="numeric" className={`${input} w-full font-mono text-[12px]`} />
          </Field>
          <div className="sm:col-span-2">
            <JsonEditor label="Headers" value={headers} onChange={setHeaders} rows={3} suggestions={[{ key: "x-delay", value: 5000, title: "Delay in ms (x-delayed-message exchanges)" }]} />
          </div>
        </div>
      )}

      <ErrorNote error={error} onClose={() => setError(null)} />
      <div className="flex items-center justify-between gap-3">
        <p className={`min-w-0 truncate text-[12px] ${result ? (result.ok ? "text-ok" : "text-muted") : "text-subtle"}`}>
          {result ? result.text : <span className="font-mono">{exchangeLabel(ex)}</span>}
        </p>
        <button type="submit" disabled={busy !== null} className={primary}>
          {busy ? (
            <Pending>Publishing…</Pending>
          ) : (
            <>
              <Send {...ICON} /> Publish <kbd className="font-mono text-[10px] opacity-60">⌘↵</kbd>
            </>
          )}
        </button>
      </div>
    </form>
  );
}
