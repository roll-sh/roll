/**
 * Importing a file into an index from the dashboard (Kibana's "Upload a file", the hosted search
 * products' JSON upload): NDJSON, a JSON array (or an object holding one) and CSV/TSV, read in
 * chunks so a large NDJSON or CSV file never sits in memory whole, typed by column for CSV, and
 * sent as `_bulk` batches bounded by bytes.
 */

export type ImportFormat = "ndjson" | "json" | "csv";

/** JSON files are parsed whole; anything larger must be NDJSON or CSV (read in chunks). */
export const JSON_MAX_BYTES = 100 * 1024 * 1024;

/** The format of a file, from its name and first bytes. */
export function detectFormat(name: string, head: string): ImportFormat {
  const ext = name.toLowerCase().split(".").pop() ?? "";
  if (ext === "csv" || ext === "tsv") return "csv";
  if (ext === "ndjson" || ext === "jsonl") return "ndjson";
  const t = head.replace(/^﻿/, "").trimStart();
  if (t.startsWith("[")) return "json";
  if (t.startsWith("{")) {
    // One object per line is NDJSON; an object spread over lines is one JSON document.
    const first = t.split("\n", 1)[0].trim();
    try {
      JSON.parse(first);
      return "ndjson";
    } catch {
      return "json";
    }
  }
  return ext === "json" ? "json" : "csv";
}

