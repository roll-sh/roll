/**
 * `/share/<id>`: a value someone shared once (docs/secrets.md, "Sharing a value once"). Public, with
 * no sign-in and no operator headers. Loading the page uses nothing up; the button opens it (a
 * `POST`), and the browser decrypts it with the key in the link's fragment, which never reaches a
 * server. Nothing is kept: leaving the page forgets the value.
 */

import { HorizonMark } from "@roll/brand";
import { Check, Copy, KeyRound, LockOpen } from "lucide-react";
import { useEffect, useState } from "react";
import { PublicShell } from "@/components/repo/public-header";
import { openSealed, openShare, ShareGone, shareKey, type SharePeek } from "@/lib/secrets-share";
import { until } from "./ui";

const primaryBtn = "inline-flex h-9 items-center gap-2 rounded-lg bg-fg px-3.5 text-[13px] font-medium text-bg transition hover:opacity-85 disabled:pointer-events-none disabled:opacity-40";
const quietBtn = "inline-flex h-8 items-center gap-1.5 rounded-lg border border-line px-2.5 text-[12.5px] text-muted transition hover:border-line-2 hover:text-fg";

const times = (n: number) => `${n} ${n === 1 ? "time" : "times"}`;

type Opened = { value: string; left: number };

export function SharePage({ id, peek, now }: { id: string; peek: SharePeek; now: number }) {
  // The key is in the fragment, which only the browser sees: unknown until mounted.
  const [key, setKey] = useState<Uint8Array | null | undefined>(undefined);
  const [hadKey, setHadKey] = useState(false);
  const [busy, setBusy] = useState(false);
  const [opened, setOpened] = useState<Opened | null>(null);
  const [gone, setGone] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    setHadKey(location.hash.length > 1);
    setKey(shareKey(location.hash));
  }, []);

  const open = async () => {
    if (!key || busy) return;
    setBusy(true);
    setError(null);
    try {
      const { ciphertext } = await openShare(id);
      const left = Math.max(0, peek.views_left - 1);
      // The open is used either way; the key leaves the address bar so the back button can't show it.
      history.replaceState(history.state, "", location.pathname);
      try {
        setOpened({ value: openSealed(ciphertext, key), left });
      } catch (e) {
        setError(`${e instanceof Error ? e.message : String(e)}. That used one of its opens: ask the sender for a new link.`);
      }
    } catch (e) {
      if (e instanceof ShareGone) setGone(true);
      else setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  if (gone) return <ShareGonePage />;

  return (
    <PublicShell crumb="Shared value">
      <main className="mx-auto w-full max-w-xl px-4 py-16 sm:py-24">
        <HorizonMark state={opened ? "success" : busy ? "thinking" : "idle"} size={40} />
        <h1 className="mt-6 text-[26px] font-semibold tracking-[-0.045em]">{opened ? "The shared value" : "Someone shared a value with you"}</h1>
        <p className="mt-2 font-mono text-[12px] text-subtle">
          {opened ? (opened.left === 0 ? "used up" : `opens ${times(opened.left)} more · expires ${until(peek.expires_at)}`) : `opens ${times(peek.views_left)} · expires ${until(peek.expires_at, now)}`}
        </p>

        {peek.note && (
          <figure className="mt-6 rounded-xl border border-line bg-panel px-4 py-3">
            <figcaption className="text-[11px] font-medium text-subtle">Note from the sender</figcaption>
            <p className="mt-1 text-[14px] leading-relaxed break-words whitespace-pre-wrap">{peek.note}</p>
          </figure>
        )}

        {opened ? (
          <section className="mt-6">
            <div className="flex items-start gap-2 rounded-xl border border-line-2 bg-panel p-3">
              <pre aria-label="The value" className="max-h-80 min-w-0 flex-1 overflow-auto font-mono text-[13px] leading-relaxed break-all whitespace-pre-wrap select-all">
                {opened.value}
              </pre>
              <button
                className={quietBtn}
                onClick={() => {
                  void navigator.clipboard?.writeText(opened.value);
                  setCopied(true);
                  setTimeout(() => setCopied(false), 1200);
                }}
              >
                {copied ? <Check className="size-3.5 text-ok" /> : <Copy className="size-3.5" />} {copied ? "Copied" : "Copy"}
              </button>
            </div>
            <p className="mt-4 text-[13px] leading-relaxed text-muted" role="status">
              {opened.left === 0 ? (
                <>
                  <b className="font-medium text-fg">This link is now used up.</b> Nobody can open it again, so copy the value before you leave: this page doesn&apos;t keep it.
                </>
              ) : (
                <>
                  The link opens {times(opened.left)} more until it expires {until(peek.expires_at)}. This page doesn&apos;t keep the value: copy it before you leave.
                </>
              )}
            </p>
          </section>
        ) : (
          <section className="mt-6 rounded-xl border border-line bg-panel p-4">
            {key === null ? (
              <p className="flex items-start gap-2 text-[13px] leading-relaxed text-danger" role="alert">
                <KeyRound className="mt-0.5 size-4 shrink-0" />
                {hadKey ? "The key at the end of this link (after #) is damaged." : "This link is missing its key, the part after #."} Ask the sender for the whole link; opening it without the key would only use it up.
              </p>
            ) : (
              <>
                <p className="text-[13px] leading-relaxed text-muted">
                  {peek.views_left === 1 ? "It opens once: after you open it, the link stops working." : `Opening it uses one of its ${peek.views_left} opens.`} Open it when you&apos;re ready to copy it. It&apos;s decrypted here, in your browser, with the key in the link.
                </p>
                <button className={`${primaryBtn} mt-4`} disabled={!key || busy} onClick={open}>
                  <LockOpen className="size-4" /> {busy ? "Opening…" : "Open the value"}
                </button>
              </>
            )}
            {error && (
              <p className="mt-3 text-[13px] text-danger" role="alert">
                {error}
              </p>
            )}
          </section>
        )}
      </main>
    </PublicShell>
  );
}

/** A share that's gone (or never was): a 404. */
export function ShareGonePage() {
  return (
    <PublicShell crumb="Shared value">
      <main className="mx-auto flex w-full max-w-xl flex-col items-center px-4 py-28 text-center">
        <HorizonMark state="sleep" size={64} />
        <p className="mt-8 font-mono text-[12px] text-subtle">404</p>
        <h1 className="mt-2 text-[26px] font-semibold tracking-[-0.045em]">This link doesn&apos;t open anymore</h1>
        <p className="mt-3 max-w-sm text-[14px] leading-relaxed text-muted">It was already opened as many times as it allowed, it expired, or the sender revoked it. Ask them for a new one.</p>
      </main>
    </PublicShell>
  );
}
