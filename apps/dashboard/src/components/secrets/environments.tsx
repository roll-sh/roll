import { ArrowDown, ArrowUp, Bell, GripVertical, KeyRound, Lock, Pencil, Plus, Trash2 } from "lucide-react";
import { useState } from "react";
import { ENV_SLUG, KEY_TYPES, LABEL_PART, SECRET_KEY, secretsHealth, type Environment, type KeyType, type Required } from "@/lib/secrets-api";
import { Select } from "../agents/ui/select";
import { Dialog } from "../agents/ui/dialog";
import { Switch } from "../agents/ui/switch";
import { useSecrets } from "./secrets-page";
import { ago, button, card, dangerButton, ErrorNote, errorText, field, sized, ghost, iconButton, label, primary, Section, until, useLoad } from "./ui";

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

      <RequiredKeys onError={setError} />

      <Retention onError={setError} />

      <Backup />

      <DeploymentKey />

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
      {e.protected && <Notify env={e} onError={onError} />}
      {e.protected && (
        <div className="mt-2 flex flex-wrap items-end gap-2 pl-12">
          <label>
            <span className={label}>Approvals</span>
            <input value={approvals} onChange={(ev) => setApprovals(ev.target.value)} inputMode="numeric" className={`${sized} h-7 w-16 font-mono`} />
          </label>
          <label className="min-w-48 flex-1">
            <span className={label}>Approvers (empty: anyone with admin)</span>
            <input value={approvers} onChange={(ev) => setApprovers(ev.target.value)} placeholder="operator, token:tok_…" className={`${field} h-7 font-mono text-[12px]`} />
          </label>
          <button className={button} disabled={!policyChanged || !Number.isInteger(n) || n < 1} onClick={() => policy({ protected: true, approvals: n, approvers: list })}>
            Save policy
          </button>
        </div>
      )}
      <MaxAge env={e} onError={onError} />
      {renaming && <Rename env={e} onDone={() => setRenaming(false)} onError={onError} />}
    </div>
  );
}

/**
 * Where a protected environment announces new change requests: a Slack or Discord incoming webhook,
 * or any HTTPS URL. The server keeps the URL sealed and shows only its host (`notify`).
 */
