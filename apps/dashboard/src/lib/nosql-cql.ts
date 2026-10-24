/**
 * CQL text helpers for the nosql console and browsers: statement splitting (as cqlsh does),
 * identifier quoting, schema-aware completion and schema-diff classification. Pure functions,
 * no React, so they are unit-tested (apps/dashboard/test).
 */

import type { NosqlSchema, SchemaKeyspace } from "./nosql-api";

/**
 * Split CQL text into statements on top-level `;`. Quotes (`'…'`, `"…"`, `$$…$$`) and comments
 * (`--`, `//`, `/* *\/`) are respected; comment-only pieces are dropped. A `BEGIN … BATCH` stays
 * one statement through its `APPLY BATCH`, like cqlsh.
 */
export function splitStatements(text: string): string[] {
  const pieces: string[] = [];
  let cur = "";
  let code = false;
  let i = 0;
  const push = () => {
    if (code && cur.trim()) pieces.push(cur.trim());
    cur = "";
    code = false;
  };
  while (i < text.length) {
    const c = text[i];
    const two = text.slice(i, i + 2);
    if (two === "--" || two === "//") {
      const end = text.indexOf("\n", i);
      const stop = end < 0 ? text.length : end;
      cur += text.slice(i, stop);
      i = stop;
      continue;
    }
    if (two === "/*") {
      const end = text.indexOf("*/", i + 2);
      const stop = end < 0 ? text.length : end + 2;
      cur += text.slice(i, stop);
      i = stop;
      continue;
    }
    if (two === "$$") {
      const end = text.indexOf("$$", i + 2);
      const stop = end < 0 ? text.length : end + 2;
      cur += text.slice(i, stop);
      code = true;
      i = stop;
      continue;
    }
    if (c === "'" || c === '"') {
      let j = i + 1;
      while (j < text.length) {
        if (text[j] === c) {
          if (text[j + 1] === c) {
            j += 2;
            continue;
          }
          break;
        }
        j++;
      }
      cur += text.slice(i, j + 1);
      code = true;
      i = j + 1;
      continue;
    }
    if (c === ";") {
      push();
      i++;
      continue;
    }
    if (!/\s/.test(c)) code = true;
    cur += c;
    i++;
  }
  push();

  // Keep batches together.
  const out: string[] = [];
  let batch: string[] | null = null;
  for (const p of pieces) {
    const head = stripComments(p).trim();
    if (batch) {
      batch.push(p);
      if (/^APPLY\s+BATCH\b/i.test(head)) {
        out.push(batch.join(";\n"));
        batch = null;
      }
    } else if (/^BEGIN\s+((UNLOGGED|COUNTER)\s+)?BATCH\b/i.test(head) && !/\bAPPLY\s+BATCH\s*$/i.test(head)) {
      batch = [p];
    } else {
      out.push(p);
    }
  }
  if (batch) out.push(batch.join(";\n"));
  return out;
}

/** The statement without comments (strings kept). */
export function stripComments(s: string) {
  return s.replace(/('(?:[^']|'')*'|"(?:[^"]|"")*"|\$\$[\s\S]*?\$\$)|--[^\n]*|\/\/[^\n]*|\/\*[\s\S]*?\*\//g, (m, str) => str ?? " ");
}

const RESERVED = new Set(
  (
    "add allow alter and apply asc authorize batch begin by columnfamily create delete desc describe drop entries execute " +
    "from full grant if in index infinity insert into is keyspace limit modify nan norecursive not null of on or order " +
    "primary rename replace revoke schema select set table to token truncate unlogged update use using view where with"
  ).split(" "),
);

/** An identifier as CQL needs it written: bare when lowercase and not reserved, else `"quoted"`. */
export function quoteIdent(name: string) {
  return /^[a-z][a-z0-9_]*$/.test(name) && !RESERVED.has(name) ? name : `"${name.replace(/"/g, '""')}"`;
}

/** `ks.table`, each part quoted when needed. */
export function qualified(ks: string, table: string) {
  return `${quoteIdent(ks)}.${quoteIdent(table)}`;
}

/** A `SELECT *` over one table, as the data browser pages it. */
export function selectAll(ks: string, table: string) {
  return `SELECT * FROM ${qualified(ks, table)}`;
}

/** The trace events of a traced statement (`system_traces.events`). */
export function traceEventsQuery(sessionId: string) {
  if (!/^[0-9a-f-]{36}$/i.test(sessionId)) throw new Error(`bad tracing session id ${sessionId}`);
  return `SELECT activity, source, source_elapsed, thread FROM system_traces.events WHERE session_id = ${sessionId}`;
}

/** cqlsh shell commands the server doesn't run (the console handles or explains them). */
export function shellCommand(statement: string): { cmd: string; arg: string } | null {
  const m = /^\s*(CLEAR|CLS|TRACING|CONSISTENCY|SERIAL\s+CONSISTENCY|PAGING|EXPAND|SOURCE|CAPTURE|SHOW|HELP|EXIT|QUIT|COPY|LOGIN|DEBUG)\b\s*([\s\S]*)$/i.exec(
    stripComments(statement),
  );
  return m ? { cmd: m[1].toUpperCase().replace(/\s+/g, " "), arg: m[2].trim() } : null;
}

