import { Cloud, Database, KeyRound, Link2, RotateCw, type LucideIcon } from "lucide-react";
import { useMemo, useState } from "react";
import { SECRET_KEY, type Batch, type Environment, type LinkField, type LinkMode, type LinkTarget, type Op } from "@/lib/secrets-api";
import type { ResourceKind } from "@/lib/workspace-api";
import { Dialog } from "../agents/ui/dialog";
import { Segmented } from "../agents/ui/toggle-group";
import { KIND_ICON } from "../workspace/kind";
import { useSecrets } from "./secrets-page";
import { ErrorNote, field, sized, ghost, primary, useLoad } from "./ui";

/** Link kinds that aren't canvas resources: roll secrets' own registries. */
const OWN_KINDS: Record<string, { icon: LucideIcon; label: string }> = {
  rotator: { icon: RotateCw, label: "Rotator" },
  aws: { icon: Cloud, label: "AWS role" },
  aws_iam_user: { icon: KeyRound, label: "IAM user" },
  outside_database: { icon: Database, label: "Database outside roll" },
  gcp: { icon: Cloud, label: "GCP account" },
  azure: { icon: Cloud, label: "Azure account" },
};

/** A link target's kind: a canvas resource's icon, roll secrets' own, or a plain link for one this dashboard doesn't know yet. */
function kindOf(kind: string): { icon: LucideIcon; label: string } {
  return OWN_KINDS[kind] ?? { icon: KIND_ICON[kind as ResourceKind] ?? Link2, label: kind };
}

function LinkKindTile({ kind }: { kind: string }) {
  const Icon = kindOf(kind).icon;
  return (
    <span className="grid size-[22px] shrink-0 place-items-center rounded-md border border-line-2 bg-raised text-fg inset-shadow-highlight">
      <Icon className="size-3.5" />
    </span>
  );
}

const MODE_LABEL: Record<LinkMode, string> = { live: "Live", managed: "Managed", dynamic: "Dynamic" };
const MODE_HELP: Record<LinkMode, string> = {
  live: "Read from the resource whenever it's resolved, so it's always current.",
  managed: "roll issues a credential, stores it as the value, and can rotate it.",
  dynamic: "Every resolve (roll run, an app's start) issues a short-lived credential, revoked when its lease ends.",
};

type Pick = { on: boolean; key: string; mode: LinkMode };
type WriteFn = (run: () => Promise<{ warnings?: string[]; change?: { seq: number } | null }>, batch: Batch, done: string) => Promise<boolean>;

/**
 * "Link a resource": the project's resources and their fields (`GET …/links`). Fields that come
 * from one issued credential (an access key and its secret) are linked together, one mode for the
 * pair, in one change, so they share one credential.
 */
export function LinkDialog({ open, onClose, env, path, taken, onWrite }: { open: boolean; onClose: () => void; env: Environment; path: string; taken: Set<string>; onWrite: WriteFn }) {
  const [n, setN] = useState(0);
  return (
    <Dialog open={open} onClose={onClose} onExited={() => setN((k) => k + 1)} title={`Link a resource in ${env.name}`} lead="The value comes from a resource on the project's canvas and stays correct when it changes or rotates." className="w-[min(720px,100%)]">
      <Form key={n} env={env} path={path} taken={taken} onWrite={onWrite} onClose={onClose} />
    </Dialog>
  );
}

