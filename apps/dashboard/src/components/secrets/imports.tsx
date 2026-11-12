import { ArrowDown, ArrowUp, ArrowUpRight, Plus, Trash2, X } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { FOLDER_PATH, type Environment, type Import } from "@/lib/secrets-api";
import { Select } from "../agents/ui/select";
import { useSecrets } from "./secrets-page";
import { ErrorNote, errorText, ghost, iconButton, primary, sized } from "./ui";

const MAX = 20;

type Row = Import & { id: number };

const same = (a: Import, b: Import) => a.environment === b.environment && a.path === b.path;

/**
 * A folder's imports, in priority order: other folders whose keys show up here read-only. The
 * folder's own keys win, then the imports top to bottom, then (a preview) its base. Saving replaces
 * the list in one call.
 */
export function ImportsPanel({ env, path, saved, highlight, onSaved, onClose }: { env: Environment; path: string; saved: Import[]; highlight: Import | null; onSaved: (list: Import[]) => void; onClose: () => void }) {
  const { api, standard, go } = useSecrets();
  const ids = useRef(0);
  const rows = (list: Import[]): Row[] => list.map((i) => ({ ...i, id: ids.current++ }));
  const [draft, setDraft] = useState<Row[]>(() => rows(saved));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const lit = useRef<HTMLLIElement>(null);

  // A save elsewhere (or another folder) replaces the draft.
  const key = JSON.stringify(saved);
  useEffect(() => {
    setDraft(rows(saved));
  }, [key]);
  // Braces: Chromium's scrollIntoView returns a promise now, and an effect may only return a cleanup.
  useEffect(() => {
    lit.current?.scrollIntoView({ block: "nearest" });
  }, [highlight]);

  const options = [...standard, ...draft.filter((d) => !standard.some((e) => e.slug === d.environment)).map((d) => ({ slug: d.environment, name: d.environment }))]
    .filter((e, i, all) => all.findIndex((o) => o.slug === e.slug) === i)
    .map((e) => ({ value: e.slug, label: e.name }));
  const problem = (r: Row): string | null =>
    !FOLDER_PATH.test(r.path) ? "A path is / or /folder/…" : r.environment === env.slug && r.path === path ? "A folder can't import itself" : draft.some((o) => o.id < r.id && same(o, r)) ? "Listed twice" : null;
  const ok = draft.every((r) => !problem(r));
  const changed = JSON.stringify(draft.map(({ environment, path }) => ({ environment, path }))) !== key;

  const update = (id: number, patch: Partial<Import>) => setDraft((d) => d.map((r) => (r.id === id ? { ...r, ...patch } : r)));
  const move = (i: number, by: number) =>
    setDraft((d) => {
      const next = [...d];
      [next[i], next[i + by]] = [next[i + by], next[i]];
      return next;
    });
  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      onSaved(await api.setImports(env.slug, path, draft.map(({ environment, path }) => ({ environment, path }))));
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  const fallback = standard.find((e) => e.slug !== env.slug)?.slug ?? standard[0]?.slug ?? env.slug;

  return (
    <section aria-label={`Imports into ${path}`} className="rise mx-3 mt-3 rounded-xl border border-line-2 bg-panel p-3 inset-shadow-highlight">
      <div className="flex items-start gap-2">
        <div className="min-w-0 flex-1">
          <h2 className="text-[13px] font-medium">
            Imports into <span className="font-mono">{path}</span>
          </h2>
          <p className="mt-0.5 text-[11.5px] text-muted">
            Their keys show up here read-only. This folder's own keys win, then the imports from top to bottom{env.kind === "ephemeral" ? `, then ${env.base}` : ""}. Saving isn't a change you roll back; it's in the audit log.
          </p>
        </div>
        <button className={iconButton} onClick={onClose} aria-label="Close imports">
          <X className="size-3.5" />
        </button>
      </div>

      {draft.length > 0 ? (
        <ol className="mt-3 space-y-1">
          {draft.map((r, i) => {
            const p = problem(r);
            const on = !!highlight && same(highlight, r);
            return (
              <li key={r.id} ref={on ? lit : undefined} className={`rounded-lg px-1.5 py-1 transition-colors ${on ? "bg-accent/[0.08]" : ""}`}>
                <div className="flex flex-wrap items-center gap-1.5">
                  <span className="w-5 text-right font-mono text-[11px] text-subtle">{i + 1}</span>
                  <Select label="Environment" value={r.environment} onChange={(v) => update(r.id, { environment: v })} options={options} className="w-40" />
                  <input value={r.path} onChange={(e) => update(r.id, { path: e.target.value.trim() })} aria-label="Folder" spellCheck={false} className={`${sized} h-8 w-48 font-mono text-[12px] max-sm:flex-1`} />
                  <span className="ml-auto flex items-center gap-0.5">
                    <button className={iconButton} disabled={!!p} onClick={() => go({ env: r.environment, path: r.path === "/" ? undefined : r.path })} aria-label={`Open ${r.environment}${r.path}`} title="Open that folder">
                      <ArrowUpRight className="size-3.5" />
                    </button>
                    <button className={iconButton} disabled={i === 0} onClick={() => move(i, -1)} aria-label={`Move import ${i + 1} up`}>
                      <ArrowUp className="size-3.5" />
                    </button>
                    <button className={iconButton} disabled={i === draft.length - 1} onClick={() => move(i, 1)} aria-label={`Move import ${i + 1} down`}>
                      <ArrowDown className="size-3.5" />
                    </button>
                    <button className={iconButton} onClick={() => setDraft((d) => d.filter((o) => o.id !== r.id))} aria-label={`Remove import ${i + 1}`}>
                      <Trash2 className="size-3.5" />
                    </button>
                  </span>
                </div>
                {p && <p className="mt-0.5 pl-6.5 text-[11px] text-danger">{p}</p>}
              </li>
            );
          })}
        </ol>
      ) : (
        <p className="mt-3 text-[12px] text-subtle">This folder imports nothing.</p>
      )}

      <ErrorNote error={error} onDismiss={() => setError(null)} className="mt-2" />
      <div className="mt-3 flex flex-wrap items-center gap-2">
        <button className={ghost} disabled={draft.length >= MAX} onClick={() => setDraft((d) => [...d, { environment: fallback, path, id: ids.current++ }])} title={draft.length >= MAX ? `At most ${MAX} imports per folder` : undefined}>
          <Plus className="size-3.5" /> Add an import
        </button>
        <span className="ml-auto" />
        {changed && (
          <button className={ghost} onClick={() => setDraft(rows(saved))}>
            Reset
          </button>
        )}
        <button className={primary} disabled={!changed || !ok || busy} onClick={save}>
          {busy ? "Saving…" : "Save imports"}
        </button>
      </div>
    </section>
  );
}
