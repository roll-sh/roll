import { Check, Copy } from "lucide-react";
import { useState } from "react";
import type { Share } from "@/lib/secrets-api";
import { sealShare, shareLink } from "@/lib/secrets-share";
import { Dialog } from "../agents/ui/dialog";
import { Select } from "../agents/ui/select";
import { useSecrets } from "./secrets-page";
import { button, ErrorNote, errorText, field, ghost, label, primary, until } from "./ui";

/** What's shared: a secret (revealed through the audited value route) or a value typed here. */
export type ShareSource = { kind: "secret"; env: string; key: string; path: string } | { kind: "value" };

const EXPIRY = [
  { value: "60", label: "1 hour" },
  { value: "1440", label: "1 day" },
  { value: "10080", label: "7 days" },
] as const;
type Expiry = (typeof EXPIRY)[number]["value"];
const VIEWS = Array.from({ length: 10 }, (_, i) => ({ value: String(i + 1), label: i === 0 ? "1 open" : `${i + 1} opens` }));

/**
 * "Share once": the value is encrypted here with a fresh key, the server gets only the ciphertext,
 * and the link (with the key after `#`) is shown once. Closing the dialog drops the value and the
 * link.
 */
export function ShareDialog({ source, onClose, onShared }: { source: ShareSource | null; onClose: () => void; onShared: (s: Share) => void }) {
  const [n, setN] = useState(0);
  const [kept, setKept] = useState<ShareSource | null>(source);
  if (source && source !== kept) setKept(source);
  const s = source ?? kept;
  return (
    <Dialog
      open={!!source}
      onClose={onClose}
      onExited={() => (setN((k) => k + 1), setKept(null))}
      title={s?.kind === "secret" ? `Share ${s.key} once` : "Share a value once"}
      lead="For someone outside roll. The value is encrypted in your browser; roll keeps only the ciphertext, and the key is in the link."
      className="w-[min(560px,100%)]"
    >
      {s && <Form key={n} source={s} onClose={onClose} onShared={onShared} />}
    </Dialog>
  );
}

function Form({ source, onClose, onShared }: { source: ShareSource; onClose: () => void; onShared: (s: Share) => void }) {
  const { api } = useSecrets();
  const [value, setValue] = useState("");
  const [note, setNote] = useState(source.kind === "secret" ? source.key : "");
  const [views, setViews] = useState("1");
  const [expiry, setExpiry] = useState<Expiry>("1440");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [made, setMade] = useState<{ link: string; share: Share } | null>(null);
  const [copied, setCopied] = useState(false);

  const create = async () => {
    setBusy(true);
    setError(null);
    try {
      let plain = value;
      if (source.kind === "secret") {
        const r = await api.value(source.env, source.key, source.path);
        if (r.problems.length) throw new Error(`${source.key} doesn't resolve (${r.problems.map((p) => `${p.key}: ${p.problem}`).join("; ")}): nothing was shared`);
        plain = r.value;
      }
      const sealed = sealShare(plain);
      const share = await api.createShare({ ciphertext: sealed.ciphertext, note: note.trim() || undefined, views: Number(views), ttl_minutes: Number(expiry) });
      setValue("");
      setMade({ link: shareLink(location.origin, share.id, sealed.key), share });
      onShared(share);
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  if (made)
    return (
      <div role="status">
        <p className="text-[12.5px] text-muted">
          Copy the link now: it isn&apos;t shown again, and roll can&apos;t rebuild it (it doesn&apos;t have the key). It opens {made.share.views_left === 1 ? "once" : `${made.share.views_left} times`} and expires {until(made.share.expires_at)}.
        </p>
        <div className="mt-2 flex items-center gap-2">
          <code aria-label="Share link" className="min-w-0 flex-1 truncate rounded-md border border-line-2 bg-panel-2 px-2 py-1.5 font-mono text-[12px] select-all">
            {made.link}
          </code>
          <button
            className={button}
            onClick={() => {
              void navigator.clipboard?.writeText(made.link);
              setCopied(true);
              setTimeout(() => setCopied(false), 1200);
            }}
          >
            {copied ? <Check className="size-3.5 text-ok" /> : <Copy className="size-3.5" />} Copy link
          </button>
        </div>
        <p className="mt-2 text-[11.5px] text-subtle">Anyone with the link can open it. Revoke it from Access → Shares.</p>
        <div className="mt-4 flex justify-end">
          <button className={primary} onClick={onClose}>
            Done
          </button>
        </div>
      </div>
    );

  const ok = source.kind === "secret" || value.length > 0;
  return (
    <form
      onSubmit={(e) => {
        e.preventDefault();
        if (ok && !busy) void create();
      }}
    >
      {source.kind === "value" && (
        <label className="mb-3 block">
          <span className={label}>Value</span>
          <textarea autoFocus value={value} onChange={(e) => setValue(e.target.value)} rows={3} spellCheck={false} autoComplete="off" className={`${field} h-auto resize-y py-1.5 font-mono`} />
        </label>
      )}
      {source.kind === "secret" && (
        <p className="mb-3 text-[12px] text-muted">
          Shares {source.key}&apos;s value in {source.env}
          {source.path === "/" ? "" : ` (${source.path})`} as it resolves now. Revealing it is in the audit log, and so is the share.
        </p>
      )}
      <label className="block">
        <span className={label}>Note (shown before it&apos;s opened, to anyone with the link)</span>
        <input autoFocus={source.kind === "secret"} value={note} onChange={(e) => setNote(e.target.value)} maxLength={1000} placeholder="What it is and what to do with it" className={field} />
      </label>
      <div className="mt-3 flex flex-wrap gap-3">
        <div>
          <span className={label}>Opens</span>
          <Select label="Opens" value={views} onChange={setViews} options={VIEWS} className="w-32" />
        </div>
        <div>
          <span className={label}>Expires after</span>
          <Select label="Expires after" value={expiry} onChange={setExpiry} options={EXPIRY} className="w-32" />
        </div>
      </div>
      <ErrorNote error={error} className="mt-3" />
      <div className="mt-4 flex justify-end gap-2">
        <button type="button" className={ghost} onClick={onClose}>
          Cancel
        </button>
        <button type="submit" className={primary} disabled={!ok || busy}>
          {busy ? "Encrypting…" : "Create link"}
        </button>
      </div>
    </form>
  );
}
