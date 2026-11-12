import { KeyRound, LogOut, X } from "lucide-react";
import { useCallback, useEffect, useRef, useState, type FormEvent } from "react";
import { codeApi, createCodeApi, type CodeInfo } from "@/lib/code-api";
import { getToken, markOperator, onSignInRequest, setToken, subscribeToken } from "@/lib/code-token";
import { ROLL_API } from "@/lib/roll-api";
import { btn, input, primary } from "../ui";

type Caller = CodeInfo["auth"]["caller"];

/**
 * Sign in when rolld wants a token (`ROLL_ADMIN_TOKEN`, or roll code's `ROLL_CODE_TOKEN`): paste
 * the operator token (every product) or a roll code owner token, checked against `GET /v1/code`
 * before it's kept, with whether it's the operator's. Opens by itself when a request comes back
 * 401. Renders nothing on an open server.
 */
export function SignIn() {
  const [auth, setAuth] = useState<CodeInfo["auth"] | null>(null);
  const [open, setOpen] = useState(false);

  const load = useCallback(() => {
    codeApi
      .info()
      .then((i) => {
        setAuth(i.auth ?? null);
        // Kept in step with the server: a token that stopped being the operator's stops going to the products.
        markOperator(Boolean(i.auth?.operator));
      })
      .catch(() => setAuth(null));
  }, []);

  useEffect(() => {
    load();
    const offToken = subscribeToken(load);
    const offRequest = onSignInRequest(() => setOpen(true));
    return () => {
      offToken();
      offRequest();
    };
  }, [load]);

  if (!auth?.required) return null;
  const who = auth.caller;
  const signedIn = who.kind === "admin" || who.kind === "owner";
  return (
    <>
      {signedIn ? (
        <button onClick={() => setToken(null)} className={btn} title="Sign out of roll in this browser">
          <span className="size-1.5 rounded-full bg-accent" aria-hidden />
          <span className="font-mono">{auth.operator ? "operator" : who.kind === "admin" ? "admin" : who.owner}</span>
          <LogOut className="size-3.5" />
        </button>
      ) : (
        <button onClick={() => setOpen(true)} className={btn}>
          <KeyRound className="size-3.5" />
          {who.kind === "invalid" ? <span className="text-danger">Token rejected</span> : "Sign in"}
        </button>
      )}
      {open && <SignInDialog current={who} onClose={() => setOpen(false)} />}
    </>
  );
}

/** Paste a token now, whatever the server said or didn't (the agents' "Connecting…" that went wrong). */
export function TokenButton({ label = "Use another token" }: { label?: string }) {
  const [open, setOpen] = useState(false);
  return (
    <>
      <button onClick={() => setOpen(true)} className={btn}>
        <KeyRound className="size-3.5" />
        {label}
      </button>
      {open && <SignInDialog current={null} onClose={() => setOpen(false)} />}
    </>
  );
}

function SignInDialog({ current, onClose }: { current: Caller | null; onClose: () => void }) {
  const [value, setValue] = useState(getToken() ?? "");
  const [error, setError] = useState<string | null>(current?.kind === "invalid" ? "The saved token isn't valid on this server." : null);
  const [busy, setBusy] = useState(false);
  const field = useRef<HTMLInputElement>(null);

  useEffect(() => {
    field.current?.focus();
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    const token = value.trim();
    if (!token) return;
    setBusy(true);
    setError(null);
    try {
      const info = await createCodeApi(ROLL_API, token).info();
      const kind = info.auth?.caller.kind;
      if (kind === "admin" || kind === "owner") {
        setToken(token, Boolean(info.auth?.operator));
        onClose();
        window.location.reload();
        return;
      }
      setError("That token isn't valid on this server.");
    } catch {
      setError("Couldn't reach the control plane.");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div
      className="fixed inset-0 z-50 flex items-start justify-center bg-black/60 px-4 pt-[18vh] backdrop-blur-sm"
      onMouseDown={(e) => e.target === e.currentTarget && onClose()}
    >
      <form onSubmit={submit} className="rise relative w-full max-w-md rounded-xl border border-line bg-panel p-5">
        <button type="button" onClick={onClose} aria-label="Close" className="absolute top-4 right-4 text-subtle hover:text-fg">
          <X className="size-4" />
        </button>
        <h2 className="text-[15px] font-semibold tracking-[-0.02em]">Sign in to roll</h2>
        <p className="mt-1.5 text-[13px] leading-relaxed text-muted">
          Paste the operator token (<span className="font-mono text-[12px]">ROLL_ADMIN_TOKEN</span>, every product) or a roll code owner
          token (<span className="font-mono text-[12px]">rlc_…</span>). It stays in this browser.
        </p>
        <input
          ref={field}
          type="password"
          autoComplete="off"
          spellCheck={false}
          value={value}
          onChange={(e) => setValue(e.target.value)}
          placeholder="rlc_…"
          aria-label="Token"
          className={`${input} mt-4 w-full font-mono`}
        />
        <p className="mt-2 min-h-5 text-[12px] text-danger" role="alert">
          {error}
        </p>
        <div className="mt-2 flex justify-end gap-2">
          <button type="button" onClick={onClose} className={btn}>
            Cancel
          </button>
          <button type="submit" disabled={busy || !value.trim()} className={primary}>
            {busy ? <span className="shimmer">Checking…</span> : "Sign in"}
          </button>
        </div>
        <p className="mt-4 border-t border-line pt-3 font-mono text-[11px] text-subtle">roll code token create &lt;owner&gt; &lt;name&gt;</p>
      </form>
    </div>
  );
}