/** The kind of a schema-diff statement, for its gutter mark. */
export function diffKind(statement: string): "create" | "drop" | "alter" | "other" {
  const head = stripComments(statement).trim().toUpperCase();
  if (head.startsWith("CREATE")) return "create";
  if (head.startsWith("DROP")) return "drop";
  if (head.startsWith("ALTER")) return "alter";
  return "other";
}

// ---------------------------------------------------------------------------------------------
// Completion.

export const KEYWORDS = (
  "SELECT FROM WHERE AND INSERT INTO VALUES UPDATE SET DELETE CREATE ALTER DROP TABLE KEYSPACE TYPE INDEX " +
  "MATERIALIZED VIEW IF NOT EXISTS PRIMARY KEY WITH USING TTL TIMESTAMP LIMIT ALLOW FILTERING ORDER BY ASC DESC " +
  "USE TRUNCATE DESCRIBE BEGIN BATCH APPLY UNLOGGED COUNTER GRANT REVOKE ROLE ROLES USER PERMISSIONS ON TO OF IN " +
  "CONTAINS DISTINCT COUNT JSON TOKEN GROUP PER PARTITION KEYSPACES TABLES TYPES SCHEMA CLUSTERING REPLICATION " +
  "DURABLE_WRITES AS LIKE NULL TRUE FALSE WRITETIME TTL CAST FUNCTION AGGREGATE CUSTOM STATIC FROZEN ADD RENAME"
).split(" ");

export const TYPES = (
  "ascii bigint blob boolean counter date decimal double duration float inet int smallint text time timestamp " +
  "timeuuid tinyint uuid varchar varint list set map frozen tuple vector"
).split(" ");

export interface Completion {
  label: string;
  /** Text that replaces `[from, to)`. */
  insert: string;
  kind: "keyword" | "keyspace" | "table" | "view" | "column" | "type";
  detail?: string;
}

export interface CompletionResult {
  from: number;
  to: number;
  items: Completion[];
}

const IDENT = String.raw`(?:"(?:[^"]|"")+"|[A-Za-z_][A-Za-z0-9_]*)`;
const unquote = (s: string) => (s.startsWith('"') ? s.slice(1, -1).replace(/""/g, '"') : s.toLowerCase());

function userKeyspaces(schema: NosqlSchema) {
  return [...schema.keyspaces].sort((a, b) => Number(a.system) - Number(b.system) || a.name.localeCompare(b.name));
}

function findKs(schema: NosqlSchema, name: string): SchemaKeyspace | undefined {
  return schema.keyspaces.find((k) => k.name === name) ?? schema.keyspaces.find((k) => k.name.toLowerCase() === name.toLowerCase());
}

/** Tables named in a statement (`FROM`, `INTO`, `UPDATE`, `TABLE`, `ON`), resolved against `keyspace`. */
export function referencedTables(statement: string, keyspace: string | null): { ks: string; table: string }[] {
  const re = new RegExp(String.raw`\b(?:FROM|INTO|UPDATE|TABLE|ON)\s+(${IDENT})(?:\s*\.\s*(${IDENT}))?`, "gi");
  const out: { ks: string; table: string }[] = [];
  for (const m of stripComments(statement).matchAll(re)) {
    if (m[2]) out.push({ ks: unquote(m[1]), table: unquote(m[2]) });
    else if (keyspace) out.push({ ks: keyspace, table: unquote(m[1]) });
  }
  return out;
}

/** The statement around `pos` (between top-level semicolons, roughly). */
function statementAt(text: string, pos: number) {
  const start = text.lastIndexOf(";", pos - 1) + 1;
  const end = text.indexOf(";", pos);
  return { before: text.slice(start, pos), whole: text.slice(start, end < 0 ? text.length : end) };
}

/**
 * Completions at `pos`: keywords, keyspaces, tables and views (of the session keyspace or a
 * `ks.` prefix), and the columns of the tables the statement names.
 */
