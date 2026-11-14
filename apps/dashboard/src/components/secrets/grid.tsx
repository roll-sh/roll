import { Folder, PenLine, Search } from "lucide-react";
import { Suggestions } from "./suggestions";
import { useEffect, useMemo, useRef, useState } from "react";
import { Link } from "@/components/link";
import { needsApproval, type Cell, type GridRow } from "@/lib/secrets-api";
import { misfit } from "@/lib/secrets-required";
import { countStatus, filterRows, folderName, type StatusFilter } from "@/lib/secrets-grid";
import { Segmented } from "../agents/ui/toggle-group";
import { FolderCrumbs } from "./folders";
import { secretsHref, useSecrets } from "./secrets-page";
import { ago, button, Chip, CommentMark, ErrorNote, errorText, ExpiryMark, field, ghost, LinkChip, PersonalMark, primary, RefMark, SourceMark, Teach, useLoad } from "./ui";

/**
 * The overview: every key in the folder as a row, the standard environments as columns in order.
 * A cell says whether the key is set there and how (linked, a reference, imported or inherited, a
 * personal override), never its value. Cells whose fingerprints match share a letter, so a row
 * that differs shows which environments agree.
 */
export function OverviewGrid() {
  const { api, project, search, go, standard, previews } = useSecrets();
  const path = search.path || "/";
  const grid = useLoad(() => api.grid(path), [api, path]);
  const [query, setQuery] = useState("");
  const [status, setStatus] = useState<StatusFilter>("all");
  const input = useRef<HTMLInputElement>(null);
  useSlashFocus(input);

  const rows = grid.data?.rows ?? [];
  const shown = useMemo(() => filterRows(rows, query, status), [rows, query, status]);
  const counts = countStatus(rows);
  // The grid's own columns are the standard environments in order; fall back to the overview's.
  const cols = grid.data?.environments ?? standard.map((e) => e.slug);
  const names = new Map(standard.map((e) => [e.slug, e.name]));

  return (
    <div className="flex min-h-full flex-col">
      <div className="sticky top-0 z-10 flex flex-wrap items-center gap-2 border-b border-line bg-canvas/90 px-3 py-2 backdrop-blur">
        <FolderCrumbs path={path} onPath={(p) => go({ path: p === "/" ? undefined : p })} />
        {grid.data?.folders.map((f) => (
          <button key={f} onClick={() => go({ path: f })} className="inline-flex h-6 items-center gap-1 rounded-md border border-line-2 px-1.5 text-[11.5px] text-muted transition-colors hover:border-line-3 hover:text-fg">
            <Folder className="size-3" />
            {folderName(f)}
          </button>
        ))}
        <span className="ml-auto" />
        <Segmented
          label="Show rows"
          value={status}
          onChange={setStatus}
          options={[
            { value: "all", label: <span className="px-1.5 text-[11.5px]">All {rows.length}</span> },
            { value: "missing", label: <span className="px-1.5 text-[11.5px]">Missing {counts.missing}</span>, title: "Set in some environments, not all" },
            { value: "different", label: <span className="px-1.5 text-[11.5px]">Differ {counts.different}</span>, title: "Set everywhere, with different values" },
            { value: "same", label: <span className="px-1.5 text-[11.5px]">Same {counts.same}</span> },
          ]}
        />
        <label className="flex h-7 w-52 items-center gap-1.5 rounded-md border border-line-2 bg-panel-2 px-2 text-subtle focus-within:border-accent/60 max-sm:w-full">
          <Search className="size-3.5" />
          <input ref={input} value={query} onChange={(e) => setQuery(e.target.value)} placeholder="Filter keys" aria-label="Filter keys" className="min-w-0 flex-1 bg-transparent font-mono text-[12px] text-fg outline-none placeholder:font-sans placeholder:text-subtle" />
          <kbd className="font-mono text-[10px]">/</kbd>
        </label>
      </div>

      <ErrorNote error={grid.error} className="m-3" />
      {path === "/" && <Suggestions onLinked={grid.reload} />}

      {grid.data && rows.length === 0 ? (
        <Teach
          title={path === "/" ? `No secrets in ${project.name} yet` : `Nothing in ${path} yet`}
          cli={[`roll secrets set -p ${project.name} -e development DATABASE_URL=…`, `roll secrets import -p ${project.name} -e development .env`, `roll run -p ${project.name} --env production -- <cmd>`]}
        >
          Open an environment to add a secret, paste a .env file, or link one of the project's resources so its credentials stay current. From the CLI:
        </Teach>
      ) : grid.data ? (
        <div className="overflow-x-auto px-3 pt-2 pb-6">
          <table className="w-full min-w-[640px] border-separate border-spacing-0 text-[12.5px]">
            <thead>
              <tr className="text-left text-[11px] font-medium text-subtle">
                <th className="sticky left-0 z-[1] w-[32%] bg-canvas py-1.5 pr-3 pl-2 font-medium">Key</th>
                {cols.map((c) => (
                  <th key={c} className="px-2 py-1.5 font-medium">
                    <Link href={secretsHref(project.id, { env: c, path: path === "/" ? undefined : path })} className="hover:text-fg">
                      {names.get(c) ?? c}
                    </Link>
                  </th>
                ))}
                <th className="w-24 px-2 py-1.5 text-right font-medium">
                  {previews.length > 0 && (
                    <span title="Preview environments inherit from their base and aren't columns here" className="font-normal text-faint">
                      +{grid.data.previews} previews
                    </span>
                  )}
                </th>
              </tr>
            </thead>
            <tbody>
              {shown.map((r) => (
                <Row key={r.key} row={r} cols={cols} path={path} names={names} onSet={grid.reload} onOpen={(env) => go({ env, path: path === "/" ? undefined : path, key: r.key })} />
              ))}
            </tbody>
          </table>
          {shown.length === 0 && <p className="px-2 py-6 text-center text-[12px] text-subtle">No key matches.</p>}
        </div>
      ) : null}
    </div>
  );
}