function Notify({ env, onError }: { env: Environment; onError: (e: string) => void }) {
  const { api, reloadEnvs } = useSecrets();
  const [editing, setEditing] = useState(false);
  const [url, setUrl] = useState("");
  const [busy, setBusy] = useState(false);
  const ok = /^https:\/\/[^\s/]+/.test(url.trim());
  // The policy is replaced whole: keep its approvals and approvers.
  const save = async (notify_url: string | null) => {
    setBusy(true);
    try {
      await api.policy(env.slug, { protected: true, approvals: Math.max(1, env.approvals), approvers: env.approvers, notify_url });
      setEditing(false);
      setUrl("");
      reloadEnvs();
    } catch (e) {
      onError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="mt-2 pl-12 text-[11.5px] text-muted">
      {editing ? (
        <form
          className="flex flex-wrap items-center gap-1.5"
          onSubmit={(ev) => {
            ev.preventDefault();
            if (ok && !busy) void save(url.trim());
          }}
        >
          <Bell className="size-3.5 shrink-0" />
          <input
            autoFocus
            value={url}
            onChange={(ev) => setUrl(ev.target.value)}
            placeholder="https://hooks.slack.com/services/…"
            aria-label={`Notify on new change requests in ${env.name}`}
            spellCheck={false}
            autoComplete="off"
            className={`${sized} h-7 w-96 max-w-full font-mono text-[12px]`}
          />
          <button type="submit" className={button} disabled={!ok || busy}>
            {busy ? "Saving…" : "Save"}
          </button>
          <button type="button" className={ghost} onClick={() => (setEditing(false), setUrl(""))}>
            Cancel
          </button>
          <span className="basis-full pl-5 text-[11px] text-subtle">A Slack or Discord incoming webhook, or any HTTPS URL. It gets the request&apos;s id, author, message and key names, never values. roll keeps the URL sealed.</span>
        </form>
      ) : env.notify ? (
        <span className="flex flex-wrap items-center gap-1.5">
          <Bell className="size-3.5 text-accent" /> New change requests are announced to <span className="font-mono text-soft">{env.notify}</span>
          <button className={`${ghost} h-6`} onClick={() => setEditing(true)}>
            Change
          </button>
          <button className={`${ghost} h-6`} disabled={busy} onClick={() => save(null)}>
            Stop notifying
          </button>
        </span>
      ) : (
        <button className={`${ghost} -ml-2 h-6`} onClick={() => setEditing(true)}>
          <Bell className="size-3.5" /> Notify on new change requests…
        </button>
      )}
    </div>
  );
}

/** How long old values stay readable: the sweeper destroys older versions' values after that (never a current one). */
const TYPE_LABEL: Record<KeyType, string> = { url: "URL", int: "integer", port: "port", bool: "boolean", json: "JSON", email: "email" };

/** Keys every standard environment must set, each with what its value must be (typed keys). */
function RequiredKeys({ onError }: { onError: (e: string) => void }) {
  const { api, reloadEnvs, required, reloadRequired } = useSecrets();
  const [adding, setAdding] = useState<Required>({ key: "", path: "/", comment: "" });
  const [busy, setBusy] = useState(false);
  const save = async (next: Required[]) => {
    setBusy(true);
    try {
      await api.setRequired(next);
      reloadRequired();
      reloadEnvs();
      return true;
    } catch (e) {
      onError(errorText(e));
      return false;
    } finally {
      setBusy(false);
    }
  };
  const rows = required;
  const keyOk = SECRET_KEY.test(adding.key) && adding.path.startsWith("/");
  return (
    <Section title="Required keys" aside={<span className="text-[11.5px] text-subtle max-sm:hidden">Every standard environment must set these; typed ones must fit.</span>}>
      {rows.length > 0 && (
        <div className={`${card} mb-2 divide-y divide-line`}>
          {rows.map((r) => (
            <div key={`${r.path}:${r.key}`} className="flex flex-wrap items-center gap-x-3 gap-y-1 px-3 py-2 text-[12.5px]">
              <span className="font-mono">{r.key}</span>
              {r.path !== "/" && <span className="font-mono text-[11px] text-subtle">{r.path}</span>}
              {r.type && <span className="rounded border border-line-2 px-1 text-[10.5px] text-muted">{TYPE_LABEL[r.type]}</span>}
              {r.pattern && <code className="truncate rounded border border-line-2 px-1 font-mono text-[10.5px] text-muted" title="The whole value must match">/{r.pattern}/</code>}
              {r.comment && <span className="truncate text-[11.5px] text-subtle">{r.comment}</span>}
              <button className={`${ghost} ml-auto`} disabled={busy} onClick={() => save(rows.filter((x) => x !== r))}>
                Remove
              </button>
            </div>
          ))}
        </div>
      )}
      <form
        className="flex flex-wrap items-end gap-2"
        onSubmit={async (ev) => {
          ev.preventDefault();
          if (!keyOk) return;
          const next = { ...adding, key: adding.key.trim(), ...(adding.pattern?.trim() ? { pattern: adding.pattern.trim() } : { pattern: undefined }) };
          if (await save([...rows.filter((x) => !(x.key === next.key && x.path === next.path)), next])) setAdding({ key: "", path: adding.path, comment: "" });
        }}
      >
        <label className="w-48">
          <span className={label}>Key</span>
          <input value={adding.key} onChange={(e) => setAdding({ ...adding, key: e.target.value.toUpperCase() })} placeholder="PORT" spellCheck={false} className={`${field} h-7 font-mono text-[12px]`} />
        </label>
        <label className="w-28">
          <span className={label}>Folder</span>
          <input value={adding.path} onChange={(e) => setAdding({ ...adding, path: e.target.value })} spellCheck={false} className={`${field} h-7 font-mono text-[12px]`} />
        </label>
        <div>
          <span className={label}>Type</span>
          <Select label="Type" value={adding.type ?? "any"} onChange={(v) => setAdding({ ...adding, type: v === "any" ? undefined : (v as KeyType) })} options={[{ value: "any", label: "any" }, ...KEY_TYPES.map((t) => ({ value: t, label: TYPE_LABEL[t] }))]} className="h-7 w-28" />
        </div>
        <label className="min-w-40 flex-1">
          <span className={label}>Pattern (optional)</span>
          <input value={adding.pattern ?? ""} onChange={(e) => setAdding({ ...adding, pattern: e.target.value })} placeholder="[a-z]{2}-[a-z]+-[0-9]" spellCheck={false} className={`${field} h-7 font-mono text-[12px]`} />
        </label>
        <button type="submit" className={button} disabled={!keyOk || busy}>
          <Plus className="size-3.5" /> Require
        </button>
      </form>
    </Section>
  );
}

/** A backup sealed to a public key the team holds (`roll secrets keygen`): the browser never sees the secret key. */
function Backup() {
  const { api, project } = useSecrets();
  const [recipient, setRecipient] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [done, setDone] = useState(false);
  const ok = /^[A-Za-z0-9_-]{43}$/.test(recipient.trim());
  const download = async () => {
    setBusy(true);
    setError(null);
    try {
      const b = await api.backup(recipient.trim());
      const url = URL.createObjectURL(new Blob([JSON.stringify(b, null, 2)], { type: "application/json" }));
      const a = document.createElement("a");
      a.href = url;
      a.download = `${project.name}-${new Date().toISOString().slice(0, 10)}.backup`;
      a.click();
      URL.revokeObjectURL(url);
      setDone(true);
    } catch (e) {
      setError(errorText(e));
    }
    setBusy(false);
  };
  return (
    <Section title="Backup" aside={<span className="text-[11.5px] text-subtle max-sm:hidden">Every environment, value, link and policy, sealed to your key.</span>}>
      <form
        className="flex flex-wrap items-end gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          if (ok && !busy) void download();
        }}
      >
        <label className="min-w-72 flex-1">
          <span className={label}>Public key (from roll secrets keygen)</span>
          <input value={recipient} onChange={(e) => setRecipient(e.target.value)} placeholder="base64url, 43 characters" spellCheck={false} className={`${field} h-7 font-mono text-[12px]`} aria-invalid={recipient.trim() !== "" && !ok} />
        </label>
        <button type="submit" className={button} disabled={!ok || busy}>
          {busy ? "Sealing…" : "Download backup"}
        </button>
      </form>
      <ErrorNote error={error} onDismiss={() => setError(null)} className="mt-2" />
      <p className="mt-1.5 text-[11.5px] text-muted">
        Only the matching secret key opens it, so the file is safe to keep anywhere.{done ? " Downloaded." : ""} Restore into an empty project with{" "}
        <code className="font-mono text-[11px]">roll secrets -p &lt;project&gt; restore {project.name}.backup --key &lt;secret key file&gt;</code>.
      </p>
    </Section>
  );
}

