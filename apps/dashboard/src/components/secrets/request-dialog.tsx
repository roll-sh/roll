import { Check, Copy } from "lucide-react";
import { useState } from "react";
import type { ValueRequest } from "@/lib/secrets-api";
import { requestLink } from "@/lib/secrets-ask";
import { Dialog } from "../agents/ui/dialog";
import { Select } from "../agents/ui/select";
import { useSecrets } from "./secrets-page";
import { button, ErrorNote, errorText, field, ghost, label, primary, until } from "./ui";

/** Where the values go: a folder of an environment. */
export type RequestTarget = { env: string; path: string; needsApproval: boolean };

const EXPIRY = [
  { value: "1440", label: "1 day" },
  { value: "10080", label: "7 days" },
  { value: "43200", label: "30 days" },
] as const;
type Expiry = (typeof EXPIRY)[number]["value"];
const ANSWERS = [1, 2, 3, 5, 10].map((n) => ({ value: String(n), label: n === 1 ? "Once" : `${n} times` }));

/** Key names typed as a list: commas, spaces or lines between them. */
export const parseKeys = (text: string) => [...new Set(text.split(/[\s,]+/).filter(Boolean))];

/**
 * "Request a value…" (docs/secrets.md, "Asking for a value"): a link for someone outside roll to
 * send a folder's values in. They're sealed in their browser to the request's own key and land as a
 * change (a change request where the folder needs approval).
 */
export function RequestDialog({ target, onClose, onCreated }: { target: RequestTarget | null; onClose: () => void; onCreated?: (r: ValueRequest) => void }) {
  const [n, setN] = useState(0);
  const [kept, setKept] = useState<RequestTarget | null>(target);
  if (target && target !== kept) setKept(target);
  const t = target ?? kept;
  return (
    <Dialog
      open={!!target}
      onClose={onClose}
      onExited={() => (setN((k) => k + 1), setKept(null))}
      title="Request a value"
      lead="A link for someone outside roll, such as a vendor or a contractor. They paste the values; their browser seals them, and roll writes them here."
      className="w-[min(560px,100%)]"
    >
      {t && <Form key={n} target={t} onClose={onClose} onCreated={onCreated} />}
    </Dialog>
  );
}

function Form({ target, onClose, onCreated }: { target: RequestTarget; onClose: () => void; onCreated?: (r: ValueRequest) => void }) {
  const { api } = useSecrets();
  const [keys, setKeys] = useState("");
  const [note, setNote] = useState("");
  const [expiry, setExpiry] = useState<Expiry>("10080");
  const [answers, setAnswers] = useState("1");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [made, setMade] = useState<{ link: string; request: ValueRequest } | null>(null);
  const [copied, setCopied] = useState(false);
  const list = parseKeys(keys);
  const where = `${target.env}${target.path === "/" ? "" : target.path}`;

  const create = async () => {
    setBusy(true);
    setError(null);
    try {
      const request = await api.createValueRequest(target.env, target.path, { keys: list, note: note.trim() || undefined, ttl_minutes: Number(expiry), answers: Number(answers) });
      setMade({ link: request.link ?? requestLink(location.origin, request.token), request });
      onCreated?.(request);
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
          Send this link to whoever has the values. It takes {made.request.answers_left === 1 ? "one answer" : `${made.request.answers_left} answers`} until {until(made.request.expires_at)}, and shows only your note and the key names.
        </p>
        <div className="mt-2 flex items-center gap-2">
          <code aria-label="Request link" className="min-w-0 flex-1 truncate rounded-md border border-line-2 bg-panel-2 px-2 py-1.5 font-mono text-[12px] select-all">
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
        <p className="mt-2 text-[11.5px] text-subtle">Cancel it from Access → Secret requests.</p>
        <div className="mt-4 flex justify-end">
          <button className={primary} onClick={onClose}>
            Done
          </button>
        </div>
      </div>
    );

  return (
    <form
      onSubmit={(e) => {
        e.preventDefault();
        if (list.length && !busy) void create();
      }}
    >
      <label className="block">
        <span className={label}>Keys</span>
        <input autoFocus value={keys} onChange={(e) => setKeys(e.target.value)} placeholder="STRIPE_SECRET_KEY, STRIPE_WEBHOOK_SECRET" spellCheck={false} autoComplete="off" className={`${field} font-mono`} />
      </label>
      <p className="mt-1.5 text-[11.5px] text-subtle">
        Written to <span className="font-mono text-soft">{where}</span>
        {target.needsApproval ? " as a change request: it needs approval there." : " as a change when they answer, or a change request where the folder needs approval."}
      </p>
      <label className="mt-3 block">
        <span className={label}>Note (shown on the page, to anyone with the link)</span>
        <input value={note} onChange={(e) => setNote(e.target.value)} maxLength={1000} placeholder="What to send, and for what" className={field} />
      </label>
      <div className="mt-3 flex flex-wrap gap-3">
        <div>
          <span className={label}>Expires after</span>
          <Select label="Expires after" value={expiry} onChange={setExpiry} options={EXPIRY} className="w-32" />
        </div>
        <div>
          <span className={label}>Answers</span>
          <Select label="Answers" value={answers} onChange={setAnswers} options={ANSWERS} className="w-32" />
        </div>
      </div>
      <ErrorNote error={error} className="mt-3" />
      <div className="mt-4 flex justify-end gap-2">
        <button type="button" className={ghost} onClick={onClose}>
          Cancel
        </button>
        <button type="submit" className={primary} disabled={!list.length || busy}>
          {busy ? "Creating…" : "Create link"}
        </button>
      </div>
    </form>
  );
}
