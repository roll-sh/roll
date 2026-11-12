import { ArrowRight } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { Link } from "@/components/link";
import { needsApproval, type CompareStatus } from "@/lib/secrets-api";
import { Select } from "../agents/ui/select";
import { FolderCrumbs } from "./folders";
import { secretsHref, useSecrets } from "./secrets-page";
import { ErrorNote, errorText, field, primary, Teach, useLoad, Warnings } from "./ui";

const STATUS: Record<CompareStatus, { label: string; tone: string; promotable: boolean }> = {
  different: { label: "differs", tone: "text-soft", promotable: true },
  only_from: { label: "only in from", tone: "text-ok", promotable: true },
  only_to: { label: "only in to", tone: "text-subtle", promotable: false },
  same: { label: "same", tone: "text-faint", promotable: false },
};

/**
 * Compare two environments key by key (fingerprints decide; nothing is revealed), pick the keys,
 * and promote them from one to the other as one change in the target.
 */
export function CompareView() {
  const { api, standard, search, go, project, reloadEnvs } = useSecrets();
  const from = search.from && standard.some((e) => e.slug === search.from) ? search.from : standard[0]?.slug;
  const to = search.to && standard.some((e) => e.slug === search.to) ? search.to : standard.find((e) => e.slug !== from)?.slug;
  const path = search.path || "/";
  const cmp = useLoad(() => (from && to && from !== to ? api.compare(from, to, path) : Promise.resolve(null)), [api, from, to, path]);
  const [picked, setPicked] = useState<Set<string>>(new Set());
  const [message, setMessage] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [done, setDone] = useState<{ text: string; warnings: string[] } | null>(null);

  const rows = cmp.data?.rows ?? [];
  const [showSame, setShowSame] = useState(false);
  const shown = rows.filter((r) => showSame || r.status !== "same");
  // Pick every key that would change by default.
  useEffect(() => setPicked(new Set(rows.filter((r) => STATUS[r.status].promotable).map((r) => r.key))), [cmp.data]);
  const promotable = useMemo(() => rows.filter((r) => STATUS[r.status].promotable), [rows]);
  const options = standard.map((e) => ({ value: e.slug, label: e.name }));
  const nav = (next: { from?: string; to?: string; path?: string }) => go({ view: "compare", from: next.from ?? from, to: next.to ?? to, path: (next.path ?? path) === "/" ? undefined : (next.path ?? path) }, true);
  const toName = standard.find((e) => e.slug === to)?.name ?? to;

  const promote = async () => {
    if (!from || !to) return;
    setBusy(true);
    setError(null);
    try {
      const r = await api.promote(to, from, { path, keys: [...picked], message: message || undefined });
      setDone({ text: r.change ? `Promoted ${r.items.length} ${r.items.length === 1 ? "key" : "keys"} to ${toName} as change #${r.change.seq}` : "Nothing to promote: they already match", warnings: r.warnings });
      setMessage("");
      cmp.reload();
      reloadEnvs();
    } catch (e) {
      setError(needsApproval(e) ? `${e.message}. Promotion doesn't open a change request yet: set the keys in ${toName} (each write offers one), or an admin lifts protection under Environments.` : errorText(e));
    } finally {
      setBusy(false);
    }
  };

  if (standard.length < 2) return <Teach title="Compare needs two environments">Add another under Environments, then compare and promote keys between them.</Teach>;

  return (
    <div className="mx-auto max-w-5xl px-4 py-5">
      <div className="flex flex-wrap items-end gap-2">
        <label>
          <span className="mb-1 block text-[11px] font-medium text-subtle">From</span>
          <Select label="From" value={from ?? null} onChange={(v) => nav({ from: v })} options={options} className="w-44" />
        </label>
        <ArrowRight className="mb-2 size-4 text-muted" />
        <label>
          <span className="mb-1 block text-[11px] font-medium text-subtle">To</span>
          <Select label="To" value={to ?? null} onChange={(v) => nav({ to: v })} options={options} className="w-44" />
        </label>
        <span className="mb-1.5 ml-2">
          <FolderCrumbs path={path} onPath={(p) => nav({ path: p })} />
        </span>
        <label className="mb-1.5 ml-auto flex items-center gap-1.5 text-[12px] text-muted">
          <input type="checkbox" checked={showSame} onChange={(e) => setShowSame(e.target.checked)} className="accent-[var(--accent)]" />
          Show keys that match
        </label>
      </div>

      {from === to && <p className="mt-4 text-[12.5px] text-muted">Pick two different environments.</p>}
      <ErrorNote error={cmp.error} className="mt-4" />

      {cmp.data && (
        <>
          <div className="mt-4 overflow-hidden rounded-xl border border-line-2 bg-panel inset-shadow-highlight">
            <div className="flex items-center gap-3 border-b border-line px-3 py-2 text-[11px] font-medium text-subtle">
              <input
                type="checkbox"
                aria-label="Pick every key that would change"
                checked={promotable.length > 0 && promotable.every((r) => picked.has(r.key))}
                onChange={(e) => setPicked(new Set(e.target.checked ? promotable.map((r) => r.key) : []))}
                className="accent-[var(--accent)]"
              />
              <span className="flex-1">Key</span>
              <span className="w-20 text-right">{from}</span>
              <span className="w-20 text-right">{to}</span>
              <span className="w-36 text-right">Status</span>
            </div>
            {shown.length === 0 && <p className="px-3 py-6 text-center text-[12px] text-subtle">{rows.length ? "Every key matches." : "Neither environment has keys here."}</p>}
            {shown.map((r) => {
              const s = STATUS[r.status];
              return (
                <label key={r.key} className={`flex items-center gap-3 border-t border-line px-3 py-1.5 text-[12.5px] first:border-t-0 ${s.promotable ? "cursor-pointer hover:bg-fg/[0.03]" : ""}`}>
                  <input
                    type="checkbox"
                    disabled={!s.promotable}
                    checked={picked.has(r.key)}
                    onChange={(e) => setPicked((p) => {
                      const n = new Set(p);
                      if (e.target.checked) n.add(r.key);
                      else n.delete(r.key);
                      return n;
                    })}
                    className="accent-[var(--accent)] disabled:opacity-30"
                  />
                  <span className="min-w-0 flex-1 truncate font-mono text-[12px]">{r.key}</span>
                  <span className="w-20 text-right font-mono text-[11px] text-muted">{r.from === null ? "—" : `v${r.from}`}</span>
                  <span className="w-20 text-right font-mono text-[11px] text-muted">{r.to === null ? "—" : `v${r.to}`}</span>
                  <span className={`w-36 text-right text-[11.5px] whitespace-nowrap ${s.tone}`}>{r.status === "only_from" ? `only in ${from}` : r.status === "only_to" ? `only in ${to}` : s.label}</span>
                </label>
              );
            })}
          </div>
          <div className="mt-3 flex flex-wrap items-center gap-2">
            <input value={message} onChange={(e) => setMessage(e.target.value)} placeholder={`promoted from ${from}`} className={`${field} max-w-sm flex-1`} aria-label="Message" />
            <button className={primary} disabled={!picked.size || busy} onClick={promote}>
              {busy ? "Promoting…" : `Promote ${picked.size} to ${toName}`}
            </button>
            <span className="text-[11.5px] text-subtle">Copies values, comments, tags and links as one change in {toName}.</span>
          </div>
          <ErrorNote error={error} className="mt-3" onDismiss={() => setError(null)} />
          {done && (
            <div className="fade-in mt-3 text-[12px] text-muted">
              {done.text} ·{" "}
              <Link className="text-accent hover:underline" href={secretsHref(project.id, { env: to })}>
                open {toName}
              </Link>
              <Warnings items={done.warnings} className="mt-1.5" />
            </div>
          )}
        </>
      )}
    </div>
  );
}