const KEY_SOURCE: Record<string, string> = {
  kms: "AWS KMS: the key is decrypted at startup and never stored (ROLL_SECRET_KEY_KMS)",
  env: "ROLL_SECRET_KEY on every node",
  file: "a key file this node generated (secrets.key): back it up, or set ROLL_SECRET_KEY",
  missing: "none: secrets are unavailable until ROLL_SECRET_KEY is set",
};

/** Where the deployment key that seals every project's data key comes from, and a rotation's progress. */
function DeploymentKey() {
  const health = useLoad(() => secretsHealth(), []);
  const h = health.data;
  if (!h) return null;
  const warn = h.key === "file" || h.key === "missing";
  return (
    <Section title="Deployment key" aside={<span className="text-[11.5px] text-subtle max-sm:hidden">For the whole server: it seals every project&apos;s data key.</span>}>
      <div className={`${card} flex flex-wrap items-center gap-x-3 gap-y-1 px-3 py-2 text-[12.5px]`}>
        <KeyRound className={`size-3.5 ${warn ? "text-danger" : "text-muted"}`} />
        <span className={warn ? "text-danger" : ""}>{KEY_SOURCE[h.key] ?? h.key}</span>
        {h.resealed !== null && (
          <span className="ml-auto text-[11.5px] text-subtle">
            {h.resealed === "all" ? "Rotation done: the previous key can be dropped" : `Rotating: ${h.resealed} data key${h.resealed === 1 ? "" : "s"} resealed so far`}
          </span>
        )}
      </div>
    </Section>
  );
}

