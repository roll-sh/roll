import { FileUp, X } from "lucide-react";
import { useRef, useState } from "react";
import { formatBytes } from "@/lib/roll-api";
import { esRaw, formatCount, indexPath, type SearchCluster } from "@/lib/search-api";
import {
  BulkBatcher,
  bulkFailures,
  bulkLines,
  columnTypes,
  csvDocument,
  csvHeader,
  CsvReader,
  detectFormat,
  indexNameFrom,
  indexNameProblem,
  JSON_MAX_BYTES,
  jsonDocuments,
  NdjsonReader,
  type ColumnType,
  type ImportFormat,
  type ReadError,
} from "@/lib/search-import";
import { Field } from "../redis/shared";
import { btn, ErrorNote, input, Panel, primary } from "../ui";

/** Bytes read from the file at a time. */
const CHUNK = 1024 * 1024;
/** Rows that decide a CSV column's type. */
const SAMPLE = 1000;
const FORMATS: Record<ImportFormat, string> = { ndjson: "NDJSON", json: "JSON", csv: "CSV" };

interface Picked {
  file: File;
  format: ImportFormat;
  /** The first documents, as they will be indexed. */
  preview: Record<string, unknown>[];
  columns: string[];
  /** CSV: the header and each column's type. */
  csv?: { header: string[]; types: ColumnType[]; delimiter: string };
  /** JSON: every document (the file was parsed whole). */
  docs?: Record<string, unknown>[];
  readErrors: ReadError[];
}

interface Progress {
  read: number;
  sent: number;
  failed: number;
  failures: { doc: number; reason: string }[];
  /** Lines (elements) that weren't documents: the first few, and how many. */
  readErrors: ReadError[];
  unreadable: number;
  done: boolean;
  cancelled: boolean;
}

function cell(v: unknown) {
  if (v === undefined) return "";
  return typeof v === "object" ? JSON.stringify(v) : String(v);
}

async function readPreview(file: File): Promise<Picked> {
  const head = await file.slice(0, CHUNK).text();
  const format = detectFormat(file.name, head);
  if (format === "json") {
    if (file.size > JSON_MAX_BYTES) throw new Error(`JSON files over ${formatBytes(JSON_MAX_BYTES)} are read whole: save it as NDJSON (one document per line) instead.`);
    const { docs, errors } = jsonDocuments(await file.text());
    if (docs.length === 0) throw new Error("The file holds no JSON objects.");
    return { file, format, preview: docs.slice(0, 5), columns: columnsOf(docs.slice(0, 50)), docs, readErrors: errors };
  }
  if (format === "ndjson") {
    const r = new NdjsonReader();
    // Only whole lines of the first chunk (unless it is the whole file).
    const docs = file.size <= CHUNK ? [...r.push(head), ...r.end()] : r.push(head);
    if (docs.length === 0) throw new Error(r.errors[0] ? `Line ${r.errors[0].at}: ${r.errors[0].message}` : "The file holds no documents.");
    return { file, format, preview: docs.slice(0, 5), columns: columnsOf(docs.slice(0, 50)), readErrors: r.errors };
  }
  const r = new CsvReader();
  const recs = file.size <= CHUNK ? [...r.push(head), ...r.end()] : r.push(head);
  if (recs.length < 2) throw new Error("A CSV file needs a header row and at least one row of values.");
  const header = csvHeader(recs[0].cells);
  const rows = recs.slice(1, SAMPLE + 1).map((x) => x.cells);
  const types = columnTypes(rows, header.length);
  const delimiter = r.delimiter ?? ",";
  const preview = rows.slice(0, 5).map((c) => csvDocument(header, types, c));
  return { file, format, preview, columns: header, csv: { header, types, delimiter }, readErrors: [] };
}

function columnsOf(docs: Record<string, unknown>[]) {
  const seen = new Set<string>();
  for (const d of docs) for (const k of Object.keys(d)) seen.add(k);
  return [...seen];
}

/**
 * Import a file into an index: NDJSON, JSON or CSV, previewed before anything is sent, then
 * indexed in `_bulk` batches through the index's own route (so a user granted only that index
 * can import into it), with progress, cancel and the first failures.
 */