function Form({ env, path, taken, onWrite, onClose }: { env: Environment; path: string; taken: Set<string>; onWrite: WriteFn; onClose: () => void }) {
  const { api, project } = useSecrets();
  const targets = useLoad(() => api.links(), [api]);
  const [at, setAt] = useState<string | null>(null);
  const target = targets.data?.find((t) => `${t.kind}:${t.ref}` === at) ?? targets.data?.[0] ?? null;
  const [picks, setPicks] = useState<Record<string, Pick>>({});
  const [ttl, setTtl] = useState("60");
  const [busy, setBusy] = useState(false);

  const pickOf = (t: LinkTarget, f: LinkField): Pick => picks[`${t.kind}:${t.ref}:${f.field}`] ?? { on: false, key: f.suggest, mode: f.modes[0] };
  const setPick = (t: LinkTarget, f: LinkField, p: Partial<Pick>) => setPicks((all) => ({ ...all, [`${t.kind}:${t.ref}:${f.field}`]: { ...pickOf(t, f), ...p } }));

  const chosen = useMemo(
    () =>
      (targets.data ?? []).flatMap((t) =>
        t.fields.map((f) => ({ t, f, p: picks[`${t.kind}:${t.ref}:${f.field}`] })).filter((x): x is { t: LinkTarget; f: LinkField; p: Pick } => !!x.p?.on),
      ),
    [targets.data, picks],
  );
  const badKey = chosen.find((c) => !SECRET_KEY.test(c.p.key));
  const dupKey = chosen.find((c, i) => chosen.findIndex((d) => d.p.key === c.p.key) !== i);
  const ttlMinutes = Number(ttl);
  const needsTtl = chosen.some((c) => c.p.mode === "dynamic");
  const ttlOk = !needsTtl || (Number.isInteger(ttlMinutes) && ttlMinutes >= 1 && ttlMinutes <= 7 * 24 * 60);

  const submit = async () => {
    const ops: Op[] = chosen.map((c) => ({
      op: "set",
      key: c.p.key,
      link: { kind: c.t.kind, ref: c.t.ref, field: c.f.field, mode: c.p.mode, ...(c.p.mode === "dynamic" && { options: { ttl_minutes: ttlMinutes } }) },
    }));
    const batch: Batch = { path, ops, message: `linked ${ops.map((o) => o.key).join(", ")}` };
    setBusy(true);
    // The view behind shows the outcome: the change, its error, or the offer to open a change request.
    await onWrite(() => api.batch(env.slug, batch), batch, `Linked ${ops.length} ${ops.length === 1 ? "key" : "keys"}`);
    setBusy(false);
    onClose();
  };

  if (targets.error) return <ErrorNote error={targets.error} />;
  if (!targets.data) return <p className="shimmer py-6 text-center text-[12px]">Loading the project's resources…</p>;
  if (!targets.data.length)
    return (
      <p className="py-6 text-center text-[12.5px] text-muted">
        {project.name} has no linkable resources yet. Add a database, a bucket or a stream to its canvas, then link its credentials here.
      </p>
    );

  // Fields of one credential sit together; the rest one by one.
  const groups = target ? groupFields(target.fields) : [];

  return (
    <div className="grid gap-3 sm:grid-cols-[200px_minmax(0,1fr)]">
      <div className="flex flex-col gap-0.5 sm:max-h-[420px] sm:overflow-y-auto">
        {targets.data.map((t) => {
          const id = `${t.kind}:${t.ref}`;
          const n = t.fields.filter((f) => pickOf(t, f).on).length;
          return (
            <button
              key={id}
              onClick={() => setAt(id)}
              className={`flex items-center gap-2 rounded-lg px-2 py-1.5 text-left transition-colors ${target === t ? "bg-fg/[0.07]" : "hover:bg-fg/[0.04]"}`}
            >
              <LinkKindTile kind={t.kind} />
              <span className="min-w-0 flex-1 leading-tight">
                <span className="block truncate text-[12.5px] font-medium">{t.name}</span>
                <span className="block truncate text-[10.5px] text-subtle">{kindOf(t.kind).label}</span>
              </span>
              {n > 0 && <span className="font-mono text-[10.5px] text-accent">{n}</span>}
            </button>
          );
        })}
      </div>

      {target && (
        <div className="min-w-0 space-y-2 sm:max-h-[420px] sm:overflow-y-auto">
          {groups.map((g) =>
            g.credential ? (
              <CredentialGroup
                key={g.credential}
                name={g.credential}
                fields={g.fields}
                pick={(f) => pickOf(target, f)}
                set={(f, p) => setPick(target, f, p)}
                taken={taken}
              />
            ) : (
              <FieldRow key={g.fields[0].field} f={g.fields[0]} p={pickOf(target, g.fields[0])} set={(p) => setPick(target, g.fields[0], p)} taken={taken} />
            ),
          )}
        </div>
      )}

      <div className="flex flex-wrap items-center gap-2 border-t border-line pt-3 sm:col-span-2">
        {needsTtl && (
          <label className="flex items-center gap-1.5 text-[12px] text-muted">
            Lease
            <input value={ttl} onChange={(e) => setTtl(e.target.value)} inputMode="numeric" className={`${sized} h-7 w-16 font-mono`} aria-label="Lease minutes" />
            minutes
          </label>
        )}
        <span className="text-[11.5px] text-danger">
          {badKey ? `${badKey.p.key || "A key"} isn't a valid key` : dupKey ? `${dupKey.p.key} is picked twice` : !ttlOk ? "A lease is 1 minute to 7 days" : ""}
        </span>
        <span className="ml-auto" />
        <button className={ghost} onClick={onClose}>
          Cancel
        </button>
        <button className={primary} disabled={!chosen.length || !!badKey || !!dupKey || !ttlOk || busy} onClick={submit}>
          {busy ? "Linking…" : `Link ${chosen.length || ""}`.trim()}
        </button>
      </div>
    </div>
  );
}

