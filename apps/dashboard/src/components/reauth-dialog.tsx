import { Fingerprint, LoaderCircle, Mail, ShieldCheck } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { accountsApi } from "@/lib/accounts-api";
import { onReauthRequired } from "@/lib/reauth";
import { credentialJSON, passkeyProblem, passkeysSupported, requestOptions } from "@/lib/webauthn";

const btn = "flex h-8 items-center justify-center gap-2 rounded-lg border border-line-2 bg-raised px-3 text-[12.5px] font-medium inset-shadow-highlight transition-colors hover:border-line-3 disabled:opacity-50";
const primary = "h-8 rounded-lg bg-fg px-3.5 text-[12.5px] font-medium text-bg transition-opacity hover:opacity-90 disabled:opacity-40";
const field = "h-8 w-full rounded-lg border border-line-2 bg-canvas px-2.5 font-mono text-[12.5px] tracking-[0.15em] outline-none transition-colors focus:border-accent/60";

type Pending = { resolve: () => void; reject: (e: Error) => void };
type Methods = ("totp" | "passkey" | "email")[];

/**
 * Sudo mode (docs/accounts.md): when a change to how the account is reached needs a recent proof,
 * this asks the person to confirm it's them (a passkey, their authenticator app, or an email
 * code), then the change runs again on its own. Mounted once, by the app shell.
 */
export function ReauthDialog() {
  const [pending, setPending] = useState<Pending | null>(null);
  const [methods, setMethods] = useState<Methods | null>(null);
  const [challenge, setChallenge] = useState<string | null>(null);
  const [code, setCode] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const input = useRef<HTMLInputElement>(null);

  useEffect(() => {
    onReauthRequired(
      () =>
        new Promise<void>((resolve, reject) => {
          setPending({ resolve, reject });
          setMethods(null);
          setChallenge(null);
          setCode("");
          setError(null);
          accountsApi.reauthInfo().then(
            (i) => setMethods(i.methods.filter((m) => m !== "passkey" || passkeysSupported())),
            () => setMethods([]),
          );
        }),
    );
    return () => onReauthRequired(null);
  }, []);
  useEffect(() => input.current?.focus(), [methods, challenge]);

  if (!pending) return null;
  const finish = (ok: boolean) => {
    if (ok) pending.resolve();
    else pending.reject(new Error("Not confirmed: nothing changed."));
    setPending(null);
  };
  /** Run `f`; `done` closes the dialog as confirmed once it succeeds. */
  const step = async (f: () => Promise<unknown>, done: boolean) => {
    setBusy(true);
    setError(null);
    try {
      await f();
      if (done) finish(true);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
  const passkey = () =>
    step(async () => {
      const o = await accountsApi.reauthPasskeyOptions();
      let used: PublicKeyCredential | null;
      try {
        used = (await navigator.credentials.get({ publicKey: requestOptions(o.publicKey) })) as PublicKeyCredential | null;
      } catch (e) {
        throw new Error(passkeyProblem(e));
      }
      if (!used) throw new Error("No passkey was used.");
      await accountsApi.reauth({ id: o.id, credential: credentialJSON(used) });
    }, true);
  const emailCode = () => step(async () => setChallenge((await accountsApi.reauthEmail()).challenge), false);
  const submitCode = () => step(() => accountsApi.reauth(challenge ? { challenge, code } : { code }), true);
  const app = methods?.includes("totp") && !challenge;
  return (
    <div role="dialog" aria-modal="true" aria-labelledby="reauth-title" className="fixed inset-0 z-50 grid place-items-center bg-bg/70 px-4" onKeyDown={(e) => e.key === "Escape" && finish(false)}>
      <div className="rise w-full max-w-sm rounded-xl border border-line-2 bg-panel p-5">
        <div className="flex items-center gap-2">
          <ShieldCheck className="size-4 text-accent" strokeWidth={1.5} />
          <h2 id="reauth-title" className="text-[15px] font-semibold tracking-[-0.03em]">
            Confirm it&apos;s you
          </h2>
        </div>
        <p className="mt-1.5 text-[12.5px] leading-relaxed text-muted">This changes how your account is reached. roll asks again after two hours.</p>
        {!methods ? (
          <div className="grid h-20 place-items-center">
            <LoaderCircle className="size-4 animate-spin text-subtle motion-reduce:animate-none" />
          </div>
        ) : (
          <div className="mt-4 space-y-3">
            {methods.includes("passkey") && !challenge && (
              <button disabled={busy} onClick={passkey} className={`${btn} w-full`}>
                <Fingerprint className="size-4" strokeWidth={1.5} /> Use a passkey
              </button>
            )}
            {(app || challenge) && (
              <form
                className="flex gap-2"
                onSubmit={(e) => {
                  e.preventDefault();
                  if (code.trim()) void submitCode();
                }}
              >
                <input
                  ref={input}
                  inputMode={challenge ? "numeric" : "text"}
                  autoComplete="one-time-code"
                  aria-label={challenge ? "Code from your email" : "Code from your authenticator app, or a recovery code"}
                  placeholder={challenge ? "Code from your email" : "Code from your app"}
                  value={code}
                  onChange={(e) => setCode(e.target.value.replace(/[^0-9a-z-]/gi, "").slice(0, 19))}
                  className={field}
                />
                <button type="submit" disabled={busy || !code.trim()} className={primary}>
                  Confirm
                </button>
              </form>
            )}
            {methods.includes("email") && !challenge && (
              <button disabled={busy} onClick={emailCode} className="flex items-center gap-1.5 text-[12px] text-muted hover:text-fg">
                <Mail className="size-3.5" strokeWidth={1.5} /> {app || methods.includes("passkey") ? "Email me a code instead" : "Email me a code"}
              </button>
            )}
            {challenge && <p className="text-[11.5px] text-subtle">We sent a code to your email. It works once, for 10 minutes.</p>}
            {methods.length === 0 && <p className="text-[12px] text-muted">There&apos;s no way to confirm on this roll: sign out and sign in again.</p>}
          </div>
        )}
        {error && <p className="mt-3 text-[12px] text-danger">{error}</p>}
        <div className="mt-4 flex justify-end">
          <button onClick={() => finish(false)} className="text-[12px] text-muted hover:text-fg">
            Cancel
          </button>
        </div>
      </div>
    </div>
  );
}
