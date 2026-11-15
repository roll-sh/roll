import { Link2, X } from "lucide-react";
import { useState } from "react";
import { needsApproval, type Suggestion } from "@/lib/secrets-api";
import { useSecrets } from "./secrets-page";
import { button, card, ErrorNote, errorText, ghost, LinkChip, useLoad } from "./ui";

/**
 * Suggested links (docs/secrets.md, "Links"): resources on the project's canvas that some
 * standard environments don't link yet, each one click to link under its usual keys everywhere it's offered (a
 * protected environment gets a change request instead).
 */
export function Suggestions({ onLinked }: { onLinked: () => void }) {
  const { api, standard } = useSecrets();
  const list = useLoad(() => api.suggestions(), [api]);
  const [hidden, setHidden] = useState<Set<string>>(new Set());
  const [busy, setBusy] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const names = new Map(standard.map((e) => [e.slug, e.name]));
  const rows = (list.data ?? []).filter((s) => !hidden.has(`${s.kind}:${s.ref}`));
  if (rows.length === 0) return null;

  const link = async (s: Suggestion) => {
    setBusy(`${s.kind}:${s.ref}`);
    setError(null);
    const ops = s.links.map((l) => ({ op: "set" as const, key: l.key, link: { kind: s.kind, ref: s.ref, field: l.field, mode: l.mode } }));
    const requested: string[] = [];
    try {
      for (const env of s.environments) {
        const batch = { path: "/", message: `linked ${s.name ?? s.ref}`, ops };
        try {
          await api.batch(env, batch);
        } catch (e) {
          if (!needsApproval(e)) throw e;
          await api.openRequest(env, batch);
          requested.push(names.get(env) ?? env);
        }
      }
      setNote(`Linked ${s.name ?? s.ref}${requested.length ? `; ${requested.join(", ")} ${requested.length === 1 ? "is" : "are"} protected, so a change request is waiting there` : ""}.`);
      list.reload();
      onLinked();
    } catch (e) {
      setError(errorText(e));
    }
    setBusy(null);
  };

  return (
    <div className="mx-3 mt-3">
      <ErrorNote error={error} onDismiss={() => setError(null)} className="mb-2" />
      {note && <p className="fade-in mb-2 text-[12px] text-muted" role="status">{note}</p>}
      <div className={`${card} divide-y divide-line`}>
        {rows.map((s) => {
          const id = `${s.kind}:${s.ref}`;
          return (
            <div key={id} className="flex flex-wrap items-center gap-x-3 gap-y-1.5 px-3 py-2">
              <LinkChip link={{ kind: s.kind, ref: s.ref, field: s.links[0]?.field ?? "", mode: s.links[0]?.mode ?? "live" }} name={s.name ?? s.ref} />
              <span className="flex flex-wrap gap-1">
                {s.links.map((l) => (
                  <span key={l.key} className="rounded border border-line-2 px-1 font-mono text-[11px] text-muted" title={`${l.field}, ${l.mode}`}>
                    {l.key}
                  </span>
                ))}
              </span>
              <span className="text-[11.5px] text-subtle">on the canvas, not linked in {s.environments.map((e) => names.get(e) ?? e).join(", ")} yet</span>
              <span className="ml-auto flex gap-1">
                <button className={button} disabled={busy !== null} onClick={() => link(s)}>
                  <Link2 className={`size-3.5 ${busy === id ? "animate-pulse text-accent" : ""}`} /> Link {s.environments.length === 1 ? "it" : `in all ${s.environments.length}`}
                </button>
                <button className={ghost} onClick={() => setHidden((h) => new Set(h).add(id))} aria-label={`Not now: ${s.name ?? s.ref}`}>
                  <X className="size-3.5" />
                </button>
              </span>
            </div>
          );
        })}
      </div>
    </div>
  );
}