function groupFields(fields: LinkField[]): { credential?: string; fields: LinkField[] }[] {
  const out: { credential?: string; fields: LinkField[] }[] = [];
  for (const f of fields) {
    const g = f.credential ? out.find((x) => x.credential === f.credential) : undefined;
    if (g) g.fields.push(f);
    else out.push(f.credential ? { credential: f.credential, fields: [f] } : { fields: [f] });
  }
  return out;
}

function ModePicker({ modes, value, onChange }: { modes: LinkMode[]; value: LinkMode; onChange: (m: LinkMode) => void }) {
  if (modes.length === 1)
    return (
      <span className="text-[11px] text-subtle" title={MODE_HELP[modes[0]]}>
        {MODE_LABEL[modes[0]]}
      </span>
    );
  return <Segmented label="Mode" value={value} onChange={onChange} options={modes.map((m) => ({ value: m, label: <span className="px-1 text-[11px]">{MODE_LABEL[m]}</span>, title: MODE_HELP[m] }))} />;
}

function KeyInput({ p, set, taken }: { p: Pick; set: (p: Partial<Pick>) => void; taken: Set<string> }) {
  return (
    <span className="flex min-w-0 flex-1 flex-col">
      <input value={p.key} onChange={(e) => set({ key: e.target.value, on: true })} spellCheck={false} aria-label="Key" className={`${field} h-7 font-mono text-[12px]`} />
      {p.on && taken.has(p.key) && <span className="mt-0.5 text-[10.5px] text-muted">{p.key} exists: it becomes linked</span>}
    </span>
  );
}

function FieldRow({ f, p, set, taken }: { f: LinkField; p: Pick; set: (p: Partial<Pick>) => void; taken: Set<string> }) {
  return (
    <div className={`flex flex-wrap items-center gap-2 rounded-lg border px-2 py-1.5 transition-colors ${p.on ? "border-line-3 bg-panel" : "border-line-2"}`}>
      <label className="flex w-40 min-w-0 items-center gap-2 text-[12px]">
        <input type="checkbox" checked={p.on} onChange={(e) => set({ on: e.target.checked })} className="accent-[var(--accent)]" />
        <span className="min-w-0 leading-tight">
          <span className="block truncate">{f.label}</span>
          <span className="block truncate font-mono text-[10.5px] text-subtle">
            {f.field}
            {f.secret && " · secret"}
          </span>
        </span>
      </label>
      <KeyInput p={p} set={set} taken={taken} />
      <ModePicker modes={f.modes} value={p.mode} onChange={(mode) => set({ mode })} />
    </div>
  );
}

/** One issued credential's fields: linked together, with one mode. */
function CredentialGroup({ name, fields, pick, set, taken }: { name: string; fields: LinkField[]; pick: (f: LinkField) => Pick; set: (f: LinkField, p: Partial<Pick>) => void; taken: Set<string> }) {
  const all = fields.every((f) => pick(f).on);
  const mode = pick(fields[0]).mode;
  const modes = fields[0].modes.filter((m) => fields.every((f) => f.modes.includes(m)));
  return (
    <div className={`rounded-lg border p-2 transition-colors ${fields.some((f) => pick(f).on) ? "border-line-3 bg-panel" : "border-line-2"}`}>
      <div className="mb-1.5 flex flex-wrap items-center gap-2">
        <KeyRound className="size-3.5 text-muted" />
        <span className="text-[12px] font-medium">One credential</span>
        <span className="font-mono text-[10.5px] text-subtle">{name}</span>
        <span className="ml-auto" />
        <ModePicker modes={modes} value={mode} onChange={(m) => fields.forEach((f) => set(f, { mode: m }))} />
        <button onClick={() => fields.forEach((f) => set(f, { on: !all }))} className={`${ghost} h-6`}>
          {all ? "Unlink all" : `Link all ${fields.length}`}
        </button>
      </div>
      <p className="mb-1.5 text-[11px] text-subtle">Linked together, these share one issued credential, and rotating one rotates all.</p>
      <div className="space-y-1">
        {fields.map((f) => {
          const p = pick(f);
          return (
            <div key={f.field} className="flex items-center gap-2">
              <label className="flex w-40 min-w-0 items-center gap-2 text-[12px]">
                <input type="checkbox" checked={p.on} onChange={(e) => set(f, { on: e.target.checked, mode })} className="accent-[var(--accent)]" />
                <span className="min-w-0 truncate">{f.label}</span>
              </label>
              <KeyInput p={p} set={(x) => set(f, { ...x, mode })} taken={taken} />
            </div>
          );
        })}
      </div>
    </div>
  );
}
