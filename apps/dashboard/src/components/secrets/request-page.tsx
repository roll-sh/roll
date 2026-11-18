/**
 * `/request/<id>`: someone asked for values through roll secrets (docs/secrets.md, "Asking for a
 * value"). Public, with no sign-in and no operator headers. It shows the asker's note and the key
 * names, nothing else about the project. The browser seals each value to the request's own key
 * before anything is sent, and the page shows nothing back: once sent, the fields are cleared.
 */

import { HorizonMark } from "@roll/brand";
import { Send, ShieldCheck } from "lucide-react";
import { useState } from "react";
import { PublicShell } from "@/components/repo/public-header";
import { answerRequest, RequestGone, sealAnswer, type ValuePeek } from "@/lib/secrets-ask";
import { until } from "./ui";

const primaryBtn = "inline-flex h-9 items-center gap-2 rounded-lg bg-fg px-3.5 text-[13px] font-medium text-bg transition hover:opacity-85 disabled:pointer-events-none disabled:opacity-40";
const fieldCls = "w-full resize-y rounded-lg border border-line bg-bg px-3 py-2 font-mono text-[13px] leading-relaxed outline-none transition-colors placeholder:text-subtle focus:border-line-3";

export function RequestPage({ id, peek, now }: { id: string; peek: ValuePeek; now: number }) {
  const [values, setValues] = useState<Record<string, string>>(() => Object.fromEntries(peek.keys.map((k) => [k, ""])));
  const [busy, setBusy] = useState(false);
  const [sent, setSent] = useState(false);
  const [gone, setGone] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const ready = peek.keys.every((k) => values[k]?.length);

  const send = async () => {
    if (!ready || busy) return;
    setBusy(true);
    setError(null);
    try {
      await answerRequest(id, await sealAnswer(id, peek.public_key, values));
      setValues({});
      setSent(true);
    } catch (e) {
      if (e instanceof RequestGone) setGone(true);
      else setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  if (gone) return <RequestGonePage />;

  return (
    <PublicShell crumb="Secret request">
      <main className="mx-auto w-full max-w-xl px-4 py-16 sm:py-24">
        <HorizonMark state={sent ? "success" : busy ? "thinking" : "idle"} size={40} />
        <h1 className="mt-6 text-[26px] font-semibold tracking-[-0.045em]">{sent ? "Sent" : peek.keys.length === 1 ? "Someone asked you for a value" : "Someone asked you for some values"}</h1>
        <p className="mt-2 font-mono text-[12px] text-subtle">{sent ? "received" : `expires ${until(peek.expires_at, now)}`}</p>

        {peek.note && !sent && (
          <figure className="mt-6 rounded-xl border border-line bg-panel px-4 py-3">
            <figcaption className="text-[11px] font-medium text-subtle">Note from whoever asked</figcaption>
            <p className="mt-1 text-[14px] leading-relaxed break-words whitespace-pre-wrap">{peek.note}</p>
          </figure>
        )}

        {sent ? (
          <p className="mt-6 text-[14px] leading-relaxed text-muted" role="status">
            roll has {peek.keys.length === 1 ? "it" : "them"} now, and whoever asked can use {peek.keys.length === 1 ? "it" : "them"}. This page kept nothing: you can close it.
          </p>
        ) : (
          <form
            className="mt-6 rounded-xl border border-line bg-panel p-4"
            onSubmit={(e) => {
              e.preventDefault();
              void send();
            }}
          >
            <div className="space-y-4">
              {peek.keys.map((k, i) => (
                <label key={k} className="block">
                  <span className="mb-1.5 block font-mono text-[12px] text-soft">{k}</span>
                  <textarea
                    autoFocus={i === 0}
                    rows={2}
                    value={values[k] ?? ""}
                    onChange={(e) => setValues((v) => ({ ...v, [k]: e.target.value }))}
                    spellCheck={false}
                    autoComplete="off"
                    autoCorrect="off"
                    autoCapitalize="off"
                    className={fieldCls}
                  />
                </label>
              ))}
            </div>
            <p className="mt-4 flex items-start gap-2 text-[12.5px] leading-relaxed text-muted">
              <ShieldCheck className="mt-0.5 size-4 shrink-0" />
              Your browser encrypts {peek.keys.length === 1 ? "it" : "each one"} before sending, to a key only this request has. It goes straight to whoever asked; nothing is shown back.
            </p>
            {error && (
              <p className="mt-3 text-[13px] text-danger" role="alert">
                {error}
              </p>
            )}
            <button type="submit" className={`${primaryBtn} mt-4`} disabled={!ready || busy}>
              <Send className="size-4" /> {busy ? "Sending…" : "Send"}
            </button>
          </form>
        )}
      </main>
    </PublicShell>
  );
}

/** A request that's gone (answered, expired, cancelled) or never was. */
export function RequestGonePage() {
  return (
    <PublicShell crumb="Secret request">
      <main className="mx-auto flex w-full max-w-xl flex-col items-center px-4 py-28 text-center">
        <HorizonMark state="sleep" size={64} />
        <p className="mt-8 font-mono text-[12px] text-subtle">410</p>
        <h1 className="mt-2 text-[26px] font-semibold tracking-[-0.045em]">This request doesn&apos;t take answers anymore</h1>
        <p className="mt-3 max-w-sm text-[14px] leading-relaxed text-muted">It was already answered, it expired, or whoever asked cancelled it. Ask them for a new link.</p>
      </main>
    </PublicShell>
  );
}
