import { ArrowDown, ArrowUp, ArrowUpRight, Plus, Share2, Trash2, X } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { FOLDER_PATH, type Environment, type Import } from "@/lib/secrets-api";
import { useWorkspace } from "@/lib/workspace-store";
import { Select } from "../agents/ui/select";
import { useSecrets } from "./secrets-page";
import { ErrorNote, errorText, ghost, iconButton, primary, sized, useLoad } from "./ui";

const MAX = 20;

type Row = Import & { id: number };

const same = (a: Import, b: Import) => a.environment === b.environment && a.path === b.path && (a.project ?? null) === (b.project ?? null);
const plain = ({ project, environment, path }: Import): Import => (project ? { project, environment, path } : { environment, path });

/**
 * A folder's imports, in priority order: other folders whose keys show up here read-only. The
 * folder's own keys win, then the imports top to bottom, then (a preview) its base. Saving replaces
 * the list in one call.
 */
export function ImportsPanel({ env, path, saved, highlight, onSaved, onClose }: { env: Environment; path: string; saved: Import[]; highlight: Import | null; onSaved: (list: Import[]) => void; onClose: () => void }) {
  const { api, standard, go, project: here } = useSecrets();
  const ws = useWorkspace();
  const others = ws.projects.filter((p) => p.id !== here.id);
  const projectName = (id: string) => ws.projects.find((p) => p.id === id)?.name ?? id;
  const offered = useLoad(() => api.shared(), [api]);
  const [picking, setPicking] = useState(false);
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
    !FOLDER_PATH.test(r.path)
      ? "A path is / or /folder/…"
      : !r.project && r.environment === env.slug && r.path === path
        ? "A folder can't import itself"
        : r.project && !r.environment.trim()
          ? "Name the other project's environment"
          : draft.some((o) => o.id < r.id && same(o, r))
            ? "Listed twice"
            : null;
  const ok = draft.every((r) => !problem(r));
  const changed = JSON.stringify(draft.map(plain)) !== JSON.stringify(saved.map(plain));

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
      onSaved(await api.setImports(env.slug, path, draft.map(plain)));
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
                  {r.project ? (
                    <>
                      <Select label="Project" value={r.project} onChange={(v) => update(r.id, { project: v })} options={[...others.map((p) => ({ value: p.id, label: p.name })), ...(others.some((p) => p.id === r.project) ? [] : [{ value: r.project, label: projectName(r.project) }])]} className="w-36" />
                      <input value={r.environment} onChange={(e) => update(r.id, { environment: e.target.value.trim() })} aria-label="Its environment" placeholder="production" spellCheck={false} className={`${sized} h-8 w-28 font-mono text-[12px]`} />
                    </>
                  ) : (
                    <Select label="Environment" value={r.environment} onChange={(v) => update(r.id, { environment: v })} options={options} className="w-40" />
                  )}
                  <input value={r.path} onChange={(e) => update(r.id, { path: e.target.value.trim() })} aria-label="Folder" spellCheck={false} className={`${sized} h-8 w-48 font-mono text-[12px] max-sm:flex-1`} />
                  <span className="ml-auto flex items-center gap-0.5">
                    <button className={iconButton} disabled={!!p || !!r.project} onClick={() => go({ env: r.environment, path: r.path === "/" ? undefined : r.path })} aria-label={`Open ${r.environment}${r.path}`} title={r.project ? `A folder ${projectName(r.project)} shares with this project` : "Open that folder"}>
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
        {others.length > 0 && (
          <button className={ghost} disabled={draft.length >= MAX} onClick={() => setPicking((p) => !p)} aria-expanded={picking} title="A folder another project shares with this one">
            <Plus className="size-3.5" /> From another project
          </button>
        )}
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
      {picking && (
        <div className="rise mt-2 rounded-lg border border-line-2 p-2">
          {(offered.data ?? []).length === 0 ? (
            <p className="text-[11.5px] text-subtle">Nothing is shared with this project yet. A folder&apos;s admin in another project shares it here first (its imports panel, &quot;Shared with other projects&quot;).</p>
          ) : (
            <ul className="flex flex-wrap gap-1.5">
              {(offered.data ?? []).map((o) => {
                const taken = draft.some((d) => same(d, o));
                return (
                  <li key={`${o.project}:${o.environment}:${o.path}`}>
                    <button
                      className={`${ghost} font-mono text-[12px]`}
                      disabled={taken || draft.length >= MAX}
                      onClick={() => {
                        setDraft((d) => [...d, { ...o, id: ids.current++ }]);
                        setPicking(false);
                      }}
                    >
                      {projectName(o.project ?? "")} · {o.environment}
                      {o.path === "/" ? "" : o.path}
                      {taken ? " (imported)" : ""}
                    </button>
                  </li>
                );
              })}
            </ul>
          )}
        </div>
      )}
      {others.length > 0 && <SharedWith env={env} path={path} others={others} />}
    </section>
  );
}

/** The projects this folder is shared with (they can import it): an admin's choice, audited. */
function SharedWith({ env, path, others }: { env: Environment; path: string; others: { id: string; name: string }[] }) {
  const { api } = useSecrets();
  const shared = useLoad(() => api.exports(env.slug, path), [api, env.slug, path]);
  const [pick, setPick] = useState(others[0]?.id ?? "");
  const [error, setError] = useState<string | null>(null);
  const list = (shared.data ?? []).map((e) => e.project);
  const name = (id: string) => others.find((p) => p.id === id)?.name ?? id;
  const save = (next: string[]) => api.setExports(env.slug, path, next).then(shared.reload, (e) => setError(errorText(e)));
  return (
    <div className="mt-3 border-t border-line pt-3">
      <h3 className="flex items-center gap-1.5 text-[12.5px] font-medium">
        <Share2 className="size-3.5 text-muted" /> Shared with other projects
      </h3>
      <p className="mt-0.5 text-[11.5px] text-muted">A project this folder is shared with can import its stored values; their readers then read them. Stopping takes effect at once.</p>
      {list.length > 0 && (
        <ul className="mt-2 flex flex-wrap gap-1.5">
          {list.map((p) => (
            <li key={p} className="flex items-center gap-1 rounded-md border border-line-2 px-1.5 py-0.5 text-[12px]">
              {name(p)}
              <button className={iconButton} onClick={() => save(list.filter((x) => x !== p))} aria-label={`Stop sharing with ${name(p)}`}>
                <X className="size-3" />
              </button>
            </li>
          ))}
        </ul>
      )}
      <div className="mt-2 flex flex-wrap items-center gap-2">
        <Select label="Project" value={pick} onChange={setPick} options={others.map((p) => ({ value: p.id, label: p.name }))} className="w-44" />
        <button className={ghost} disabled={!pick || list.includes(pick)} onClick={() => save([...list, pick])}>
          <Share2 className="size-3.5" /> Share
        </button>
      </div>
      <ErrorNote error={error ?? shared.error} onDismiss={error ? () => setError(null) : undefined} className="mt-2" />
    </div>
  );
}
