import { Check, Copy, KeyRound, Pencil, Plus, RotateCw, Trash2 } from "lucide-react";
import { useState } from "react";
import type { Rotator } from "@/lib/secrets-api";
import { useSecrets } from "./secrets-page";
import { ActorMark, ago, button, card, dangerButton, ErrorNote, errorText, field, ghost, primary, Section, useLoad } from "./ui";

/**
 * Rotators (docs/secrets.md, "Rotators"): URLs roll calls to issue and revoke values that live
 * outside roll. A key links to one (Link a resource) in managed mode and rotates on its schedule.
 */
export function Rotators() {
  const { api } = useSecrets();
  const list = useLoad(() => api.rotators(), [api]);
  const [adding, setAdding] = useState(false);
  const [name, setName] = useState("");
  const [url, setUrl] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [made, setMade] = useState<{ name: string; secret: string; rekeyed?: boolean } | null>(null);
  const [rekeying, setRekeying] = useState<string | null>(null);
  const [editing, setEditing] = useState<{ id: string; name: string; url: string } | null>(null);
  const [copied, setCopied] = useState(false);
  const [deleting, setDeleting] = useState<string | null>(null);
  const ok = /^[A-Za-z0-9_-]{1,64}$/.test(name) && /^https:\/\/[^\s/]+/.test(url.trim());

  const add = async () => {
    setBusy(true);
    setError(null);
    try {
      const r = await api.addRotator({ name, url: url.trim() });
      setMade({ name: r.name, secret: r.secret });
      setAdding(false);
      setName("");
      setUrl("");
      list.reload();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  const rekey = (r: Rotator) => {
    setBusy(true);
    setError(null);
    api.rotateRotatorSecret(r.id).then(
      (x) => (setRekeying(null), setBusy(false), setMade({ name: x.name, secret: x.secret, rekeyed: true })),
      (e) => (setRekeying(null), setBusy(false), setError(errorText(e))),
    );
  };
  const editOk = !!editing && /^[A-Za-z0-9_-]{1,64}$/.test(editing.name) && (editing.url.trim() === "" || /^https:\/\/[^\s/]+/.test(editing.url.trim()));
  const save = (r: Rotator) => {
    if (!editing || !editOk) return;
    setBusy(true);
    setError(null);
    api.updateRotator(r.id, { ...(editing.name !== r.name && { name: editing.name }), ...(editing.url.trim() && { url: editing.url.trim() }) }).then(
      () => (setEditing(null), setBusy(false), list.reload()),
      (e) => (setBusy(false), setError(errorText(e))),
    );
  };
  const del = (r: Rotator) =>
    api.deleteRotator(r.id).then(
      () => (setDeleting(null), list.reload()),
      (e) => (setDeleting(null), setError(errorText(e))),
    );

  return (
    <Section
      title="Rotators"
      aside={
        <button className={button} onClick={() => setAdding((a) => !a)}>
          <Plus className="size-3.5" /> Add a rotator
        </button>
      }
    >
      <p className="mb-2 text-[11.5px] text-muted">
        For values that live outside roll, such as a vendor&apos;s API key: roll calls your HTTPS endpoint to issue a new one and, after the grace period, to revoke the old one. Link a key to a rotator from Link a resource; it rotates on its schedule.
      </p>
      <ErrorNote error={error ?? list.error} onDismiss={error ? () => setError(null) : undefined} className="mb-2" />
      {made && (
        <div className="fade-in mb-3 rounded-xl border border-accent/40 bg-panel p-3 shadow-card inset-shadow-highlight" role="status">
          <p className="text-[12.5px]">
            <b className="font-medium">{made.name}</b>{" "}
            <span className="text-muted">
              {made.rekeyed
                ? "signs with a new secret from now on. Copy it into your endpoint now: it isn't shown again (a call your endpoint refuses meanwhile fails, changes nothing, and is retried)."
                : "is added. Copy its signing secret now: it isn't shown again."}
            </span>
          </p>
          <div className="mt-2 flex items-center gap-2">
            <code aria-label="Signing secret" className="min-w-0 flex-1 truncate rounded-md border border-line-2 bg-panel-2 px-2 py-1.5 font-mono text-[12px] select-all">
              {made.secret}
            </code>
            <button
              className={button}
              onClick={() => {
                void navigator.clipboard?.writeText(made.secret);
                setCopied(true);
                setTimeout(() => setCopied(false), 1200);
              }}
            >
              {copied ? <Check className="size-3.5 text-ok" /> : <Copy className="size-3.5" />} Copy
            </button>
            <button className={ghost} onClick={() => setMade(null)}>
              Done
            </button>
          </div>
          <p className="mt-2 text-[11px] text-subtle">
            Each call is a signed POST (<span className="font-mono">X-Roll-Signature-256</span>, as for streams): <span className="font-mono">{`{"action": "issue"}`}</span> answers{" "}
            <span className="font-mono">{`{"value": "…", "handle": …}`}</span>; <span className="font-mono">{`{"action": "revoke", "handle": …}`}</span> answers any 2xx.
          </p>
        </div>
      )}
      {adding && (
        <form
          className="rise mb-3 flex flex-wrap items-end gap-2 rounded-xl border border-line-2 bg-panel p-3"
          onSubmit={(e) => {
            e.preventDefault();
            if (ok && !busy) void add();
          }}
        >
          <label className="w-44">
            <span className="mb-1 block text-[11px] font-medium text-subtle">Name</span>
            <input autoFocus value={name} onChange={(e) => setName(e.target.value)} placeholder="stripe" spellCheck={false} className={`${field} font-mono text-[12px]`} />
          </label>
          <label className="min-w-64 flex-1">
            <span className="mb-1 block text-[11px] font-medium text-subtle">HTTPS endpoint (kept sealed)</span>
            <input value={url} onChange={(e) => setUrl(e.target.value)} placeholder="https://rotate.example.com/stripe" spellCheck={false} className={`${field} font-mono text-[12px]`} />
          </label>
          <button type="submit" className={primary} disabled={!ok || busy}>
            {busy ? "Adding…" : "Add rotator"}
          </button>
          <button type="button" className={ghost} onClick={() => setAdding(false)}>
            Cancel
          </button>
        </form>
      )}
      {list.data && list.data.length === 0 && !adding ? (
        <p className="rounded-xl border border-dashed border-line-2 px-3 py-4 text-[12px] text-muted">No rotators. Add one to have roll rotate a key whose value comes from outside roll.</p>
      ) : (
        list.data && (
          <div className={`${card} divide-y divide-line`}>
            {list.data.map((r) => (
              <div key={r.id} className="flex flex-wrap items-center gap-x-3 gap-y-1 px-3 py-2">
                <RotateCw className="size-3.5 text-muted" />
                <span className="font-mono text-[12px]">{r.name}</span>
                <span className="font-mono text-[11.5px] text-subtle">{r.host}</span>
                <span className="ml-auto text-[11px] text-subtle">
                  added {ago(r.created_at)} · <ActorMark who={r.created_by} />
                </span>
                {rekeying === r.id ? (
                  <span className="flex gap-1">
                    <button className={primary} disabled={busy} onClick={() => rekey(r)}>
                      {busy ? "Rotating…" : "New signing secret"}
                    </button>
                    <button className={ghost} onClick={() => setRekeying(null)}>
                      Cancel
                    </button>
                  </span>
                ) : deleting === r.id ? (
                  <span className="flex gap-1">
                    <button className={dangerButton} onClick={() => del(r)}>
                      Delete
                    </button>
                    <button className={ghost} onClick={() => setDeleting(null)}>
                      Keep
                    </button>
                  </span>
                ) : (
                  <span className="flex gap-0.5">
                    <button className={ghost} onClick={() => setRekeying(r.id)} aria-label={`Rotate ${r.name}'s signing secret`} title="Rotate the signing secret">
                      <KeyRound className="size-3.5" />
                    </button>
                    <button className={ghost} onClick={() => setEditing(editing?.id === r.id ? null : { id: r.id, name: r.name, url: "" })} aria-label={`Edit the rotator ${r.name}`} title="Edit">
                      <Pencil className="size-3.5" />
                    </button>
                    <button className={ghost} onClick={() => setDeleting(r.id)} aria-label={`Delete the rotator ${r.name}`}>
                      <Trash2 className="size-3.5" />
                    </button>
                  </span>
                )}
                {editing?.id === r.id && (
                  <form
                    className="rise grid w-full gap-2 pt-1 sm:grid-cols-[11rem_1fr_auto]"
                    onSubmit={(e) => {
                      e.preventDefault();
                      save(r);
                    }}
                  >
                    <input value={editing.name} onChange={(e) => setEditing({ ...editing, name: e.target.value })} aria-label="Name" spellCheck={false} className={`${field} font-mono text-[12px]`} />
                    <input value={editing.url} onChange={(e) => setEditing({ ...editing, url: e.target.value })} placeholder="A new HTTPS endpoint; empty keeps it" aria-label="Endpoint" spellCheck={false} autoComplete="off" className={`${field} font-mono text-[12px]`} />
                    <span className="flex gap-1">
                      <button type="submit" className={primary} disabled={!editOk || busy}>
                        Save
                      </button>
                      <button type="button" className={ghost} onClick={() => setEditing(null)}>
                        Cancel
                      </button>
                    </span>
                  </form>
                )}
              </div>
            ))}
          </div>
        )
      )}
    </Section>
  );
}