function Retention({ onError }: { onError: (e: string) => void }) {
  const { api, historyDays, reloadEnvs } = useSecrets();
  const [days, setDays] = useState(historyDays?.toString() ?? "");
  const [seen, setSeen] = useState(historyDays);
  if (seen !== historyDays) {
    setSeen(historyDays);
    setDays(historyDays?.toString() ?? "");
  }
  const [busy, setBusy] = useState(false);
  const n = Number(days);
  const ok = days === "" || (Number.isInteger(n) && n >= 1 && n <= 3650);
  const next = days === "" ? null : n;
  return (
    <Section title="Old values" aside={<span className="text-[11.5px] text-subtle">For every environment of the project.</span>}>
      <form
        className="flex flex-wrap items-center gap-2 text-[12.5px]"
        onSubmit={async (ev) => {
          ev.preventDefault();
          if (!ok || next === historyDays) return;
          setBusy(true);
          try {
            await api.setHistoryDays(next);
            reloadEnvs();
          } catch (e) {
            onError(errorText(e));
          } finally {
            setBusy(false);
          }
        }}
      >
        <label className="flex items-center gap-2">
          Keep old values for
          <input value={days} onChange={(ev) => setDays(ev.target.value.trim())} inputMode="numeric" placeholder="∞" aria-label="Days to keep old values" className={`${sized} h-7 w-20 text-center font-mono`} />
          days
        </label>
        <span className="text-[11.5px] text-subtle">{days === "" ? "(blank: forever)" : ""}</span>
        {next !== historyDays && ok && (
          <button type="submit" className={button} disabled={busy}>
            {busy ? "Saving…" : next === null ? "Keep forever" : "Save"}
          </button>
        )}
        {!ok && <span className="text-[11px] text-danger">1–3650 days, or blank for forever</span>}
      </form>
      <p className="mt-1.5 text-[11.5px] text-muted">
        After that, an old version&apos;s value is destroyed for good: history still shows the change, but it can&apos;t be revealed or rolled back to. A secret&apos;s current value is never removed.
      </p>
    </Section>
  );
}

/** "Flag keys unchanged for N days": insights mark them stale (only a flag; nothing stops). */
function MaxAge({ env, onError }: { env: Environment; onError: (e: string) => void }) {
  const { api, reloadEnvs } = useSecrets();
  const [days, setDays] = useState(env.max_age_days?.toString() ?? "");
  const [busy, setBusy] = useState(false);
  const n = Number(days);
  const ok = days === "" || (Number.isInteger(n) && n >= 1 && n <= 3650);
  const next = days === "" ? null : n;
  const save = async (v: number | null) => {
    setBusy(true);
    try {
      await api.updateEnvironment(env.slug, { max_age_days: v });
      setDays(v?.toString() ?? "");
      reloadEnvs();
    } catch (e) {
      onError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <form
      className="mt-1.5 flex flex-wrap items-center gap-1.5 pl-12 text-[11.5px] text-muted"
      onSubmit={(ev) => {
        ev.preventDefault();
        if (ok && next !== env.max_age_days) void save(next);
      }}
    >
      <label className="flex items-center gap-1.5" title="Insights flag a key whose value hasn't changed for longer; nothing stops using it">
        Flag keys unchanged for
        <input value={days} onChange={(ev) => setDays(ev.target.value.trim())} inputMode="numeric" placeholder="—" aria-label={`Days before a key in ${env.name} is flagged stale`} className={`${sized} h-6 w-14 px-1.5 text-center font-mono text-[11.5px]`} />
        days
      </label>
      {next !== env.max_age_days && ok && (
        <button type="submit" className={`${button} h-6`} disabled={busy}>
          {next === null ? "Turn off" : "Save"}
        </button>
      )}
      {!ok && <span className="text-[11px] text-danger">1–3650 days, or blank for never</span>}
      {env.max_age_days === null && days === "" && <span className="text-subtle">(off)</span>}
    </form>
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
