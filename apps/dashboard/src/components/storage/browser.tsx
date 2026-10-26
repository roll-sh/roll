"use client";

import {
  ChevronRight,
  Download,
  File,
  Folder,
  FolderPlus,
  History,
  Info,
  Link2,
  RefreshCw,
  Trash2,
  Upload,
  X,
} from "lucide-react";
import { useCallback, useEffect, useRef, useState, type DragEvent } from "react";
import { formatBytes } from "@/lib/roll-api";
import {
  agoMs,
  baseName,
  formatDate,
  storageApi,
  uploadObject,
  type Bucket,
  type ObjectInfo,
  type ObjectSummary,
  type ObjectVersion,
} from "@/lib/storage-api";
import { sessionDownload, storageHeaders } from "@/lib/storage-session";
import { Drawer, iconBtn, LoadMore, Section, Spinner } from "../tasks/ui";
import { btn, danger, ErrorNote, input, panel, primary, useCopy } from "../ui";
import { Progress, Segmented } from "./shared";

const PAGE = 200;
const UPLOAD_CONCURRENCY = 3;

interface Upload {
  id: number;
  key: string;
  file: File;
  loaded: number;
  total: number;
  state: "queued" | "uploading" | "done" | "error";
  error?: string;
}

/** A file and the path it was dropped under (folder drops keep their structure). */
type Picked = { file: File; path: string };

/** Walk a dropped folder (webkitGetAsEntry) into files with relative paths. */
async function walk(entry: FileSystemEntry, dir: string, out: Picked[]) {
  if (entry.isFile) {
    const file = await new Promise<File>((res, rej) => (entry as FileSystemFileEntry).file(res, rej));
    out.push({ file, path: `${dir}${entry.name}` });
  } else if (entry.isDirectory) {
    const reader = (entry as FileSystemDirectoryEntry).createReader();
    // readEntries returns batches until an empty one.
    for (;;) {
      const batch = await new Promise<FileSystemEntry[]>((res, rej) => reader.readEntries(res, rej));
      if (batch.length === 0) break;
      for (const e of batch) await walk(e, `${dir}${entry.name}/`, out);
    }
  }
}

async function droppedFiles(e: DragEvent): Promise<Picked[]> {
  const items = [...e.dataTransfer.items];
  const entries = items.map((i) => i.webkitGetAsEntry?.()).filter((x): x is FileSystemEntry => !!x);
  if (entries.length === 0) return [...e.dataTransfer.files].map((file) => ({ file, path: file.name }));
  const out: Picked[] = [];
  for (const entry of entries) await walk(entry, "", out);
  return out;
}

