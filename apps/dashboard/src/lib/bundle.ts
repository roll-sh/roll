/**
 * A folder as `roll up` sends it (crates/roll-fetch: a gzipped tar, `roll-bundle-1` first, then
 * an empty `env`, then the tree under `tree/`, every directory before what's in it, a name past
 * 100 bytes as a GNU long name): Netlify Drop's "drop a folder, it's live", built in the browser.
 * What `roll up` leaves out is left out here too (`.git`, `node_modules`, build output), and so
 * are `.env` files (variables go in roll, never in a deploy).
 */

export interface BundleFile {
  /** Relative to the folder, `/`-separated. */
  path: string;
  data: Uint8Array;
}

/** A file roll-fetch takes at most (its `max_file_bytes`). */
export const MAX_FILE_BYTES = 100 << 20;

/** A script (`#!` first) keeps its exec bit: a browser's File has no mode, a shebang says it. */
const executable = (data: Uint8Array) => data[0] === 0x23 && data[1] === 0x21;

const SKIP = new Set([".git", ".roll", "node_modules", "target", ".venv", "__pycache__", ".next", ".turbo", ".DS_Store"]);
/** The most a dropped folder may hold (the API takes 512 MiB). */
export const MAX_BYTES = 256 << 20;

/** Whether `roll up` leaves this path out: a skipped directory anywhere in it, or a `.env` file. */
export function skipped(path: string): boolean {
  const parts = path.split("/");
  const name = parts[parts.length - 1] ?? "";
  return parts.some((p) => SKIP.has(p)) || name === ".env" || (name.startsWith(".env.") && name !== ".env.example");
}

const enc = new TextEncoder();

function header(name: string, size: number, type: "0" | "5" | "L", mode: number): Uint8Array {
  const h = new Uint8Array(512);
  const put = (s: string, at: number, len: number) => h.set(enc.encode(s).subarray(0, len), at);
  // A number field: octal, zero-padded, ended by a NUL.
  const octal = (n: number, len: number) => n.toString(8).padStart(len - 1, "0") + "\0";
  put(name, 0, 100);
  put(octal(mode, 8), 100, 8);
  put(octal(0, 8), 108, 8);
  put(octal(0, 8), 116, 8);
  put(octal(size, 12), 124, 12);
  put(octal(0, 12), 136, 12);
  put("        ", 148, 8);
  put(type, 156, 1);
  put("ustar  \0", 257, 8); // GNU's magic and version, as the long name entries are GNU's.
  let sum = 0;
  for (const b of h) sum += b;
  put(sum.toString(8).padStart(6, "0") + "\0 ", 148, 8);
  return h;
}

function pad(n: number): number {
  return (512 - (n % 512)) % 512;
}

/** One entry's blocks: a GNU long name first when the name doesn't fit in 100 bytes. */
function entry(name: string, data: Uint8Array, type: "0" | "5", mode: number): Uint8Array[] {
  const out: Uint8Array[] = [];
  const bytes = enc.encode(name);
  if (bytes.length > 100) {
    const long = new Uint8Array(bytes.length + 1);
    long.set(bytes);
    out.push(header("././@LongLink", long.length, "L", 0o644), long, new Uint8Array(pad(long.length)));
  }
  out.push(header(name, data.length, type, mode), data, new Uint8Array(pad(data.length)));
  return out;
}

/** The bundle's tar (not yet gzipped): the files given, their directories made first. */
export function tarBundle(files: BundleFile[]): Blob {
  const parts: Uint8Array[] = [...entry("roll-bundle-1", new Uint8Array(0), "0", 0o644), ...entry("env", new Uint8Array(0), "0", 0o600)];
  const dirs = new Set<string>();
  for (const f of [...files].sort((a, b) => a.path.localeCompare(b.path))) {
    const segs = f.path.split("/");
    for (let i = 1; i < segs.length; i++) {
      const d = segs.slice(0, i).join("/");
      if (!dirs.has(d)) {
        dirs.add(d);
        parts.push(...entry(`tree/${d}`, new Uint8Array(0), "5", 0o755));
      }
    }
    parts.push(...entry(`tree/${f.path}`, f.data, "0", executable(f.data) ? 0o755 : 0o644));
  }
  parts.push(new Uint8Array(1024));
  // The parts as they are: no second copy of everything in one buffer.
  return new Blob(parts as BlobPart[]);
}

/** gzip, with the browser's own `CompressionStream`. */
export async function gzip(tar: Blob): Promise<Blob> {
  const stream = tar.stream().pipeThrough(new CompressionStream("gzip"));
  return new Response(stream).blob();
}

/** Every file under what was dropped (folders walked), paths relative to the dropped folder. */
export async function filesFromDrop(items: DataTransferItemList): Promise<{ name: string; files: { path: string; file: File }[] }> {
  const entries = Array.from(items)
    .map((i) => i.webkitGetAsEntry?.())
    .filter((e): e is FileSystemEntry => !!e);
  const out: { path: string; file: File }[] = [];
  const walk = async (e: FileSystemEntry, prefix: string): Promise<void> => {
    if (e.isFile) {
      const file = await new Promise<File>((ok, no) => (e as FileSystemFileEntry).file(ok, no));
      out.push({ path: prefix + e.name, file });
    } else if (e.isDirectory) {
      const reader = (e as FileSystemDirectoryEntry).createReader();
      // A directory reads in batches until one comes back empty.
      for (;;) {
        const batch = await new Promise<FileSystemEntry[]>((ok, no) => reader.readEntries(ok, no));
        if (!batch.length) break;
        for (const c of batch) await walk(c, `${prefix}${e.name}/`);
      }
    }
  };
  for (const e of entries) await walk(e, "");
  // One folder dropped: its contents are the tree (and it names the service).
  if (entries.length === 1 && entries[0]!.isDirectory) {
    const root = `${entries[0]!.name}/`;
    return { name: entries[0]!.name, files: out.map((f) => ({ path: f.path.slice(root.length), file: f.file })) };
  }
  return { name: "site", files: out };
}

/** A dropped or chosen folder's files as a bundle, what's left out said. */
export async function bundleOf(files: { path: string; file: File }[]): Promise<{ body: Blob; files: number; left: number }> {
  const kept = files.filter((f) => !skipped(f.path));
  const bytes = kept.reduce((n, f) => n + f.file.size, 0);
  if (!kept.length) throw new Error("there's nothing to deploy in it");
  if (bytes > MAX_BYTES) throw new Error(`it's ${Math.round(bytes / 2 ** 20)} MiB: drop at most ${MAX_BYTES >> 20} MiB, or deploy with roll up`);
  const big = kept.find((f) => f.file.size > MAX_FILE_BYTES);
  if (big) throw new Error(`${big.path} is more than ${MAX_FILE_BYTES >> 20} MiB: a file that big goes another way (an image, a bucket)`);
  const data = await Promise.all(kept.map(async (f) => ({ path: f.path, data: new Uint8Array(await f.file.arrayBuffer()) })));
  return { body: await gzip(tarBundle(data)), files: kept.length, left: files.length - kept.length };
}
