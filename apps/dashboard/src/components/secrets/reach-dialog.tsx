import { ShieldAlert, ShieldCheck } from "lucide-react";
import type { ReactNode } from "react";
import type { Environment, Holder, Reach } from "@/lib/secrets-api";
import { Dialog } from "../agents/ui/dialog";
import { useSecrets } from "./secrets-page";
import { ActorMark, ago, ErrorNote, until, useLoad } from "./ui";

/**
 * Reach (docs/secrets.md, "Reach"): who and what can read a key's value — tokens and identities
 * whose scope covers it, untrusted code, syncs, projects and folders it flows to — and who read it
 * in 30 days. Names only, a project admin's.
 */
export function ReachDialog({ open, onClose, env, path, keyName }: { open: boolean; onClose: () => void; env: Environment; path: string; keyName: string }) {
  return (
    <Dialog open={open} onClose={onClose} title={`Who can read ${keyName}`} lead={`In ${env.name}${path === "/" ? "" : ` ${path}`}: credentials that reach it, where it goes by itself, and who read it lately. Names only.`} className="w-[min(640px,100%)]">
      {open && <Body env={env} path={path} keyName={keyName} />}
    </Dialog>
  );
}

function Body({ env, path, keyName }: { env: Environment; path: string; keyName: string }) {
  const { api } = useSecrets();
  const reach = useLoad(() => api.reach(env.slug, path, keyName), [api, env.slug, path, keyName]);
  const r = reach.data;
  if (reach.error) return <ErrorNote error={reach.error} />;
  if (!r) return <p className="text-[12px] text-subtle">Looking…</p>;
  const flows = [...r.syncs.map((s) => `${s.kind} ${summary(s.target)}`), ...r.projects.map((p) => `shared with project ${p}`), ...r.imported_by.map((f) => `imported by ${f}`)];
  return (
    <div className="space-y-4 text-[12.5px]">
      <p className="flex items-start gap-2 text-muted">
        {r.untrusted ? <ShieldAlert className="mt-0.5 size-3.5 shrink-0 text-danger" /> : <ShieldCheck className="mt-0.5 size-3.5 shrink-0 text-ok" />}
        {r.untrusted ? `Checks, sandboxes and previews running code roll hasn't vetted can read it: protect ${env.name} to stop that.` : `${env.name} is protected: code roll hasn't vetted never reads it.`}
      </p>
      <Part title="Tokens" empty="No token reaches it.">
        {r.tokens.map((h) => (
          <HolderRow key={h.id} h={h} />
        ))}
      </Part>
      <Part title="Workload identities" empty="No identity reaches it.">
        {r.identities.map((h) => (
          <HolderRow key={h.id} h={h} />
        ))}
      </Part>
      <Part title="Where it goes by itself" empty="No sync, share or import takes it anywhere.">
        {flows.map((f) => (
          <li key={f} className="truncate px-3 py-2 font-mono text-[11.5px] text-soft">
            {f}
          </li>
        ))}
      </Part>
      <Part title="Read in the last 30 days" empty="Nobody read it in 30 days." note={`Not counted: ${r.uncounted}.`}>
        {r.readers.map((x) => (
          <li key={x.actor} className="flex items-center justify-between gap-3 px-3 py-2">
            <ActorMark who={x.actor} />
            <span className="shrink-0 text-[11.5px] text-subtle">
              {x.reads} {x.reads === 1 ? "read" : "reads"} · {ago(x.last_at)}
            </span>
          </li>
        ))}
      </Part>
    </div>
  );
}

function Part({ title, empty, note, children }: { title: string; empty: string; note?: string; children: ReactNode[] }) {
  return (
    <section>
      <h3 className="mb-1.5 text-[11px] font-medium text-subtle">{title}</h3>
      {children.length ? <ul className="divide-y divide-line overflow-hidden rounded-lg border border-line-2 bg-panel">{children}</ul> : <p className="text-[12px] text-faint">{empty}</p>}
      {note && <p className="mt-1.5 text-[11px] text-faint">{note}</p>}
    </section>
  );
}

function HolderRow({ h }: { h: Holder }) {
  return (
    <li className="flex items-center justify-between gap-3 px-3 py-2">
      <span className="min-w-0 truncate text-fg">{h.name}</span>
      <span className="flex shrink-0 items-center gap-1.5 text-[11px] text-subtle">
        {h.actions.map((a) => (
          <span key={a} className={`rounded border px-1 font-mono ${a === "reveal" ? "border-accent/40 text-accent" : "border-line-2"}`}>
            {a}
          </span>
        ))}
        <span>{h.last_used_at ? `used ${ago(h.last_used_at)}` : "never used"}</span>
        {h.expires_at && <span>· ends {until(h.expires_at)}</span>}
      </span>
    </li>
  );
}

/** A sync's target in a few words (its kind's own fields: repo, project, namespace…). */
function summary(target: unknown): string {
  if (!target || typeof target !== "object") return "";
  return Object.entries(target as Record<string, unknown>)
    .filter(([, v]) => typeof v === "string" && v.length < 80)
    .map(([k, v]) => `${k}=${v as string}`)
    .slice(0, 3)
    .join(" ");
}
