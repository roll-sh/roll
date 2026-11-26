import { Check, Pencil, Trash2, X } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { edited, localTime, toSpec, type DiscoverState } from "@/lib/search-discover";
import { type SavedSearch, searchApi } from "@/lib/search-api";
import { btn, input, primary } from "../ui";

const message = (e: unknown) => (e instanceof Error ? e.message : String(e));

/**
 * Discover's saved searches (Kibana's saved searches, OpenSearch Dashboards' saved queries), kept
 * per cluster by roll's control plane: everyone who may read the cluster sees them, and they
 * outlive the indices they read. Open one, save the view as a new one or over the one opened,
 * rename or delete one.
 */
export function SavedSearches({
  cluster,
  state,
  opened,
  onOpen,
  onSaved,
}: {
  cluster: string;
  state: DiscoverState;
  /** The saved search the view was opened from, if any. */
  opened: SavedSearch | null;
  onOpen: (s: SavedSearch) => void;
  /** The view was saved (as `s`), or the opened search was renamed or deleted (`null`). */
  onSaved: (s: SavedSearch | null) => void;
}) {
  const [list, setList] = useState<SavedSearch[] | null>(null);
  const [name, setName] = useState("");
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [renaming, setRenaming] = useState<{ id: string; name: string } | null>(null);
  const [deleting, setDeleting] = useState<string | null>(null);

  const refresh = useCallback(
    () =>
      searchApi.savedSearches(cluster).then(
        (r) => setList(r.saved_searches),
        (e) => setErr(message(e)),
      ),
    [cluster],
  );
  useEffect(() => {
    refresh();
  }, [refresh]);

  const run = async (f: () => Promise<void>) => {
    setBusy(true);
    try {
      await f();
      setErr(null);
      await refresh();
    } catch (e) {
      setErr(message(e));
    } finally {
      setBusy(false);
    }
  };
  const saveAs = () =>
    run(async () => {
      onSaved(await searchApi.saveSearch(cluster, name, toSpec(state)));
      setName("");
    });
  const saveOver = (s: SavedSearch) => run(async () => onSaved(await searchApi.updateSavedSearch(cluster, s.id, { search: toSpec(state) })));
  const rename = (id: string, to: string) =>
    run(async () => {
      const s = await searchApi.updateSavedSearch(cluster, id, { name: to });
      if (opened?.id === id) onSaved(s);
      setRenaming(null);
    });
  const remove = (id: string) =>
    run(async () => {
      await searchApi.deleteSavedSearch(cluster, id);
      if (opened?.id === id) onSaved(null);
      setDeleting(null);
    });

  return (
    <section aria-label="Saved searches" className="rise space-y-3 rounded-lg border border-line bg-panel px-4 py-3">
      <form
        className="flex flex-wrap items-center gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          if (name.trim()) saveAs();
        }}
      >
        <input aria-label="Saved search name" value={name} onChange={(e) => setName(e.target.value)} placeholder="Name this view" maxLength={128} className={`${input} w-64 max-w-full text-[12px]`} />
        <button type="submit" disabled={busy || !name.trim() || !state.view} className={primary}>
          Save as new
        </button>
        {opened && (
          <button type="button" onClick={() => saveOver(opened)} disabled={busy || !edited(state, opened.search)} className={btn} title={`Replace ${opened.name}'s view with this one`}>
            Save to {opened.name}
          </button>
        )}
        <span className="ml-auto text-[11px] text-subtle">Everyone who can read this cluster sees its saved searches.</span>
      </form>
      {err && (
        <p role="alert" className="font-mono text-[12px] text-danger">
          {err}
        </p>
      )}
      {!list ? (
        <p className="text-[12px] text-subtle">
          <span className="shimmer">Reading saved searches…</span>
        </p>
      ) : list.length === 0 ? (
        <p className="text-[12px] text-subtle">No saved searches yet.</p>
      ) : (
        <ul className="-mx-2 max-h-64 overflow-y-auto">
          {list.map((s) => (
            <li key={s.id} className="group flex items-center gap-2 rounded px-2 py-1 transition hover:bg-panel-2">
              {renaming?.id === s.id ? (
                <form
                  className="flex min-w-0 flex-1 items-center gap-1"
                  onSubmit={(e) => {
                    e.preventDefault();
                    if (renaming.name.trim()) rename(s.id, renaming.name);
                  }}
                >
                  <input aria-label={`New name for ${s.name}`} autoFocus value={renaming.name} onChange={(e) => setRenaming({ id: s.id, name: e.target.value })} maxLength={128} className={`${input} min-w-0 flex-1 py-1 text-[12px]`} />
                  <button type="submit" disabled={busy || !renaming.name.trim()} className="rounded p-1 text-subtle transition hover:text-fg" aria-label="Rename">
                    <Check className="size-3.5" strokeWidth={1.5} />
                  </button>
                  <button type="button" onClick={() => setRenaming(null)} className="rounded p-1 text-subtle transition hover:text-fg" aria-label="Cancel the rename">
                    <X className="size-3.5" strokeWidth={1.5} />
                  </button>
                </form>
              ) : (
                <button type="button" onClick={() => onOpen(s)} className="flex min-w-0 flex-1 items-baseline gap-2 text-left" aria-label={`Open ${s.name}`}>
                  <span className={`truncate text-[13px] ${opened?.id === s.id ? "text-accent" : "text-fg"}`}>{s.name}</span>
                  <span className="truncate font-mono text-[11px] text-subtle">
                    {s.search.view} · {s.search.mode}
                    {s.search.query ? ` · ${s.search.query}` : ""}
                  </span>
                </button>
              )}
              {deleting === s.id ? (
                <span className="flex shrink-0 items-center gap-1 text-[12px]">
                  <button type="button" onClick={() => remove(s.id)} disabled={busy} className={`${btn} py-0.5 text-danger`}>
                    Delete {s.name}
                  </button>
                  <button type="button" onClick={() => setDeleting(null)} className="rounded p-1 text-subtle transition hover:text-fg" aria-label="Keep it">
                    <X className="size-3.5" strokeWidth={1.5} />
                  </button>
                </span>
              ) : (
                renaming?.id !== s.id && (
                  <span className="flex shrink-0 items-center gap-0.5">
                    <span className="mr-1 hidden font-mono text-[11px] text-subtle sm:inline" title="Last saved, in your time zone">
                      {localTime(s.updated_at * 1000).slice(0, 16)}
                    </span>
                    <button type="button" onClick={() => setRenaming({ id: s.id, name: s.name })} className="rounded p-1 text-subtle opacity-0 transition group-hover:opacity-100 hover:text-fg focus:opacity-100" aria-label={`Rename ${s.name}`}>
                      <Pencil className="size-3.5" strokeWidth={1.5} />
                    </button>
                    <button type="button" onClick={() => setDeleting(s.id)} className="rounded p-1 text-subtle opacity-0 transition group-hover:opacity-100 hover:text-fg focus:opacity-100" aria-label={`Delete ${s.name}`}>
                      <Trash2 className="size-3.5" strokeWidth={1.5} />
                    </button>
                  </span>
                )
              )}
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}