export function complete(text: string, pos: number, schema: NosqlSchema | null, keyspace: string | null): CompletionResult {
  const { before, whole } = statementAt(text, pos);
  // Inside a string or comment: nothing.
  const quotes = (stripComments(before).match(/'/g) ?? []).length;
  if (quotes % 2 === 1 || /--[^\n]*$|\/\/[^\n]*$/.test(before)) return { from: pos, to: pos, items: [] };

  const word = /("?[A-Za-z0-9_]*"?\s*\.\s*)?"?[A-Za-z0-9_]*$/.exec(before)?.[0] ?? "";
  let from = pos - word.length;
  let to = pos;
  // Replace the rest of the word after the cursor too.
  const after = /^[A-Za-z0-9_]*"?/.exec(text.slice(pos))?.[0] ?? "";
  to += after.length;

  const dot = word.indexOf(".");
  const prev = before
    .slice(0, before.length - word.length)
    .trimEnd()
    .match(/([A-Za-z_]+|[,(=<>*])$/)?.[1]
    ?.toUpperCase();
  const items: Completion[] = [];
  const match = (label: string, prefix: string) => label.toLowerCase().startsWith(prefix.toLowerCase());

  if (dot >= 0 && schema) {
    // `ks.<table>`
    const ksName = unquote(word.slice(0, dot).trim());
    const prefix = word.slice(dot + 1).trim().replace(/^"/, "");
    from = pos - word.slice(dot + 1).trimStart().length;
    const ks = findKs(schema, ksName);
    for (const t of ks?.tables ?? []) if (match(t.name, prefix)) items.push({ label: t.name, insert: quoteIdent(t.name), kind: "table" });
    for (const v of ks?.views ?? []) if (match(v.name, prefix)) items.push({ label: v.name, insert: quoteIdent(v.name), kind: "view" });
    for (const u of ks?.types ?? []) if (match(u.name, prefix)) items.push({ label: u.name, insert: quoteIdent(u.name), kind: "type" });
    return { from, to, items: items.slice(0, 50) };
  }

  const prefix = word.replace(/^"/, "");
  const keyspaces = () => {
    for (const k of schema ? userKeyspaces(schema) : []) {
      if (match(k.name, prefix)) items.push({ label: k.name, insert: quoteIdent(k.name), kind: "keyspace", detail: k.system ? "system" : undefined });
    }
  };
  const tables = () => {
    const ks = schema && keyspace ? findKs(schema, keyspace) : undefined;
    for (const t of ks?.tables ?? []) if (match(t.name, prefix)) items.push({ label: t.name, insert: quoteIdent(t.name), kind: "table", detail: ks!.name });
    for (const v of ks?.views ?? []) if (match(v.name, prefix)) items.push({ label: v.name, insert: quoteIdent(v.name), kind: "view", detail: ks!.name });
    for (const k of schema ? userKeyspaces(schema) : []) {
      if (match(k.name, prefix)) items.push({ label: `${k.name}.`, insert: `${quoteIdent(k.name)}.`, kind: "keyspace", detail: k.system ? "system" : undefined });
    }
  };
  const columns = () => {
    const seen = new Set<string>();
    for (const r of referencedTables(whole, keyspace)) {
      const ks = schema ? findKs(schema, r.ks) : undefined;
      const t = ks?.tables.find((x) => x.name === r.table) ?? ks?.views.find((x) => x.name === r.table);
      for (const c of t?.columns ?? []) {
        if (!seen.has(c.name) && match(c.name, prefix)) {
          seen.add(c.name);
          items.push({ label: c.name, insert: quoteIdent(c.name), kind: "column", detail: c.type });
        }
      }
    }
  };
  const keywords = (list = KEYWORDS) => {
    if (!prefix) return;
    const upper = prefix === prefix.toLowerCase() && /[a-z]/.test(prefix) ? false : true;
    for (const k of new Set(list)) {
      if (match(k, prefix) && k.toLowerCase() !== prefix.toLowerCase()) items.push({ label: k, insert: upper ? k : k.toLowerCase(), kind: "keyword" });
    }
  };

  switch (prev) {
    case "FROM":
    case "INTO":
    case "UPDATE":
    case "TRUNCATE":
    case "ON":
      tables();
      break;
    case "TABLE":
    case "VIEW":
    case "COLUMNFAMILY":
      if (/\b(CREATE)\s+(TABLE|MATERIALIZED\s+VIEW)\s*(IF\s+NOT\s+EXISTS\s*)?$/i.test(before.slice(0, before.length - word.length).trimEnd())) break;
      tables();
      break;
    case "USE":
    case "KEYSPACE":
      keyspaces();
      break;
    case "SELECT":
    case "WHERE":
    case "AND":
    case "SET":
    case "BY":
    case ",":
    case "(":
    case "DISTINCT":
    case "IF":
      columns();
      keywords();
      break;
    default:
      if (prefix) {
        columns();
        keywords();
      }
  }
  // Column types while defining tables and types.
  const defining = /^\s*(CREATE|ALTER)\b/i.test(stripComments(whole));
  if (defining && prefix && !["FROM", "INTO", "UPDATE", "TRUNCATE", "ON", "TABLE", "VIEW", "USE", "KEYSPACE"].includes(prev ?? "")) {
    for (const t of TYPES) if (match(t, prefix) && t !== prefix.toLowerCase()) items.push({ label: t, insert: t, kind: "type" });
  }
  // Exact matches only would be noise.
  const filtered = items.filter((c) => c.insert !== word);
  return { from, to, items: filtered.slice(0, 50) };
}

/** Apply a completion: the new text and cursor position. */
export function applyCompletion(text: string, r: CompletionResult, c: Completion): { text: string; pos: number } {
  const next = text.slice(0, r.from) + c.insert + text.slice(r.to);
  return { text: next, pos: r.from + c.insert.length };
}