export function ImportPanel({ cluster, existing, onClose, onImported }: { cluster: SearchCluster; existing: string[]; onClose: () => void; onImported: (index: string) => void }) {
  const [picked, setPicked] = useState<Picked | null>(null);
  const [name, setName] = useState("");
  const [idField, setIdField] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [reading, setReading] = useState(false);
  const [progress, setProgress] = useState<Progress | null>(null);
  const [dragging, setDragging] = useState(false);
  const cancel = useRef(false);
  const running = progress !== null && !progress.done;

  const pick = async (file: File | undefined) => {
    if (!file) return;
    setError(null);
    setProgress(null);
    setReading(true);
    try {
      const p = await readPreview(file);
      setPicked(p);
      setName(indexNameFrom(file.name));
      setIdField(p.columns.includes("id") ? "id" : p.columns.includes("_id") ? "_id" : "");
    } catch (e) {
      setPicked(null);
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setReading(false);
    }
  };

  const run = async () => {
    if (!picked) return;
    const p = picked;
    const index = name.trim();
    const id = idField || null;
    cancel.current = false;
    setError(null);
    const state: Progress = { read: 0, sent: 0, failed: 0, failures: [], readErrors: p.format === "json" ? p.readErrors.slice(0, 5) : [], unreadable: p.format === "json" ? p.readErrors.length : 0, done: false, cancelled: false };
    const show = () => setProgress({ ...state, failures: [...state.failures], readErrors: [...state.readErrors] });
    show();
    // Half the cluster's body limit, at most 4 MiB a batch: small enough to keep the progress moving.
    const batcher = new BulkBatcher(Math.min(4 * 1024 * 1024, (cluster.max_body_mb * 1024 * 1024) / 2));
    let docNo = 0;
    const send = async (b: { body: string; docs: number } | null) => {
      if (!b) return;
      const r = await esRaw(cluster.id, "POST", `${indexPath(index)}/_bulk`, b.body);
      for (const f of bulkFailures(r.value)) {
        state.failed++;
        if (state.failures.length < 5) state.failures.push({ doc: state.sent + f.at + 1, reason: f.reason });
      }
      state.sent += b.docs;
      show();
    };
    const add = async (doc: Record<string, unknown>) => {
      docNo++;
      await send(batcher.add(bulkLines(doc, id)));
    };
    try {
      if (p.docs) {
        for (const d of p.docs) {
          if (cancel.current) break;
          await add(d);
          if (docNo % 1000 === 0) state.read = Math.round((p.file.size * docNo) / p.docs.length);
        }
        state.read = p.file.size;
      } else {
        const decoder = new TextDecoder();
        const nd = p.format === "ndjson" ? new NdjsonReader() : null;
        const csv = p.csv ? new CsvReader(p.csv.delimiter) : null;
        let header = true;
        const take = async (text: string, last: boolean) => {
          if (nd) {
            for (const d of last ? [...nd.push(text), ...nd.end()] : nd.push(text)) await add(d);
            state.readErrors = [...nd.errors];
            state.unreadable = nd.errorCount;
          } else if (csv && p.csv) {
            for (const r of last ? [...csv.push(text), ...csv.end()] : csv.push(text)) {
              if (header) {
                header = false;
                continue;
              }
              await add(csvDocument(p.csv.header, p.csv.types, r.cells));
            }
          }
        };
        for (let at = 0; at < p.file.size && !cancel.current; at += CHUNK) {
          const buf = await p.file.slice(at, at + CHUNK).arrayBuffer();
          const last = at + CHUNK >= p.file.size;
          await take(decoder.decode(buf, { stream: !last }), last);
          state.read = Math.min(p.file.size, at + CHUNK);
          show();
        }
      }
      if (!cancel.current) await send(batcher.take());
      // Searchable now rather than at the next refresh.
      await esRaw(cluster.id, "POST", `${indexPath(index)}/_refresh`).catch(() => undefined);
      state.cancelled = cancel.current;
      state.done = true;
      show();
      if (docNo > 0) onImported(index);
    } catch (e) {
      state.done = true;
      state.cancelled = true;
      show();
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  const nameProblem = picked ? indexNameProblem(name.trim()) : null;
  const exists = existing.includes(name.trim());

  return (
    <Panel
      title="Import data"
      className="rise"
      aside={
        <button onClick={onClose} disabled={running} className={btn} aria-label="Close import">
          <X className="size-3.5" />
        </button>
      }
    >
      <div className="space-y-4 px-5 py-4">
        {!running && !progress?.done && (
          <label
            onDragOver={(e) => {
              e.preventDefault();
              setDragging(true);
            }}
            onDragLeave={() => setDragging(false)}
            onDrop={(e) => {
              e.preventDefault();
              setDragging(false);
              pick(e.dataTransfer.files[0]);
            }}
            className={`flex cursor-pointer flex-col items-center gap-2 rounded-lg border border-dashed px-4 py-6 text-center transition ${
              dragging ? "border-accent bg-panel-2" : "border-line-2 hover:border-fg/40"
            }`}
          >
            <FileUp className="size-5 text-muted" strokeWidth={1.5} />
            <span className="text-[13px]">
              {reading ? <span className="shimmer">Reading…</span> : picked ? `${picked.file.name} · ${formatBytes(picked.file.size)} · ${FORMATS[picked.format]}` : "Drop a file, or choose one"}
            </span>
            <span className="text-[12px] text-subtle">NDJSON (one document per line), a JSON array, or CSV with a header row. Nothing is sent before you import.</span>
            <input type="file" accept=".json,.ndjson,.jsonl,.csv,.tsv,application/json,text/csv" className="sr-only" onChange={(e) => pick(e.target.files?.[0])} />
          </label>
        )}
        <ErrorNote error={error} />

        {picked && (
          <>
            <div className="overflow-x-auto rounded-lg border border-line">
              <table className="w-full text-[12px]">
                <thead className="text-left text-[11px] text-subtle">
                  <tr className="border-b border-line">
                    {picked.columns.slice(0, 8).map((c, i) => (
                      <th key={c} className="px-3 py-2 font-normal">
                        <span className="font-mono text-fg/80">{c}</span>
                        {picked.csv && <span className="ml-1.5 font-mono">{picked.csv.types[i]}</span>}
                      </th>
                    ))}
                    {picked.columns.length > 8 && <th className="px-3 py-2 font-normal">+{picked.columns.length - 8} more</th>}
                  </tr>
                </thead>
                <tbody>
                  {picked.preview.map((d, i) => (
                    <tr key={i} className="border-b border-line last:border-0">
                      {picked.columns.slice(0, 8).map((c) => (
                        <td key={c} className="max-w-56 truncate px-3 py-1.5 font-mono text-muted">
                          {cell(d[c])}
                        </td>
                      ))}
                      {picked.columns.length > 8 && <td />}
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
            <div className="grid items-end gap-3 sm:grid-cols-[minmax(0,1fr)_minmax(0,14rem)_auto]">
              <Field label="Index">
                <input value={name} onChange={(e) => setName(e.target.value)} disabled={running} className={`${input} w-full font-mono`} spellCheck={false} />
              </Field>
              <Field label="Document id">
                <select value={idField} onChange={(e) => setIdField(e.target.value)} disabled={running} className={`${input} w-full`}>
                  <option value="">Generated</option>
                  {picked.columns.map((c) => (
                    <option key={c} value={c}>
                      {c}
                    </option>
                  ))}
                </select>
              </Field>
              {running ? (
                <button onClick={() => (cancel.current = true)} className={btn}>
                  Cancel
                </button>
              ) : (
                !progress?.done && (
                  <button onClick={run} disabled={!!nameProblem} className={primary}>
                    Import
                  </button>
                )
              )}
            </div>
            {!progress?.done && (
              <p className={`text-[12px] ${nameProblem ? "text-danger" : "text-subtle"}`}>
                {nameProblem ?? (exists ? "The index exists: the documents are added to it." : "A new index, created with the first batch and its fields mapped from the values.")}
                {!nameProblem && idField && " A document whose id is already in the index replaces it."}
              </p>
            )}
          </>
        )}

        {progress && picked && (
          <div className="space-y-2" aria-live="polite">
            <div className="h-1 overflow-hidden rounded-full bg-panel-2">
              <div className="h-full rounded-full bg-accent transition-[width] duration-300 ease-out" style={{ width: `${picked.file.size ? (100 * progress.read) / picked.file.size : 100}%` }} />
            </div>
            <p className="font-mono text-[12px] text-muted">
              {formatCount(progress.sent - progress.failed)} indexed
              {progress.failed > 0 && <span className="text-danger"> · {formatCount(progress.failed)} failed</span>}
              {progress.unreadable > 0 && (
                <span className="text-danger">
                  {" "}
                  · {formatCount(progress.unreadable)} {picked.format === "json" ? "elements" : "lines"} not documents
                </span>
              )}
              {" · "}
              {formatBytes(progress.read)} of {formatBytes(picked.file.size)}
              {progress.done && (progress.cancelled ? " · stopped" : " · done")}
            </p>
            {progress.failures.map((f) => (
              <p key={f.doc} className="truncate font-mono text-[11px] text-danger" title={f.reason}>
                document {f.doc}: {f.reason}
              </p>
            ))}
            {progress.readErrors.map((r) => (
              <p key={r.at} className="truncate font-mono text-[11px] text-danger">
                {picked.format === "json" ? "element" : "line"} {r.at}: {r.message}
              </p>
            ))}
            {progress.done && (
              <button
                onClick={() => {
                  setPicked(null);
                  setProgress(null);
                }}
                className={btn}
              >
                Import another file
              </button>
            )}
          </div>
        )}
      </div>
    </Panel>
  );
}
