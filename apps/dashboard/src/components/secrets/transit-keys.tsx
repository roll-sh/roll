import { Check, KeySquare, Plus, RotateCw, Signature } from "lucide-react";
import { useState } from "react";
import { useSecrets } from "./secrets-page";
import { ActorMark, ago, button, card, ErrorNote, errorText, field, ghost, primary, Section, useLoad } from "./ui";

/**
 * Encryption keys (docs/secrets.md, "Encryption keys"): encryption as a service. Apps encrypt and
 * decrypt through roll (`…/transit/{name}/encrypt`, `roll secrets transit encrypt`) and the key never
 * leaves it. Rotating adds a version new encryptions use; older ones keep decrypting.
 */
export function TransitKeys() {
  const { api, project } = useSecrets();
  const list = useLoad(() => api.transitKeys(), [api]);
  const [adding, setAdding] = useState(false);
  const [name, setName] = useState("");
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState<string | null>(null);
  const ok = /^[A-Za-z0-9_-]{1,64}$/.test(name);
  // The newest version's public key (PEM), for a service that checks `…/sign`'s signatures.
  const copyPublic = (key: string) =>
    run(`public:${key}`, async () => {
      const all = await api.transitPublicKeys(key);
      await navigator.clipboard?.writeText(all[all.length - 1]?.pem ?? "");
      setCopied(key);
      setTimeout(() => setCopied((c) => (c === key ? null : c)), 1500);
    });

  const run = async (what: string, f: () => Promise<unknown>) => {
    setBusy(what);
    setError(null);
    try {
      await f();
      list.reload();
      return true;
    } catch (e) {
      setError(errorText(e));
      return false;
    } finally {
      setBusy(null);
    }
  };

  return (
    <Section
      title="Encryption keys"
      aside={
        <button className={button} onClick={() => setAdding((a) => !a)}>
          <Plus className="size-3.5" /> Add a key
        </button>
      }
    >
      <p className="mb-2 text-[11.5px] text-muted">
        Encryption as a service: apps send data to <span className="font-mono">POST /v1/secrets/projects/{project.id}/transit/NAME/encrypt</span> and keep the ciphertext; decrypting is a reveal, audited.
        The key never leaves roll. It also signs (<span className="font-mono">…/sign</span>, Ed25519: services check with its public key) and makes HMACs for lookups over encrypted fields (<span className="font-mono">…/hmac</span>). Rotate to start a new version; older ciphertexts keep opening until you retire them.
      </p>
      <ErrorNote error={error ?? list.error} onDismiss={error ? () => setError(null) : undefined} className="mb-2" />
      {adding && (
        <form
          className="rise mb-3 flex flex-wrap items-end gap-2 rounded-xl border border-line-2 bg-panel p-3"
          onSubmit={(e) => {
            e.preventDefault();
            if (ok && !busy) void run("add", () => api.addTransitKey(name)).then((done) => done && (setAdding(false), setName("")));
          }}
        >
          <label className="w-56">
            <span className="mb-1 block text-[11px] font-medium text-subtle">Name</span>
            <input autoFocus value={name} onChange={(e) => setName(e.target.value)} placeholder="card-numbers" spellCheck={false} className={`${field} font-mono text-[12px]`} />
          </label>
          <button type="submit" className={primary} disabled={!ok || !!busy}>
            {busy === "add" ? "Making it…" : "Make key"}
          </button>
          <button type="button" className={ghost} onClick={() => setAdding(false)}>
            Cancel
          </button>
        </form>
      )}
      {list.data && list.data.length === 0 && !adding ? (
        <p className="rounded-xl border border-dashed border-line-2 px-3 py-4 text-[12px] text-muted">No encryption keys. Add one to encrypt data for an app without the app holding a key.</p>
      ) : (
        list.data && (
          <div className={`${card} divide-y divide-line`}>
            {list.data.map((k) => (
              <div key={k.id} className="flex flex-wrap items-center gap-x-3 gap-y-1 px-3 py-2">
                <KeySquare className="size-3.5 text-muted" />
                <span className="font-mono text-[12px]">{k.name}</span>
                <span className="font-mono text-[11px] text-subtle">
                  v{k.latest_version}
                  {k.min_decrypt_version > 1 && ` · decrypts from v${k.min_decrypt_version}`}
                </span>
                <span className="ml-auto text-[11px] text-subtle">
                  made {ago(k.created_at)} · <ActorMark who={k.created_by} />
                </span>
                <button className={ghost} disabled={!!busy} onClick={() => void copyPublic(k.name)} aria-label={`Copy ${k.name}'s public key`} title="Copy the public key (PEM) that checks this key's signatures">
                  {copied === k.name ? <Check className="size-3.5 text-ok" /> : <Signature className="size-3.5" />}
                </button>
                <button className={ghost} disabled={!!busy} onClick={() => void run(k.name, () => api.rotateTransitKey(k.name))} aria-label={`Rotate ${k.name}`} title="Rotate: new encryptions use a new version">
                  <RotateCw className={`size-3.5 ${busy === k.name ? "motion-safe:animate-spin" : ""}`} />
                </button>
              </div>
            ))}
          </div>
        )
      )}
    </Section>
  );
}
