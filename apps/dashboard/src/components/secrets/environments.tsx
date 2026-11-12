import { ArrowDown, ArrowUp, GripVertical, Lock, Pencil, Plus, Trash2 } from "lucide-react";
import { useState } from "react";
import { ENV_SLUG, LABEL_PART, type Environment } from "@/lib/secrets-api";
import { Select } from "../agents/ui/select";
import { Dialog } from "../agents/ui/dialog";
import { Switch } from "../agents/ui/switch";
import { useSecrets } from "./secrets-page";
import { ago, button, card, dangerButton, ErrorNote, errorText, field, sized, ghost, iconButton, label, primary, Section, until } from "./ui";

/**
 * The project's environments: add, rename (references to the slug are rewritten), reorder (the
 * overview's columns and the promotion order), protect with approvals, delete; and the preview
 * environments, which inherit from a base and expire.
 */
export function EnvironmentsView() {
  const { api, standard, previews, reloadEnvs, project } = useSecrets();
  const [error, setError] = useState<string | null>(null);
  const [order, setOrder] = useState<string[] | null>(null);
  const [drag, setDrag] = useState<string | null>(null);
  const [deleting, setDeleting] = useState<Environment | null>(null);
  const slugs = order ?? standard.map((e) => e.slug);
  const bySlug = new Map(standard.map((e) => [e.slug, e]));
  const rows = slugs.map((s) => bySlug.get(s)).filter((e): e is Environment => !!e);

  const act = (p: Promise<unknown>) =>
    p.then(
      () => {
        setError(null);
        reloadEnvs();
      },
      (e) => setError(errorText(e)),
    );
  const saveOrder = (next: string[]) => {
    setOrder(next);
    api.order(next).then(
      () => {
        reloadEnvs();
        setOrder(null);
      },
      (e) => {
        setOrder(null);
        setError(errorText(e));
      },
    );
  };
  const move = (slug: string, by: number) => {
    const i = slugs.indexOf(slug);
    const j = i + by;
    if (j < 0 || j >= slugs.length) return;
    const next = [...slugs];
    [next[i], next[j]] = [next[j], next[i]];
    saveOrder(next);
  };

  return (
    <div className="mx-auto max-w-4xl space-y-8 px-4 py-5">
      <ErrorNote error={error} onDismiss={() => setError(null)} />
      <Section title="Environments" aside={<span className="text-[11.5px] text-subtle">The order is the overview's columns and the way promotions go.</span>}>
        <div className={`${card} divide-y divide-line`}>
          {rows.map((e, i) => (
            <div
              key={e.slug}
              draggable
              onDragStart={(ev) => {
                setDrag(e.slug);
                ev.dataTransfer.effectAllowed = "move";
              }}
              onDragOver={(ev) => {
                if (!drag || drag === e.slug) return;
                ev.preventDefault();
                const next = slugs.filter((s) => s !== drag);
                next.splice(next.indexOf(e.slug) + (slugs.indexOf(drag) < i ? 1 : 0), 0, drag);
                setOrder(next);
              }}
              onDragEnd={() => {
                if (drag && order) saveOrder(order);
                setDrag(null);
              }}
              className={`transition-opacity ${drag === e.slug ? "opacity-50" : ""}`}
            >
              <EnvRow env={e} first={i === 0} last={i === rows.length - 1} onMove={(by) => move(e.slug, by)} onDelete={() => setDeleting(e)} onError={setError} />
            </div>
          ))}
        </div>
        <AddEnvironment onAdd={(slug, name) => act(api.createEnvironment({ slug, name: name || undefined }))} />
      </Section>

      <Previews previews={previews} onError={setError} onChanged={reloadEnvs} />

      <DeleteEnvironment
        env={deleting}
        project={project.name}
        onClose={() => setDeleting(null)}
        onDelete={(e, confirm) =>
          api.deleteEnvironment(e.slug, confirm).then(() => {
            setDeleting(null);
            reloadEnvs();
          })
        }
      />
    </div>
  );
}

function EnvRow({ env: e, first, last, onMove, onDelete, onError }: { env: Environment; first: boolean; last: boolean; onMove: (by: number) => void; onDelete: () => void; onError: (e: string) => void }) {
  const { api, reloadEnvs } = useSecrets();
  const [renaming, setRenaming] = useState(false);
  const [approvals, setApprovals] = useState(String(Math.max(1, e.approvals)));
  const [approvers, setApprovers] = useState(e.approvers.join(", "));
  const policy = (p: { protected: boolean; approvals?: number; approvers?: string[] }) => api.policy(e.slug, p).then(reloadEnvs, (err) => onError(errorText(err)));
  const n = Number(approvals);
  const list = approvers.split(/[\s,]+/).filter(Boolean);
  const policyChanged = e.protected && (n !== Math.max(1, e.approvals) || list.join(",") !== e.approvers.join(","));
  return (
    <div className="px-3 py-2.5">
      <div className="flex flex-wrap items-center gap-2">
        <GripVertical className="size-3.5 cursor-grab text-faint" aria-hidden />
        <span className="flex flex-col">
          <button className={`${iconButton} h-3.5`} disabled={first} onClick={() => onMove(-1)} aria-label={`Move ${e.name} up`}>
            <ArrowUp className="size-3" />
          </button>
          <button className={`${iconButton} h-3.5`} disabled={last} onClick={() => onMove(1)} aria-label={`Move ${e.name} down`}>
            <ArrowDown className="size-3" />
          </button>
        </span>
        <span className="min-w-0 flex-1 leading-tight">
          <span className="flex items-center gap-1.5 text-[13px] font-medium">
            {e.protected && <Lock className="size-3 text-muted" />}
            {e.name}
          </span>
          <span className="font-mono text-[11px] text-subtle">
            {e.slug} · {e.secrets} {e.secrets === 1 ? "secret" : "secrets"}
          </span>
        </span>
        <label className="flex items-center gap-2 text-[12px] text-muted" title="Writes to a protected environment open a change request that needs approval">
          Protected
          <Switch size="sm" checked={e.protected} onCheckedChange={(on) => policy({ protected: on, approvals: on ? Math.max(1, e.approvals) : undefined })} />
        </label>
        <button className={iconButton} onClick={() => setRenaming((r) => !r)} aria-label={`Rename ${e.name}`} title="Rename">
          <Pencil className="size-3.5" />
        </button>
        <button className={iconButton} onClick={onDelete} aria-label={`Delete ${e.name}`} title="Delete">
          <Trash2 className="size-3.5" />
        </button>
      </div>
      {e.protected && (
        <div className="mt-2 flex flex-wrap items-end gap-2 pl-12">
          <label>
            <span className={label}>Approvals</span>
            <input value={approvals} onChange={(ev) => setApprovals(ev.target.value)} inputMode="numeric" className={`${sized} h-7 w-16 font-mono`} />
          </label>
          <label className="min-w-48 flex-1">
            <span className={label}>Approvers (empty: anyone with admin)</span>
            <input value={approvers} onChange={(ev) => setApprovers(ev.target.value)} placeholder="operator, token:rls_…" className={`${field} h-7 font-mono text-[12px]`} />
          </label>
          <button className={button} disabled={!policyChanged || !Number.isInteger(n) || n < 1} onClick={() => policy({ protected: true, approvals: n, approvers: list })}>
            Save policy
          </button>
        </div>
      )}
      {renaming && <Rename env={e} onDone={() => setRenaming(false)} onError={onError} />}
    </div>
  );
}

function Rename({ env, onDone, onError }: { env: Environment; onDone: () => void; onError: (e: string) => void }) {
  const { api, reloadEnvs, go, search } = useSecrets();
  const [slug, setSlug] = useState(env.slug);
  const [name, setName] = useState(env.name);
  const [busy, setBusy] = useState(false);
  const ok = ENV_SLUG.test(slug) && name.trim().length > 0 && name.length <= 48;
  const changed = slug !== env.slug || name !== env.name;
  return (
    <form
      className="fade-in mt-2.5 rounded-lg border border-line-2 bg-panel-2/50 p-2.5 pl-12"
      onSubmit={async (ev) => {
        ev.preventDefault();
        if (!ok || !changed) return;
        setBusy(true);
        try {
          await api.updateEnvironment(env.slug, { ...(slug !== env.slug && { slug }), ...(name !== env.name && { name: name.trim() }) });
          reloadEnvs();
          if (search.env === env.slug) go({ ...search, env: slug }, true);
          onDone();
        } catch (e) {
          onError(errorText(e));
        } finally {
          setBusy(false);
        }
      }}
    >
      <div className="flex flex-wrap items-end gap-2">
        <label className="w-44">
          <span className={label}>Name</span>
          <input autoFocus value={name} onChange={(e) => setName(e.target.value)} maxLength={48} className={`${field} h-7`} />
        </label>
        <label className="w-44">
          <span className={label}>Slug</span>
          <input value={slug} onChange={(e) => setSlug(e.target.value.toLowerCase())} maxLength={32} className={`${field} h-7 font-mono`} />
        </label>
        <button type="submit" className={primary} disabled={!ok || !changed || busy}>
          {busy ? "Saving…" : "Rename"}
        </button>
        <button type="button" className={ghost} onClick={onDone}>
          Cancel
        </button>
      </div>
      {slug !== env.slug && (
        <p className="mt-2 text-[11.5px] text-muted">
          Every reference to it in the project is rewritten in the same step: <span className="font-mono text-soft">{`\${${env.slug}.KEY}`}</span> becomes{" "}
          <span className="font-mono text-soft">{`\${${slug || "…"}.KEY}`}</span>. The CLI's <span className="font-mono">-e {env.slug}</span> and tokens scoped to it need the new slug.
        </p>
      )}
      {!ENV_SLUG.test(slug) && <p className="mt-1 text-[11px] text-danger">1–32 characters of a-z, 0-9 and -, not starting or ending with -.</p>}
    </form>
  );
}

function AddEnvironment({ onAdd }: { onAdd: (slug: string, name: string) => Promise<unknown> }) {
  const [slug, setSlug] = useState("");
  const [name, setName] = useState("");
  const ok = ENV_SLUG.test(slug);
  return (
    <form
      className="mt-3 flex flex-wrap items-end gap-2"
      onSubmit={(e) => {
        e.preventDefault();
        if (ok) onAdd(slug, name.trim()).then(() => (setSlug(""), setName("")));
      }}
    >
      <label className="w-44">
        <span className={label}>Slug</span>
        <input value={slug} onChange={(e) => setSlug(e.target.value.toLowerCase())} placeholder="qa" maxLength={32} className={`${field} h-7 font-mono`} />
      </label>
      <label className="w-52">
        <span className={label}>Name (optional)</span>
        <input value={name} onChange={(e) => setName(e.target.value)} placeholder="QA" maxLength={48} className={`${field} h-7`} />
      </label>
      <button type="submit" className={button} disabled={!ok}>
        <Plus className="size-3.5" /> Add environment
      </button>
    </form>
  );
}

function DeleteEnvironment({ env, project, onClose, onDelete }: { env: Environment | null; project: string; onClose: () => void; onDelete: (e: Environment, confirm?: string) => Promise<void> }) {
  const [typed, setTyped] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [kept, setKept] = useState<Environment | null>(env);
  if (env && env !== kept) setKept(env);
  const e = env ?? kept;
  const needType = !!e?.protected;
  return (
    <Dialog
      open={!!env}
      onClose={onClose}
      onExited={() => (setTyped(""), setError(null), setKept(null))}
      busy={busy}
      title={e ? `Delete ${e.name}?` : ""}
      lead={e ? `Its ${e.secrets} ${e.secrets === 1 ? "secret" : "secrets"}, folders, history, requests and leases go with it (leases are revoked first). This can't be undone.` : undefined}
      footer={
        <>
          <button className={ghost} onClick={onClose}>
            Cancel
          </button>
          <button
            className={dangerButton}
            disabled={busy || (needType && typed !== e?.slug)}
            onClick={async () => {
              if (!e) return;
              setBusy(true);
              setError(null);
              try {
                await onDelete(e, needType ? typed : undefined);
              } catch (err) {
                setError(errorText(err));
              } finally {
                setBusy(false);
              }
            }}
          >
            {busy ? "Deleting…" : "Delete environment"}
          </button>
        </>
      }
    >
      {needType && e && (
        <label className="block">
          <span className={label}>
            {e.name} is protected. Type <span className="font-mono text-fg">{e.slug}</span> to delete it from {project}.
          </span>
          <input autoFocus value={typed} onChange={(ev) => setTyped(ev.target.value)} className={`${field} font-mono`} />
        </label>
      )}
      <ErrorNote error={error} className="mt-2" />
    </Dialog>
  );
}

const UNITS = { minutes: 1, hours: 60, days: 1440 } as const;
type Unit = keyof typeof UNITS;
const MAX_LABELS = 16;

/** `pr=117, change=chg_42` → the labels, or what's wrong with them. */
function parseLabels(text: string): { labels: Record<string, string> } | { error: string } {
  const labels: Record<string, string> = {};
  for (const part of text.split(/[\s,]+/).filter(Boolean)) {
    const [k, v, ...rest] = part.split("=");
    if (rest.length || v === undefined || !LABEL_PART.test(k) || !LABEL_PART.test(v)) return { error: `${part}: labels are key=value, each 1–64 characters of a-z, 0-9, _, . and -` };
    labels[k] = v;
  }
  return Object.keys(labels).length > MAX_LABELS ? { error: `At most ${MAX_LABELS} labels` } : { labels };
}

/** A preview environment by hand: a slug, the standard environment it reads the rest from, labels and an expiry. */
function NewPreview({ onCreated, onError }: { onCreated: (e: Environment) => void; onError: (e: string) => void }) {
  const { api, standard } = useSecrets();
  const [slug, setSlug] = useState("");
  const [base, setBase] = useState<string | null>(null);
  const [labels, setLabels] = useState("");
  const [ttl, setTtl] = useState("1");
  const [unit, setUnit] = useState<Unit>("days");
  const [busy, setBusy] = useState(false);
  const from = base ?? standard.find((e) => e.slug === "staging")?.slug ?? standard[0]?.slug ?? null;
  const parsed = parseLabels(labels);
  const n = ttl.trim() === "" ? null : Number(ttl);
  const ttlOk = n === null || (Number.isInteger(n) && n > 0);
  const slugOk = ENV_SLUG.test(slug);
  const ok = slugOk && !!from && !("error" in parsed) && ttlOk;
  return (
    <form
      className="mt-3 rounded-xl border border-line-2 p-3"
      aria-label="New preview environment"
      onSubmit={async (ev) => {
        ev.preventDefault();
        if (!ok || busy || "error" in parsed || !from) return;
        setBusy(true);
        try {
          const e = await api.createEnvironment({ slug, kind: "ephemeral", base: from, labels: parsed.labels, ttl_minutes: n === null ? undefined : n * UNITS[unit] });
          setSlug("");
          setLabels("");
          onCreated(e);
        } catch (err) {
          onError(errorText(err));
        } finally {
          setBusy(false);
        }
      }}
    >
      <div className="flex flex-wrap items-end gap-2">
        <label className="w-36">
          <span className={label}>Slug</span>
          <input value={slug} onChange={(e) => setSlug(e.target.value.toLowerCase())} placeholder="pr-117" maxLength={32} spellCheck={false} className={`${field} h-7 font-mono`} />
        </label>
        <div className="w-40">
          <span className={label}>Base</span>
          <Select label="Base" value={from} onChange={setBase} options={standard.map((e) => ({ value: e.slug, label: e.name }))} className="h-7 w-40" />
        </div>
        <label className="min-w-40 flex-1">
          <span className={label}>Labels</span>
          <input value={labels} onChange={(e) => setLabels(e.target.value)} placeholder="pr=117, change=chg_42" spellCheck={false} className={`${field} h-7 font-mono text-[12px]`} />
        </label>
        <div>
          <span className={label}>Expires after (blank: never)</span>
          <span className="flex gap-1">
            <input value={ttl} onChange={(e) => setTtl(e.target.value)} inputMode="numeric" aria-label="Expires after" className={`${sized} h-7 w-14 font-mono`} />
            <Select label="Unit" value={unit} onChange={setUnit} options={(Object.keys(UNITS) as Unit[]).map((u) => ({ value: u, label: u }))} className="h-7 w-24" />
          </span>
        </div>
        <button type="submit" className={button} disabled={!ok || busy}>
          <Plus className="size-3.5" /> {busy ? "Creating…" : "Create preview"}
        </button>
      </div>
      {slug && !slugOk && <p className="mt-1.5 text-[11px] text-danger">1–32 characters of a-z, 0-9 and -, not starting or ending with -.</p>}
      {"error" in parsed && <p className="mt-1.5 text-[11px] text-danger">{parsed.error}</p>}
      {!ttlOk && <p className="mt-1.5 text-[11px] text-danger">A whole number above 0, or blank to keep it until it's deleted.</p>}
      <p className="mt-1.5 text-[11px] text-subtle">It stores only what you set in it and reads every other key from {from ?? "its base"} at the same path.</p>
    </form>
  );
}

function Previews({ previews, onError, onChanged }: { previews: Environment[]; onError: (e: string) => void; onChanged: () => void }) {
  const { api, project, go } = useSecrets();
  const [created, setCreated] = useState<string | null>(null);
  const [selector, setSelector] = useState("");
  const [result, setResult] = useState<string | null>(null);
  const labelOk = /^[a-z0-9_.-]+=[a-z0-9_.-]+$/.test(selector);
  return (
    <Section title={`Previews ${previews.length ? previews.length : ""}`.trim()} aside={<span className="text-[11.5px] text-subtle">Ephemeral: they store only overrides and read the rest from their base.</span>}>
      {previews.length === 0 ? (
        <p className="rounded-xl border border-dashed border-line-2 px-3 py-4 text-[12px] text-muted">
          No preview environments. roll previews makes one per change or pull request; make one below, or from the CLI:{" "}
          <span className="font-mono text-soft">roll secrets envs add pr-117 -p {project.name} --base staging --ephemeral</span>
        </p>
      ) : (
        <div className={`${card} divide-y divide-line`}>
          {previews.map((e) => (
            <div key={e.slug} className="flex flex-wrap items-center gap-x-3 gap-y-1 px-3 py-2">
              <button onClick={() => go({ env: e.slug })} className={`font-mono text-[12.5px] hover:text-accent hover:underline ${created === e.slug ? "text-accent" : ""}`} title={`Open ${e.slug}`}>
                {e.slug}
              </button>
              <span className="text-[11.5px] text-muted">
                from <b className="font-medium text-soft">{e.base}</b>
              </span>
              <span className="flex flex-wrap gap-1">
                {Object.entries(e.labels).map(([k, v]) => (
                  <span key={k} className="rounded border border-line-2 px-1 font-mono text-[10.5px] text-subtle">
                    {k}={v}
                  </span>
                ))}
              </span>
              <span className="ml-auto font-mono text-[11px] text-subtle">
                {e.secrets} {e.secrets === 1 ? "override" : "overrides"} · {e.expires_at ? `expires ${until(e.expires_at)}` : "no expiry"} · {ago(e.created_at)}
              </span>
              <button className={iconButton} aria-label={`Delete ${e.slug}`} onClick={() => api.deleteEnvironment(e.slug).then(onChanged, (err) => onError(errorText(err)))}>
                <Trash2 className="size-3.5" />
              </button>
            </div>
          ))}
        </div>
      )}
      <NewPreview
        onError={onError}
        onCreated={(e) => {
          setCreated(e.slug);
          onChanged();
        }}
      />
      {previews.length > 0 && (
        <form
          className="mt-3 flex flex-wrap items-center gap-2"
          onSubmit={(ev) => {
            ev.preventDefault();
            if (!labelOk) return;
            api.deleteByLabel(selector).then(
              (r) => {
                setResult(r.deleted.length ? `Deleted ${r.deleted.join(", ")}` : "No preview has that label");
                setSelector("");
                onChanged();
              },
              (err) => onError(errorText(err)),
            );
          }}
        >
          <input value={selector} onChange={(e) => setSelector(e.target.value)} placeholder="pr=117" aria-label="Label" className={`${sized} h-7 w-44 font-mono`} />
          <button className={button} disabled={!labelOk}>
            Delete previews with this label
          </button>
          {result && <span className="text-[11.5px] text-muted">{result}</span>}
        </form>
      )}
    </Section>
  );
}