/** Letters for each distinct fingerprint in a row, in column order. */
function groups(row: GridRow, cols: string[]): Map<string, string> {
  const out = new Map<string, string>();
  for (const c of cols) {
    const cell = row.cells[c];
    if (cell && !out.has(cell.fp)) out.set(cell.fp, String.fromCharCode(97 + out.size));
  }
  return out;
}

function Row({ row, cols, path, names, onOpen, onSet }: { row: GridRow; cols: string[]; path: string; names: Map<string, string>; onOpen: (env: string) => void; onSet: () => void }) {
  const letters = row.status === "different" ? groups(row, cols) : null;
  const missingIn = cols.filter((c) => !row.cells[c]);
  const [setting, setSetting] = useState(false);
  return (
    <>
    <tr className="group/row">
      <td className="sticky left-0 z-[1] border-t border-line bg-canvas py-1.5 pr-3 pl-2 font-mono text-[12px] group-hover/row:bg-[color-mix(in_oklab,var(--canvas),var(--fg)_3%)]">
        <span className="flex items-center gap-1">
          <span className="min-w-0 truncate" title={row.key}>
            {row.key}
          </span>
          <button
            onClick={() => setSetting((s) => !s)}
            aria-label={`Set ${row.key} in several environments`}
            title="Set in several environments"
            className="ml-auto shrink-0 rounded p-0.5 text-subtle opacity-0 transition-opacity hover:text-fg focus-visible:opacity-100 group-hover/row:opacity-100"
          >
            <PenLine className="size-3.5" />
          </button>
        </span>
      </td>
      {cols.map((c) => (
        <td key={c} className="border-t border-line p-0 group-hover/row:bg-fg/[0.02]">
          <CellView cell={row.cells[c]} letter={letters && row.cells[c] ? letters.get(row.cells[c]!.fp) : undefined} onClick={() => onOpen(c)} />
        </td>
      ))}
      <td className="border-t border-line px-2 py-1.5 text-right text-[11px] group-hover/row:bg-fg/[0.02]">
        {row.status === "missing" ? (
          <span className="text-danger/80" title={`Not set in ${missingIn.join(", ")}`}>
            missing
          </span>
        ) : row.status === "different" ? (
          <span className="text-soft" title="Values differ between environments (by fingerprint)">
            differs
          </span>
        ) : (
          <span className="text-subtle">same</span>
        )}
      </td>
    </tr>
    {setting && (
      <tr>
        <td colSpan={cols.length + 2} className="border-t border-line bg-panel/60 p-0">
          <SetAcross
            keyName={row.key}
            path={path}
            cols={cols}
            names={names}
            initial={missingIn.length ? missingIn : cols}
            seen={row.cells}
            onDone={() => {
              setSetting(false);
              onSet();
            }}
            onCancel={() => setSetting(false)}
          />
        </td>
      </tr>
    )}
    </>
  );
}

