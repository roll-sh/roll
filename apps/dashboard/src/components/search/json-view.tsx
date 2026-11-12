import { Fragment, useMemo } from "react";

/**
 * JSON with quiet monochrome highlighting: keys in the foreground colour, strings muted,
 * numbers and literals between, punctuation subtle. A single regex pass over the text, so key
 * order and formatting are exactly what was given (Elasticsearch's own order).
 */
const TOKEN = /("(?:\\.|[^"\\])*")(\s*:)?|\b(?:true|false|null)\b|-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?|[{}[\],]/g;

export type JsonPart = { text: string; kind: "key" | "string" | "literal" | "punct" | "plain" };

export function jsonParts(text: string): JsonPart[] {
  const parts: JsonPart[] = [];
  let last = 0;
  for (const m of text.matchAll(TOKEN)) {
    const i = m.index ?? 0;
    if (i > last) parts.push({ text: text.slice(last, i), kind: "plain" });
    if (m[1] !== undefined) {
      parts.push({ text: m[1], kind: m[2] ? "key" : "string" });
      if (m[2]) parts.push({ text: m[2], kind: "punct" });
    } else {
      parts.push({ text: m[0], kind: /^[{}[\],]$/.test(m[0]) ? "punct" : "literal" });
    }
    last = i + m[0].length;
  }
  if (last < text.length) parts.push({ text: text.slice(last), kind: "plain" });
  return parts;
}

const TONE: Record<JsonPart["kind"], string> = {
  key: "text-fg",
  string: "text-muted",
  literal: "text-fg/75",
  punct: "text-subtle",
  plain: "",
};

/** Highlighting stops past this size: a 4 MiB response would make hundreds of thousands of spans. */
const HIGHLIGHT_LIMIT = 256 * 1024;

export function JsonText({ text, json = true }: { text: string; json?: boolean }) {
  const parts = useMemo(() => (json && text.length <= HIGHLIGHT_LIMIT ? jsonParts(text) : null), [text, json]);
  if (!parts) return <>{text}</>;
  return (
    <>
      {parts.map((p, i) =>
        p.kind === "plain" ? (
          <Fragment key={i}>{p.text}</Fragment>
        ) : (
          <span key={i} className={TONE[p.kind]}>
            {p.text}
          </span>
        ),
      )}
    </>
  );
}