/** The object browser: prefixes as folders, cursor pagination, uploads, selection, details. */
export function ObjectBrowser({ bucket, onChanged }: { bucket: Bucket; onChanged: () => void }) {
  const [prefix, setPrefixRaw] = useState("");
  const [filter, setFilter] = useState("");
  const [objects, setObjects] = useState<ObjectSummary[] | null>(null);
  const [prefixes, setPrefixes] = useState<string[]>([]);
  const [cursor, setCursor] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [detail, setDetail] = useState<string | null>(null);
  const [dragging, setDragging] = useState(false);
  const [uploads, setUploads] = useState<Upload[]>([]);
  const [deleting, setDeleting] = useState<{ keys?: string[]; prefix?: string } | null>(null);
  const [busy, setBusy] = useState(false);
  const [newFolder, setNewFolder] = useState<string | null>(null);
  // Versions mode: every version and delete marker under the prefix, instead of the listing.
  const [showVersions, setShowVersions] = useState(false);
  const [versionsTick, setVersionsTick] = useState(0);
  const fileInput = useRef<HTMLInputElement>(null);
  const nextId = useRef(0);
  const { copied, copy } = useCopy();

  const setPrefix = (p: string) => {
    setPrefixRaw(p);
    setFilter("");
    setSelected(new Set());
    const url = new URL(window.location.href);
    if (p) url.searchParams.set("prefix", p);
    else url.searchParams.delete("prefix");
    window.history.replaceState(null, "", url);
  };

  useEffect(() => {
    const p = new URL(window.location.href).searchParams.get("prefix");
    if (p) setPrefixRaw(p);
  }, []);

  const query = prefix + filter;
  const load = useCallback(
    async (more: string | null) => {
      setLoading(true);
      try {
        const r = await storageApi.objects(bucket.name, { prefix: query, delimiter: "/", cursor: more, limit: PAGE });
        setObjects((cur) => (more && cur ? [...cur, ...r.objects] : r.objects));
        setPrefixes((cur) => (more ? [...cur, ...r.prefixes] : r.prefixes));
        setCursor(r.next_cursor);
        setError(null);
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
      } finally {
        setLoading(false);
      }
    },
    [bucket.name, query],
  );

  useEffect(() => {
    const t = setTimeout(() => load(null), filter ? 200 : 0);
    return () => clearTimeout(t);
  }, [load, filter]);

  const reload = useCallback(() => {
    setSelected(new Set());
    load(null);
    onChanged();
  }, [load, onChanged]);

  // Upload queue: a few at a time, reload once the batch is done.
  const enqueue = (picked: Picked[]) => {
    if (picked.length === 0) return;
    setUploads((u) => [
      ...u.filter((x) => x.state !== "done"),
      ...picked.map(({ file, path }) => ({
        id: nextId.current++,
        key: prefix + path,
        file,
        loaded: 0,
        total: file.size,
        state: "queued" as const,
      })),
    ]);
  };

  useEffect(() => {
    const active = uploads.filter((u) => u.state === "uploading").length;
    const queued = uploads.filter((u) => u.state === "queued");
    if (active >= UPLOAD_CONCURRENCY || queued.length === 0) return;
    for (const up of queued.slice(0, UPLOAD_CONCURRENCY - active)) {
      setUploads((u) => u.map((x) => (x.id === up.id ? { ...x, state: "uploading" } : x)));
      uploadObject(bucket.name, up.key, up.file, (loaded, total) =>
        setUploads((u) => u.map((x) => (x.id === up.id ? { ...x, loaded, total } : x))),
      ).then(
        () => setUploads((u) => u.map((x) => (x.id === up.id ? { ...x, state: "done", loaded: x.total } : x))),
        (e) =>
          setUploads((u) =>
            u.map((x) => (x.id === up.id ? { ...x, state: "error", error: e instanceof Error ? e.message : String(e) } : x)),
          ),
      );
    }
  }, [uploads, bucket.name]);

  const pending = uploads.some((u) => u.state === "queued" || u.state === "uploading");
  const wasPending = useRef(false);
  useEffect(() => {
    if (wasPending.current && !pending) reload();
    wasPending.current = pending;
  }, [pending, reload]);

  const remove = async () => {
    if (!deleting) return;
    setBusy(true);
    try {
      if (deleting.keys?.length === 1 && !deleting.prefix) await storageApi.deleteObject(bucket.name, deleting.keys[0]);
      else await storageApi.deleteMany(bucket.name, deleting);
      setDeleting(null);
      if (detail && deleting.keys?.includes(detail)) setDetail(null);
      reload();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const copyUrl = async (key: string) => {
    try {
      const r = await storageApi.presign(bucket.name, key, "GET", 3600);
      copy(`url-${key}`, r.url);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  const crumbs = prefix.split("/").filter(Boolean);
  const allKeys = objects?.map((o) => o.key) ?? [];
  const allSelected = allKeys.length > 0 && allKeys.every((k) => selected.has(k));
  const empty = objects !== null && objects.length === 0 && prefixes.length === 0;

  return (
    <div className="space-y-4">
      <ErrorNote error={error} />

      <section
        className={`${panel} relative`}
        onDragOver={(e) => {
          if (!e.dataTransfer.types.includes("Files")) return;
          e.preventDefault();
          setDragging(true);
        }}
        onDragLeave={(e) => {
          if (e.currentTarget.contains(e.relatedTarget as Node)) return;
          setDragging(false);
        }}
        onDrop={async (e) => {
          e.preventDefault();
          setDragging(false);
          enqueue(await droppedFiles(e));
        }}
      >
        {/* Toolbar: breadcrumbs, filter, actions. */}
        <div className="flex flex-wrap items-center gap-2 border-b border-line px-4 py-3">
          <nav className="flex min-w-0 flex-1 flex-wrap items-center gap-0.5 font-mono text-[12px]" aria-label="Prefix">
            <button onClick={() => setPrefix("")} className={`rounded px-1.5 py-0.5 hover:bg-panel-2 ${prefix ? "text-muted" : "text-fg"}`}>
              {bucket.name}
            </button>
            {crumbs.map((c, i) => {
              const p = `${crumbs.slice(0, i + 1).join("/")}/`;
              return (
                <span key={p} className="flex items-center gap-0.5">
                  <ChevronRight className="size-3 text-subtle" />
                  <button
                    onClick={() => setPrefix(p)}
                    className={`rounded px-1.5 py-0.5 hover:bg-panel-2 ${i === crumbs.length - 1 ? "text-fg" : "text-muted"}`}
                  >
                    {c}
                  </button>
                </span>
              );
            })}
          </nav>
          <input
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
            placeholder="Filter by prefix…"
            spellCheck={false}
            className={`${input} w-44 py-1 font-mono text-[12px]`}
          />
          <button
            onClick={() => (showVersions ? setVersionsTick((t) => t + 1) : load(null))}
            className={iconBtn}
            title="Refresh"
            aria-label="Refresh"
          >
            {loading && !showVersions ? <Spinner /> : <RefreshCw className="size-3.5" />}
          </button>
          {bucket.versioning !== null && (
            <button
              onClick={() => {
                setShowVersions((v) => !v);
                setSelected(new Set());
              }}
              aria-pressed={showVersions}
              title="Show every version and delete marker"
              className={`${btn} ${showVersions ? "border-line-2 bg-panel-2 text-fg" : ""}`}
            >
              <History className="size-3.5" strokeWidth={1.5} /> Versions
            </button>
          )}
          <button onClick={() => setNewFolder("")} className={btn}>
            <FolderPlus className="size-3.5" /> Folder
          </button>
          <button onClick={() => fileInput.current?.click()} className={primary}>
            <Upload className="size-3.5" /> Upload
          </button>
          <input
            ref={fileInput}
            type="file"
            multiple
            hidden
            onChange={(e) => {
              enqueue([...(e.target.files ?? [])].map((file) => ({ file, path: file.name })));
              e.target.value = "";
            }}
          />
        </div>

        {newFolder !== null && (
          <form
            className="rise flex flex-wrap items-center gap-2 border-b border-line px-4 py-3"
            onSubmit={(e) => {
              e.preventDefault();
              const name = newFolder.replace(/^\/+|\/+$/g, "");
              if (!name) return;
              setNewFolder(null);
              setPrefix(`${prefix}${name}/`);
            }}
          >
            <span className="font-mono text-[12px] text-subtle">{prefix}</span>
            <input
              autoFocus
              value={newFolder}
              onChange={(e) => setNewFolder(e.target.value)}
              placeholder="folder-name"
              spellCheck={false}
              className={`${input} min-w-0 flex-1 py-1 font-mono text-[12px]`}
            />
            <button type="submit" disabled={!newFolder.trim()} className={btn}>
              Open
            </button>
            <button type="button" onClick={() => setNewFolder(null)} className={iconBtn} aria-label="Cancel">
              <X className="size-3.5" />
            </button>
            <p className="w-full text-[11px] text-subtle">
              S3 has no real folders: a folder appears once an object with that prefix exists. Upload into it to keep it.
            </p>
          </form>
        )}

        {/* Selection bar. */}
        {!showVersions && selected.size > 0 && (
          <div className="rise flex items-center gap-3 border-b border-line bg-panel-2/50 px-4 py-2 text-[12px]">
            <span className="font-mono">{selected.size} selected</span>
            <button onClick={() => setSelected(new Set())} className="text-subtle hover:text-fg">
              Clear
            </button>
            <button
              onClick={() => setDeleting({ keys: [...selected] })}
              className="ml-auto inline-flex items-center gap-1.5 text-subtle hover:text-danger"
            >
              <Trash2 className="size-3.5" /> Delete
            </button>
          </div>
        )}

        {deleting && (
          <div className="rise flex flex-wrap items-center gap-3 border-b border-danger/40 bg-danger/5 px-4 py-3 text-[13px]">
            <span className="min-w-0 flex-1 text-muted">
              {deleting.prefix ? (
                <>
                  Delete everything under <span className="font-mono text-fg">{deleting.prefix}</span>?
                </>
              ) : deleting.keys?.length === 1 ? (
                <>
                  Delete <span className="font-mono text-fg">{deleting.keys[0]}</span>?
                </>
              ) : (
                <>Delete {deleting.keys?.length} objects?</>
              )}{" "}
              {bucket.versioning === "Enabled"
                ? "A delete marker hides it; older versions are kept under Versions."
                : bucket.versioning === "Suspended"
                  ? "A delete marker hides it; its null version moves to Recently deleted and other versions are kept."
                  : bucket.recover_days > 0
                    ? `Recoverable from Recently deleted for ${bucket.recover_days} day${bucket.recover_days === 1 ? "" : "s"}.`
                    : "Recovery is off for this bucket — this is final."}
            </span>
            <button onClick={remove} disabled={busy} className={danger}>
              {busy ? <span className="shimmer">Deleting…</span> : "Delete"}
            </button>
            <button onClick={() => setDeleting(null)} className={btn}>
              Cancel
            </button>
          </div>
        )}

        {/* Listing. */}
        {showVersions ? (
          <VersionsList key={`${query}:${versionsTick}`} bucket={bucket} prefix={query} onChanged={onChanged} />
        ) : objects === null ? (
          <ListSkeleton />
        ) : empty ? (
          <div className="flex flex-col items-center gap-3 px-5 py-14 text-center">
            <Upload className="size-5 text-subtle" />
            <p className="text-[13px] text-muted">
              {filter ? (
                <>
                  Nothing matches <span className="font-mono text-fg">{query}</span>.
                </>
              ) : prefix ? (
                "This folder is empty."
              ) : (
                "This bucket is empty."
              )}
            </p>
            {!filter && <p className="text-[12px] text-subtle">Drop files or folders here, or use Upload.</p>}
          </div>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full text-[13px]">
              <thead>
                <tr className="border-b border-line text-left text-[11px] text-subtle">
                  <th className="w-10 py-2 pl-4">
                    <input
                      type="checkbox"
                      aria-label="Select all objects"
                      checked={allSelected}
                      onChange={(e) => setSelected(e.target.checked ? new Set(allKeys) : new Set())}
                      className="accent-accent"
                    />
                  </th>
                  <th className="py-2 pr-3 font-normal">Name</th>
                  <th className="hidden py-2 pr-3 text-right font-normal sm:table-cell">Size</th>
                  <th className="hidden py-2 pr-3 font-normal md:table-cell">Type</th>
                  <th className="hidden py-2 pr-3 font-normal md:table-cell">Modified</th>
                  <th className="w-32 py-2 pr-4" />
                </tr>
              </thead>
              <tbody>
                {prefixes.map((p) => (
                  <tr key={p} className="group border-b border-line last:border-0 hover:bg-panel-2/60">
                    <td className="py-2 pl-4" />
                    <td className="py-2 pr-3">
                      <button onClick={() => setPrefix(p)} className="flex min-w-0 items-center gap-2 text-left">
                        <Folder className="size-4 shrink-0 text-subtle" />
                        <span className="truncate font-mono text-[12px]">{baseName(p, prefix)}</span>
                      </button>
                    </td>
                    <td className="hidden py-2 pr-3 text-right font-mono text-[12px] text-subtle sm:table-cell">—</td>
                    <td className="hidden py-2 pr-3 text-[12px] text-subtle md:table-cell">folder</td>
                    <td className="hidden py-2 pr-3 md:table-cell" />
                    <td className="py-2 pr-4 text-right">
                      <button
                        onClick={() => setDeleting({ prefix: p })}
                        className={`${iconBtn} opacity-0 group-hover:opacity-100 focus-visible:opacity-100 hover:text-danger`}
                        title="Delete folder"
                        aria-label={`Delete everything under ${p}`}
                      >
                        <Trash2 className="size-3.5" />
                      </button>
                    </td>
                  </tr>
                ))}
                {objects.map((o) => (
                  <tr
                    key={o.key}
                    className={`group border-b border-line last:border-0 hover:bg-panel-2/60 ${selected.has(o.key) ? "bg-panel-2/40" : ""}`}
                  >
                    <td className="py-2 pl-4">
                      <input
                        type="checkbox"
                        aria-label={`Select ${o.key}`}
                        checked={selected.has(o.key)}
                        onChange={(e) =>
                          setSelected((s) => {
                            const n = new Set(s);
                            if (e.target.checked) n.add(o.key);
                            else n.delete(o.key);
                            return n;
                          })
                        }
                        className="accent-accent"
                      />
                    </td>
                    <td className="max-w-0 py-2 pr-3">
                      <button onClick={() => setDetail(o.key)} className="flex w-full min-w-0 items-center gap-2 text-left">
                        <File className="size-4 shrink-0 text-subtle" />
                        <span className="truncate font-mono text-[12px]">{baseName(o.key, prefix) || o.key}</span>
                      </button>
                    </td>
                    <td className="hidden py-2 pr-3 text-right font-mono text-[12px] tabular-nums sm:table-cell">
                      {formatBytes(o.size)}
                    </td>
                    <td className="hidden max-w-40 truncate py-2 pr-3 font-mono text-[11px] text-subtle md:table-cell">
                      {o.content_type ?? "—"}
                    </td>
                    <td className="hidden py-2 pr-3 text-[12px] whitespace-nowrap text-subtle md:table-cell" title={formatDate(o.last_modified)}>
                      {agoMs(o.last_modified)}
                    </td>
                    <td className="py-2 pr-4">
                      <div className="flex justify-end gap-0.5 opacity-60 transition group-hover:opacity-100">
                        <a
                          href={storageApi.downloadUrl(bucket.name, o.key)}
                          onClick={sessionDownload(storageApi.downloadUrl(bucket.name, o.key), o.key, setError)}
                          className={iconBtn} title="Download" aria-label={`Download ${o.key}`}>
                          <Download className="size-3.5" />
                        </a>
                        <button onClick={() => copyUrl(o.key)} className={iconBtn} title="Copy presigned URL (1 hour)" aria-label={`Copy URL for ${o.key}`}>
                          {copied === `url-${o.key}` ? <span className="font-mono text-[10px] text-ok">copied</span> : <Link2 className="size-3.5" />}
                        </button>
                        <button onClick={() => setDetail(o.key)} className={iconBtn} title="Details" aria-label={`Details of ${o.key}`}>
                          <Info className="size-3.5" />
                        </button>
                        <button
                          onClick={() => setDeleting({ keys: [o.key] })}
                          className={`${iconBtn} hover:text-danger`}
                          title="Delete"
                          aria-label={`Delete ${o.key}`}
                        >
                          <Trash2 className="size-3.5" />
                        </button>
                      </div>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
        {!showVersions && <LoadMore cursor={cursor} onMore={() => load(cursor)} loading={loading} />}

        {dragging && (
          <div className="pointer-events-none absolute inset-0 z-10 grid place-items-center rounded-xl border border-dashed border-accent bg-bg/80 backdrop-blur-sm">
            <p className="flex items-center gap-2 text-[13px]">
              <Upload className="size-4 text-accent" /> Drop to upload to{" "}
              <span className="font-mono">
                {bucket.name}/{prefix}
              </span>
            </p>
          </div>
        )}
      </section>

      {uploads.length > 0 && <Uploads uploads={uploads} onClear={() => setUploads((u) => u.filter((x) => x.state === "queued" || x.state === "uploading"))} />}

      {detail && (
        <ObjectDrawer
          bucket={bucket}
          objectKey={detail}
          onClose={() => setDetail(null)}
          onDelete={() => setDeleting({ keys: [detail] })}
        />
      )}
    </div>
  );
}

/**
 * Every version and delete marker under `prefix`, newest first within a key, with download and
 * permanent delete per version.
 */
function VersionsList({ bucket, prefix, onChanged }: { bucket: Bucket; prefix: string; onChanged: () => void }) {
  const [versions, setVersions] = useState<ObjectVersion[] | null>(null);
  const [cursor, setCursor] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [confirm, setConfirm] = useState<ObjectVersion | null>(null);
  const [busy, setBusy] = useState(false);

  const load = useCallback(
    async (more: string | null) => {
      setLoading(true);
      try {
        const r = await storageApi.versions(bucket.name, { prefix, cursor: more, limit: PAGE });
        setVersions((cur) => (more && cur ? [...cur, ...r.versions] : r.versions));
        setCursor(r.next_cursor);
        setError(null);
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
      } finally {
        setLoading(false);
      }
    },
    [bucket.name, prefix],
  );

  useEffect(() => {
    const t = setTimeout(() => load(null), prefix ? 200 : 0);
    return () => clearTimeout(t);
  }, [load, prefix]);

  const remove = async () => {
    if (!confirm) return;
    setBusy(true);
    try {
      await storageApi.deleteObject(bucket.name, confirm.key, confirm.version_id);
      setConfirm(null);
      onChanged();
      await load(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const days = bucket.recover_days;
  return (
    <>
      {error && <p className="border-b border-line px-4 py-2 text-[12px] text-danger">{error}</p>}
      {confirm && (
        <div className="rise flex flex-wrap items-center gap-3 border-b border-danger/40 bg-danger/5 px-4 py-3 text-[13px]">
          <span className="min-w-0 flex-1 text-muted">
            {confirm.delete_marker ? (
              <>
                Remove this delete marker from <span className="font-mono text-fg">{confirm.key}</span>?{" "}
                {confirm.is_latest ? "The version before it becomes current again." : "Delete markers hold no data."}
              </>
            ) : (
              <>
                Delete version <span className="font-mono break-all text-fg">{confirm.version_id}</span> of{" "}
                <span className="font-mono text-fg">{confirm.key}</span> for good?{" "}
                {days > 0
                  ? `It moves to Recently deleted for ${days} day${days === 1 ? "" : "s"}, where it can be restored.`
                  : "Recovery is off for this bucket — this is final."}
                {confirm.is_latest && " The version before it becomes current."}
              </>
            )}
          </span>
          <button onClick={remove} disabled={busy} className={danger}>
            {busy ? <span className="shimmer">Deleting…</span> : confirm.delete_marker ? "Remove marker" : "Delete for good"}
          </button>
          <button onClick={() => setConfirm(null)} className={btn}>
            Cancel
          </button>
        </div>
      )}
      {versions === null ? (
        <ListSkeleton />
      ) : versions.length === 0 ? (
        <p className="px-5 py-14 text-center text-[13px] text-muted">
          {prefix ? (
            <>
              No versions under <span className="font-mono text-fg">{prefix}</span>.
            </>
          ) : (
            "No versions yet."
          )}
        </p>
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full text-[13px]">
            <thead>
              <tr className="border-b border-line text-left text-[11px] text-subtle">
                <th className="py-2 pr-3 pl-4 font-normal">Key</th>
                <th className="w-[22rem] py-2 pr-3 font-normal">Version</th>
                <th className="hidden py-2 pr-3 text-right font-normal sm:table-cell">Size</th>
                <th className="hidden py-2 pr-3 font-normal md:table-cell">Modified</th>
                <th className="w-24 py-2 pr-4" />
              </tr>
            </thead>
            <tbody>
              {versions.map((v, i) => {
                const first = i === 0 || versions[i - 1].key !== v.key;
                return (
                  <tr
                    key={`${v.key}\u0000${v.version_id}`}
                    className={`group hover:bg-panel-2/60 ${first && i > 0 ? "border-t border-line" : ""}`}
                  >
                    <td className="max-w-0 py-2 pr-3 pl-4">
                      {first && (
                        <span className="flex min-w-0 items-center gap-2">
                          <File className="size-4 shrink-0 text-subtle" />
                          <span className="truncate font-mono text-[12px]" title={v.key}>
                            {baseName(v.key, prefix.endsWith("/") ? prefix : prefix.slice(0, prefix.lastIndexOf("/") + 1)) || v.key}
                          </span>
                        </span>
                      )}
                    </td>
                    <td className="py-2 pr-3 whitespace-nowrap">
                      <span className="font-mono text-[11px] text-muted" title={v.version_id}>
                        {v.version_id}
                      </span>
                      {v.is_latest && (
                        <span className="ml-2 rounded-md border border-line px-1.5 py-px text-[10px] text-muted">latest</span>
                      )}
                      {v.delete_marker && <span className="ml-2 text-[11px] text-subtle">delete marker</span>}
                    </td>
                    <td className="hidden py-2 pr-3 text-right font-mono text-[12px] tabular-nums sm:table-cell">
                      {v.delete_marker ? <span className="text-subtle">—</span> : formatBytes(v.size)}
                    </td>
                    <td className="hidden py-2 pr-3 text-[12px] whitespace-nowrap text-subtle md:table-cell" title={formatDate(v.last_modified)}>
                      {agoMs(v.last_modified)}
                    </td>
                    <td className="py-2 pr-4">
                      <div className="flex justify-end gap-0.5 opacity-60 transition group-hover:opacity-100">
                        {!v.delete_marker && (
                          <a
                            href={storageApi.downloadUrl(bucket.name, v.key, v.version_id)}
                            onClick={sessionDownload(storageApi.downloadUrl(bucket.name, v.key, v.version_id), v.key, setError)}
                            className={iconBtn}
                            title="Download this version"
                            aria-label={`Download version ${v.version_id} of ${v.key}`}
                          >
                            <Download className="size-3.5" />
                          </a>
                        )}
                        <button
                          onClick={() => setConfirm(v)}
                          className={`${iconBtn} hover:text-danger`}
                          title={v.delete_marker ? "Remove delete marker" : "Delete this version for good"}
                          aria-label={`Delete version ${v.version_id} of ${v.key}`}
                        >
                          <Trash2 className="size-3.5" />
                        </button>
                      </div>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
      <LoadMore cursor={cursor} onMore={() => load(cursor)} loading={loading} />
    </>
  );
}

function ListSkeleton() {
  return (
    <ul aria-hidden className="divide-y divide-line">
      {Array.from({ length: 5 }, (_, i) => (
        <li key={i} className="flex items-center gap-3 px-4 py-3">
          <span className="size-4 rounded bg-panel-2" />
          <span className="h-3 rounded bg-panel-2" style={{ width: `${30 + ((i * 17) % 40)}%` }} />
          <span className="ml-auto h-3 w-12 rounded bg-panel-2" />
        </li>
      ))}
    </ul>
  );
}

function Uploads({ uploads, onClear }: { uploads: Upload[]; onClear: () => void }) {
  const done = uploads.filter((u) => u.state === "done").length;
  const failed = uploads.filter((u) => u.state === "error").length;
  const total = uploads.reduce((s, u) => s + u.total, 0);
  const loaded = uploads.reduce((s, u) => s + (u.state === "done" ? u.total : u.loaded), 0);
  const running = uploads.some((u) => u.state === "queued" || u.state === "uploading");
  return (
    <section className={`${panel} rise`}>
      <div className="flex items-center gap-3 border-b border-line px-4 py-3">
        <p className="text-[13px] font-medium">
          {running ? <span className="shimmer">Uploading</span> : "Uploads"}{" "}
          <span className="font-mono text-[12px] font-normal text-subtle">
            {done}/{uploads.length}
            {failed > 0 && <span className="text-danger"> · {failed} failed</span>}
          </span>
        </p>
        <span className="ml-auto font-mono text-[11px] text-subtle">
          {formatBytes(loaded)} / {formatBytes(total)}
        </span>
        {!running && (
          <button onClick={onClear} className={iconBtn} aria-label="Clear finished uploads">
            <X className="size-3.5" />
          </button>
        )}
      </div>
      <ul className="max-h-56 divide-y divide-line overflow-y-auto">
        {uploads.map((u) => (
          <li key={u.id} className="space-y-1.5 px-4 py-2">
            <div className="flex items-center gap-3 text-[12px]">
              <span className="min-w-0 flex-1 truncate font-mono">{u.key}</span>
              <span className={`shrink-0 font-mono text-[11px] ${u.state === "error" ? "text-danger" : "text-subtle"}`}>
                {u.state === "error" ? u.error : u.state === "done" ? formatBytes(u.total) : u.state === "queued" ? "queued" : `${Math.floor((u.loaded / Math.max(1, u.total)) * 100)}%`}
              </span>
            </div>
            {u.state === "uploading" && <Progress value={u.loaded / Math.max(1, u.total)} />}
          </li>
        ))}
      </ul>
    </section>
  );
}

const EXPIRIES = [
  { label: "1h", value: 3600 },
  { label: "24h", value: 86_400 },
  { label: "7d", value: 604_800 },
];

/** Images roll can render (and so preview). */
const PREVIEWABLE = /^image\/(jpeg|jpg|png|webp|gif)\b/i;
const PREVIEW_WIDTH = 200;

/**
 * A small rendition of an image (200 px wide, WebP when the browser takes it), made by the image
 * renderer and cached as a derived image: the original is never downloaded to preview it.
 */
function ImagePreview({ bucket, objectKey, versionId }: { bucket: string; objectKey: string; versionId?: string | null }) {
  const [src, setSrc] = useState<string | null>(null);
  const [failed, setFailed] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    let href: string | null = null;
    setSrc(null);
    setFailed(null);
    (async () => {
      const res = await fetch(storageApi.thumbnailUrl(bucket, objectKey, PREVIEW_WIDTH, versionId), { headers: storageHeaders() });
      if (!res.ok) {
        let msg = `${res.status} ${res.statusText}`.trim();
        try {
          msg = ((await res.json()) as { error?: string }).error ?? msg;
        } catch {
          // keep the status
        }
        throw new Error(msg);
      }
      href = URL.createObjectURL(await res.blob());
      if (alive) setSrc(href);
    })().catch((e) => alive && setFailed(e instanceof Error ? e.message : String(e)));
    return () => {
      alive = false;
      if (href) URL.revokeObjectURL(href);
    };
  }, [bucket, objectKey, versionId]);

  return (
    <div className="space-y-2">
      <div className="flex h-[200px] items-center justify-center overflow-hidden rounded-lg border border-line bg-bg">
        {src ? (
          // A blob: URL of a rendition; next/image has nothing to optimize here.
          // eslint-disable-next-line @next/next/no-img-element
          <img src={src} alt={`Preview of ${objectKey}`} className="rise max-h-full max-w-full object-contain" />
        ) : failed ? (
          <p className="px-4 text-center text-[12px] text-subtle">No preview: {failed}</p>
        ) : (
          <span className="shimmer text-[12px] text-subtle">Rendering preview…</span>
        )}
      </div>
      <p className="text-[11px] text-subtle">
        A {PREVIEW_WIDTH} px rendition, made once and cached. Previews aren&apos;t billed as transformations.
      </p>
    </div>
  );
}

function ObjectDrawer({
  bucket,
  objectKey,
  onClose,
  onDelete,
}: {
  bucket: Bucket;
  objectKey: string;
  onClose: () => void;
  onDelete: () => void;
}) {
  const [info, setInfo] = useState<ObjectInfo | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [expires, setExpires] = useState(3600);
  const [link, setLink] = useState<{ url: string; expires_at: number } | null>(null);
  const { copied, copy } = useCopy();

  useEffect(() => {
    let alive = true;
    setInfo(null);
    setLink(null);
    storageApi.object(bucket.name, objectKey).then(
      (i) => alive && setInfo(i),
      (e) => alive && setError(e instanceof Error ? e.message : String(e)),
    );
    return () => {
      alive = false;
    };
  }, [bucket.name, objectKey]);

  const presign = async () => {
    try {
      const r = await storageApi.presign(bucket.name, objectKey, "GET", expires);
      setLink(r);
      copy("presigned", r.url);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  const encodedKey = objectKey.split("/").map(encodeURIComponent).join("/");
  const publicUrl = bucket.public ? `${bucket.url.replace(/\/$/, "")}/${encodedKey}` : null;
  const isImage = !!info && PREVIEWABLE.test(info.content_type ?? "");
  // The developer API's resized rendition of a public image (through the CDN when it's on).
  const imageUrl =
    isImage && bucket.public && bucket.api_public_url
      ? `${bucket.api_public_url.replace("/object/public/", "/render/image/public/")}/${encodedKey}?width=${PREVIEW_WIDTH}`
      : null;
  const rows: [string, string | number | null | undefined][] = info
    ? [
        ["Size", `${formatBytes(info.size)} (${info.size.toLocaleString("en-US")} bytes)`],
        ["Last modified", formatDate(info.last_modified)],
        ["Content-Type", info.content_type],
        ["ETag", info.etag],
        ["Parts", info.parts > 0 ? `${info.parts} (multipart)` : "single upload"],
        ["Version", bucket.versioning !== null || info.version_id !== "null" ? info.version_id : null],
        [
          "Object Lock",
          info.lock
            ? [
                info.lock.mode && info.lock.retain_until != null ? `${info.lock.mode.toLowerCase()} until ${formatDate(info.lock.retain_until)}` : null,
                info.lock.legal_hold ? "legal hold" : null,
              ]
                .filter(Boolean)
                .join(" · ") || null
            : null,
        ],
        ["Cache-Control", info.cache_control],
        ["Content-Disposition", info.content_disposition],
        ["Content-Encoding", info.content_encoding],
        ["Content-Language", info.content_language],
        ["Expires", info.expires],
      ]
    : [];

  return (
    <Drawer
      title={<span className="font-mono text-[13px]">{objectKey}</span>}
      onClose={onClose}
      actions={
        <a
          href={storageApi.downloadUrl(bucket.name, objectKey)}
          onClick={sessionDownload(storageApi.downloadUrl(bucket.name, objectKey), objectKey, setError)}
          className={btn}
        >
          <Download className="size-3.5" /> Download
        </a>
      }
    >
      {error && <p className="px-5 py-4 text-[13px] text-danger">{error}</p>}
      {!info && !error && (
        <p className="px-5 py-6 text-[13px] text-subtle">
          <span className="shimmer">Loading object…</span>
        </p>
      )}
      {info && (
        <>
          {isImage && (
            <Section title="Preview">
              <ImagePreview bucket={bucket.name} objectKey={objectKey} versionId={info.version_id !== "null" ? info.version_id : null} />
            </Section>
          )}
          <Section title="Object">
            <dl className="space-y-1.5 text-[12px]">
              {rows
                .filter(([, v]) => v != null && v !== "")
                .map(([k, v]) => (
                  <div key={k} className="grid grid-cols-[9rem_minmax(0,1fr)] gap-3">
                    <dt className="text-subtle">{k}</dt>
                    <dd className="font-mono break-all">{v}</dd>
                  </div>
                ))}
            </dl>
          </Section>
          <Section title="Checksum">
            {info.checksum ? (
              <p className="font-mono text-[12px] break-all">
                <span className="text-subtle">
                  {info.checksum.algorithm} · {info.checksum.type.toLowerCase().replace("_", " ")}
                </span>{" "}
                {info.checksum.value}
              </p>
            ) : (
              <p className="text-[12px] text-subtle">No additional checksum was sent with the upload (MD5 is in the ETag).</p>
            )}
          </Section>
          <Section title="Metadata">
            <KeyValues map={info.metadata} empty="No user metadata (x-amz-meta-*)." prefix="x-amz-meta-" />
          </Section>
          <Section title="Tags">
            <KeyValues map={info.tags} empty="No tags." />
          </Section>
          <Section title="Share">
            <div className="space-y-3">
              {publicUrl && (
                <div className="space-y-1">
                  <p className="text-[11px] text-subtle">Public URL — the bucket is public</p>
                  <div className="flex items-center gap-1 rounded-lg border border-line bg-bg px-2.5 py-2">
                    <code className="min-w-0 flex-1 truncate font-mono text-[11px]">{publicUrl}</code>
                    <button onClick={() => copy("public", publicUrl)} className={iconBtn} aria-label="Copy public URL">
                      {copied === "public" ? <span className="font-mono text-[10px] text-ok">copied</span> : <Link2 className="size-3.5" />}
                    </button>
                  </div>
                </div>
              )}
              {imageUrl && (
                <div className="space-y-1">
                  <p className="text-[11px] text-subtle">
                    Resized image — {PREVIEW_WIDTH} px wide{bucket.api_cdn ? ", served by the CDN" : ""}; change width, height, quality or format
                  </p>
                  <div className="flex items-center gap-1 rounded-lg border border-line bg-bg px-2.5 py-2">
                    <code className="min-w-0 flex-1 truncate font-mono text-[11px]">{imageUrl}</code>
                    <button onClick={() => copy("image", imageUrl)} className={iconBtn} aria-label="Copy resized image URL">
                      {copied === "image" ? <span className="font-mono text-[10px] text-ok">copied</span> : <Link2 className="size-3.5" />}
                    </button>
                  </div>
                </div>
              )}
              <div className="flex flex-wrap items-center gap-2">
                <div className="w-40">
                  <Segmented value={expires} options={EXPIRIES} onChange={setExpires} />
                </div>
                <button onClick={presign} className={btn}>
                  <Link2 className="size-3.5" /> {copied === "presigned" ? "Copied" : "Copy presigned URL"}
                </button>
              </div>
              {link && (
                <p className="rise font-mono text-[11px] break-all text-subtle">
                  {link.url}
                  <span className="mt-1 block font-sans">expires {formatDate(link.expires_at)}</span>
                </p>
              )}
            </div>
          </Section>
          <Section title="Danger">
            <button onClick={onDelete} className={`${btn} hover:border-danger/50 hover:text-danger`}>
              <Trash2 className="size-3.5" /> Delete object
            </button>
          </Section>
        </>
      )}
    </Drawer>
  );
}

function KeyValues({ map, empty, prefix = "" }: { map: Record<string, string>; empty: string; prefix?: string }) {
  const entries = Object.entries(map ?? {});
  if (entries.length === 0) return <p className="text-[12px] text-subtle">{empty}</p>;
  return (
    <dl className="space-y-1.5 text-[12px]">
      {entries.map(([k, v]) => (
        <div key={k} className="grid grid-cols-[9rem_minmax(0,1fr)] gap-3">
          <dt className="truncate font-mono text-subtle" title={`${prefix}${k}`}>
            {k}
          </dt>
          <dd className="font-mono break-all">{v}</dd>
        </div>
      ))}
    </dl>
  );
}