/** One key's value written to several environments: one change each (a request where protected). */
function SetAcross({ keyName, path, cols, names, initial, seen, onDone, onCancel }: { keyName: string; path: string; cols: string[]; names: Map<string, string>; initial: string[]; seen: Record<string, Cell | null>; onDone: () => void; onCancel: () => void }) {
  const { api, required } = useSecrets();
  const [value, setValue] = useState("");
  const [picked, setPicked] = useState<Set<string>>(new Set(initial));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [requested, setRequested] = useState<string[]>([]);
  const unfit = misfit(required.find((r) => r.key === keyName && r.path === path), value);
  const submit = async () => {
    if (busy || picked.size === 0 || unfit) return;
    setBusy(true);
    setError(null);
    const asked: string[] = [];
    try {
      for (const env of cols.filter((c) => picked.has(c))) {
        try {
          // Check-and-set against what the grid showed: the environment's own entry at its version,
          // or none (0). An inherited or personal cell says nothing about the own entry.
          const cell = seen[env];
          const if_version = cell == null ? 0 : cell.source === "own" && !cell.personal ? cell.version : undefined;
          await api.set(env, keyName, { value, if_version }, path);
        } catch (e) {
          if (!needsApproval(e)) throw e;
          await api.openRequest(env, { path, message: `set ${keyName}`, ops: [{ op: "set", key: keyName, value }] });
          asked.push(names.get(env) ?? env);
        }
      }
      setValue("");
      if (asked.length) setRequested(asked);
      else onDone();
    } catch (e) {
      setError(errorText(e));
    }
    setBusy(false);
  };
  return (
    <form
      className="rise flex flex-wrap items-end gap-2 px-3 py-2.5"
      onSubmit={(e) => {
        e.preventDefault();
        void submit();
      }}
      onKeyDown={(e) => e.key === "Escape" && onCancel()}
    >
      <label className="min-w-56 flex-1">
        <span className="mb-1 block text-[11px] text-subtle">
          New value of <span className="font-mono text-fg">{keyName}</span>
        </span>
        <input autoFocus value={value} onChange={(e) => setValue(e.target.value)} spellCheck={false} autoComplete="off" className={`${field} font-mono`} aria-invalid={!!unfit} />
        {unfit && value && <span className="mt-1 block text-[11px] text-danger">{unfit}</span>}
      </label>
      <div>
        <span className="mb-1 block text-[11px] text-subtle">In</span>
        <div className="flex flex-wrap gap-1">
          {cols.map((c) => (
            <Chip key={c} on={picked.has(c)} onClick={() => setPicked((p) => { const n = new Set(p); if (n.has(c)) n.delete(c); else n.add(c); return n; })}>
              {names.get(c) ?? c}
            </Chip>
          ))}
        </div>
      </div>
      <span className="flex gap-1">
        <button type="button" className={ghost} onClick={onCancel}>
          Cancel
        </button>
        <button type="submit" className={primary} disabled={busy || picked.size === 0 || !!unfit}>
          {busy ? "Setting…" : `Set in ${picked.size}`}
        </button>
      </span>
      <ErrorNote error={error} onDismiss={() => setError(null)} className="w-full" />
      {requested.length > 0 && (
        <p className="w-full text-[11.5px] text-muted" role="status">
          {requested.join(", ")} {requested.length === 1 ? "is" : "are"} protected: a change request is waiting there.{" "}
          <button type="button" className={`${button} ml-1`} onClick={onDone}>
            Done
          </button>
        </p>
      )}
    </form>
  );
}

function CellView({ cell, letter, onClick }: { cell: Cell | null; letter?: string; onClick: () => void }) {
  if (!cell)
    return (
      <button onClick={onClick} className="flex h-8 w-full items-center px-2 text-left text-faint transition-colors hover:text-muted" title="Not set here: open to add it">
        —
      </button>
    );
  return (
    <button
      onClick={onClick}
      title={`v${cell.version} · ${ago(cell.updated_at)} · fingerprint ${cell.fp.slice(0, 8)}${cell.misfit ? ` · ${cell.misfit}` : ""}`}
      className="flex h-8 w-full min-w-0 items-center gap-1.5 px-2 text-left transition-colors hover:bg-fg/[0.03]"
    >
      <span className={`size-1.5 shrink-0 rounded-full ${cell.source === "own" ? "bg-fg/70" : "border border-fg/50"}`} aria-label={cell.source === "own" ? "set" : `from ${cell.source}`} />
      {letter && <span className="font-mono text-[10.5px] text-muted" title="Cells with the same letter have the same value">{letter}</span>}
      {cell.link && <LinkChip link={cell.link} compact />}
      {cell.reference && <RefMark />}
      {cell.personal && <PersonalMark />}
      {cell.comment && <CommentMark />}
      {cell.expires_at != null && <ExpiryMark at={cell.expires_at} />}
      {cell.misfit && (
        <span className="rounded border border-danger/40 px-1 font-mono text-[10px] text-danger" title={cell.misfit}>
          doesn&apos;t fit
        </span>
      )}
      <SourceMark source={cell.source} />
    </button>
  );
}

/** "/" focuses the filter (not while typing somewhere else). */
export function useSlashFocus(input: React.RefObject<HTMLInputElement | null>) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const t = e.target as HTMLElement | null;
      if (e.key !== "/" || e.metaKey || e.ctrlKey || t?.closest("input, textarea, [contenteditable]")) return;
      e.preventDefault();
      input.current?.focus();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [input]);
}