/** A lowercase index name from a file name, as Elasticsearch allows them. */
export function indexNameFrom(file: string): string {
  const stem = file.replace(/\.[^.]*$/, "");
  const name = stem
    .toLowerCase()
    .replace(/[\\/*?"<>| ,#:]+/g, "-")
    .replace(/^[-_+.]+/, "")
    .replace(/-+$/, "")
    .slice(0, 200);
  return name || "import";
}

/** Why Elasticsearch would refuse an index name, or `null`. */
export function indexNameProblem(name: string): string | null {
  if (!name) return "Name the index.";
  if (name !== name.toLowerCase()) return "Index names are lowercase.";
  if (/[\\/*?"<>| ,#:]/.test(name)) return 'Index names can\'t hold \\ / * ? " < > | , # : or spaces.';
  if (/^[-_+]/.test(name)) return "Index names can't start with -, _ or +.";
  if (name === "." || name === "..") return "Index names can't be . or ..";
  if (new TextEncoder().encode(name).length > 255) return "Index names are at most 255 bytes.";
  return null;
}

/** A document that couldn't be read from the file. */
export interface ReadError {
  /** Line (NDJSON, CSV: the record's first line) or element (JSON) number, from 1. */
  at: number;
  message: string;
}

/** Splits NDJSON as chunks arrive. */
export class NdjsonReader {
  private rest = "";
  private line = 0;
  /** The first few lines that weren't documents, and how many there were. */
  readonly errors: ReadError[] = [];
  errorCount = 0;

  private fail(message: string) {
    this.errorCount++;
    if (this.errors.length < 5) this.errors.push({ at: this.line, message });
  }

  push(text: string): Record<string, unknown>[] {
    const lines = (this.rest + text).split("\n");
    this.rest = lines.pop() ?? "";
    return this.parse(lines);
  }

  end(): Record<string, unknown>[] {
    const last = this.rest;
    this.rest = "";
    return this.parse([last]);
  }

  private parse(lines: string[]): Record<string, unknown>[] {
    const out: Record<string, unknown>[] = [];
    for (const raw of lines) {
      this.line++;
      const l = (this.line === 1 ? raw.replace(/^﻿/, "") : raw).trim();
      if (!l) continue;
      try {
        const v: unknown = JSON.parse(l);
        if (v && typeof v === "object" && !Array.isArray(v)) out.push(v as Record<string, unknown>);
        else this.fail("not a JSON object");
      } catch (e) {
        this.fail(e instanceof Error ? e.message : String(e));
      }
    }
    return out;
  }
}

/** The documents of a JSON file: an array of objects, an object holding exactly one such
 * array (`{"products": [...]}`), or one object. */
export function jsonDocuments(text: string): { docs: Record<string, unknown>[]; errors: ReadError[] } {
  const v: unknown = JSON.parse(text.replace(/^﻿/, ""));
  const isObj = (x: unknown): x is Record<string, unknown> => !!x && typeof x === "object" && !Array.isArray(x);
  let items: unknown[];
  if (Array.isArray(v)) items = v;
  else if (isObj(v)) {
    const arrays = Object.values(v).filter((x) => Array.isArray(x) && x.length > 0 && x.every(isObj));
    items = arrays.length === 1 ? (arrays[0] as unknown[]) : [v];
  } else throw new Error("The file holds no JSON objects.");
  const docs: Record<string, unknown>[] = [];
  const errors: ReadError[] = [];
  items.forEach((x, i) => (isObj(x) ? docs.push(x) : errors.push({ at: i + 1, message: "not a JSON object" })));
  return { docs, errors };
}

/**
 * Splits CSV (RFC 4180: quoted fields may hold the delimiter, quotes doubled, line breaks) as
 * chunks arrive. The delimiter is the first line's most common of `,` `;` and tab.
 */
export class CsvReader {
  private delim: string | null;
  private field = "";
  private record: string[] = [];
  private quoted = false;
  /** The last character was a quote inside a quoted field (an escape or the field's end). */
  private quoteSeen = false;
  private started = false;
  /** Text held back until the first line is whole (the delimiter is guessed from it). */
  private pending = "";
  private line = 1;
  private recordLine = 1;

  constructor(delimiter?: string) {
    this.delim = delimiter ?? null;
  }

  /** The delimiter given or found on the first line (`null` before any text). */
  get delimiter(): string | null {
    return this.delim;
  }

  /** Records completed by this chunk, each with the line it started on. */
  push(text: string): { at: number; cells: string[] }[] {
    if (!this.started) {
      text = text.replace(/^﻿/, "");
      this.started = true;
    }
    if (this.delim === null) {
      this.pending += text;
      if (!this.pending.includes("\n")) return [];
      text = this.pending;
      this.pending = "";
      this.delim = guessDelimiter(text.split(/\r?\n/, 1)[0]);
    }
    const out: { at: number; cells: string[] }[] = [];
    const d = this.delim;
    for (let i = 0; i < text.length; i++) {
      const c = text[i];
      if (this.quoted) {
        if (this.quoteSeen) {
          this.quoteSeen = false;
          if (c === '"') {
            this.field += '"';
            continue;
          }
          this.quoted = false;
          // Falls through: the character after the closing quote.
        } else if (c === '"') {
          this.quoteSeen = true;
          continue;
        } else {
          if (c === "\n") this.line++;
          this.field += c;
          continue;
        }
      }
      if (c === '"' && this.field === "") {
        this.quoted = true;
      } else if (c === d) {
        this.record.push(this.field);
        this.field = "";
      } else if (c === "\n") {
        this.finish(out);
        this.line++;
        this.recordLine = this.line;
      } else if (c !== "\r") {
        this.field += c;
      }
    }
    return out;
  }

  end(): { at: number; cells: string[] }[] {
    if (this.delim === null) {
      // A file of one line.
      this.delim = guessDelimiter(this.pending);
      const text = this.pending;
      this.pending = "";
      const out = this.push(text);
      return [...out, ...this.end()];
    }
    const out: { at: number; cells: string[] }[] = [];
    this.quoted = false;
    this.quoteSeen = false;
    this.finish(out);
    return out;
  }

  private finish(out: { at: number; cells: string[] }[]) {
    this.record.push(this.field);
    this.field = "";
    const r = this.record;
    this.record = [];
    if (r.length === 1 && r[0] === "") return;
    out.push({ at: this.recordLine, cells: r });
  }
}

function guessDelimiter(line: string): string {
  let best = ",";
  let most = 0;
  for (const d of [",", ";", "\t"]) {
    const n = line.split(d).length - 1;
    if (n > most) {
      best = d;
      most = n;
    }
  }
  return best;
}

export type ColumnType = "long" | "double" | "boolean" | "string";

const INT = /^-?(0|[1-9]\d{0,15})$/;
const FLOAT = /^-?(0|[1-9]\d*)?(\.\d+)?([eE][+-]?\d+)?$/;

/**
 * Each column's type from a sample of rows: a number or boolean only when every non-empty value
 * in the sample is one (`007` and `1e400` stay strings: leading zeros are identifiers, and a
 * value past a double is not a number Elasticsearch keeps).
 */
export function columnTypes(rows: string[][], width: number): ColumnType[] {
  const types: ColumnType[] = [];
  for (let c = 0; c < width; c++) {
    let t: ColumnType | null = null;
    for (const r of rows) {
      const v = (r[c] ?? "").trim();
      if (v === "") continue;
      const own: ColumnType = INT.test(v) ? "long" : FLOAT.test(v) && /\d/.test(v) && Number.isFinite(Number(v)) ? "double" : /^(true|false)$/i.test(v) ? "boolean" : "string";
      t = t === null ? own : t === own ? t : (t === "long" && own === "double") || (t === "double" && own === "long") ? "double" : "string";
      if (t === "string") break;
    }
    types.push(t ?? "string");
  }
  return types;
}

/** A CSV record as a document: header names as fields, typed values, empty cells left out. A
 * value that doesn't fit its column's type is kept as text (Elasticsearch then reports it). */
export function csvDocument(header: string[], types: ColumnType[], cells: string[]): Record<string, unknown> {
  const doc: Record<string, unknown> = {};
  header.forEach((name, i) => {
    const raw = cells[i];
    if (raw === undefined || raw.trim() === "" || !name) return;
    const v = raw.trim();
    switch (types[i]) {
      case "long":
      case "double": {
        const n = Number(v);
        doc[name] = v !== "" && Number.isFinite(n) && FLOAT.test(v) ? n : raw;
        break;
      }
      case "boolean":
        doc[name] = /^true$/i.test(v) ? true : /^false$/i.test(v) ? false : raw;
        break;
      default:
        doc[name] = raw;
    }
  });
  return doc;
}

/** Header names: blank ones named by position, repeats numbered (`name`, `name_2`). */
export function csvHeader(cells: string[]): string[] {
  const seen = new Map<string, number>();
  return cells.map((c, i) => {
    const base = c.trim() || `column_${i + 1}`;
    const n = (seen.get(base) ?? 0) + 1;
    seen.set(base, n);
    return n === 1 ? base : `${base}_${n}`;
  });
}

/** The `_bulk` lines of one document (the index comes from the route; `_id` from `idField`). */
export function bulkLines(doc: Record<string, unknown>, idField: string | null): string {
  const id = idField ? doc[idField] : undefined;
  const action = id === undefined || id === null || id === "" ? { index: {} } : { index: { _id: String(id) } };
  return `${JSON.stringify(action)}\n${JSON.stringify(doc)}\n`;
}

/** Collects bulk lines into bodies of at most `maxBytes` (one larger document goes alone). */
export class BulkBatcher {
  private parts: string[] = [];
  private bytes = 0;
  private count = 0;
  private readonly enc = new TextEncoder();

  constructor(private readonly maxBytes: number) {}

  /** Adds a document's lines; returns a full batch when this one doesn't fit beside it. */
  add(lines: string): { body: string; docs: number } | null {
    const n = this.enc.encode(lines).length;
    let full: { body: string; docs: number } | null = null;
    if (this.count > 0 && this.bytes + n > this.maxBytes) full = this.take();
    this.parts.push(lines);
    this.bytes += n;
    this.count++;
    return full;
  }

  take(): { body: string; docs: number } | null {
    if (this.count === 0) return null;
    const b = { body: this.parts.join(""), docs: this.count };
    this.parts = [];
    this.bytes = 0;
    this.count = 0;
    return b;
  }
}

/** A `_bulk` answer's failures, each with its position in the batch. */
export function bulkFailures(resp: unknown): { at: number; status: number; reason: string }[] {
  const items = (resp as { items?: Record<string, { status?: number; error?: { type?: string; reason?: string } }>[] } | null)?.items ?? [];
  const out: { at: number; status: number; reason: string }[] = [];
  items.forEach((it, i) => {
    const r = Object.values(it)[0];
    if (r?.error) out.push({ at: i, status: r.status ?? 0, reason: [r.error.type, r.error.reason].filter(Boolean).join(": ") });
  });
  return out;
}
