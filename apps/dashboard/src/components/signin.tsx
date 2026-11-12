import { RollLogo, RollMark } from "@roll/brand";
import { ArrowLeft, LoaderCircle, Mail } from "lucide-react";
import { useNavigate } from "@tanstack/react-router";
import { useEffect, useRef, useState, type ReactNode } from "react";
import { Link } from "@/components/link";
import { acceptSignIn, accountsApi, AccountsError, useSession, type Provider, type SignInAnswer } from "@/lib/accounts-api";
import { GitHubMark, GoogleMark } from "./agents/brand-icons";
import { TokenButton } from "./code/sign-in";
import { useMarkState } from "./mark-state";

const ERRORS: Record<string, string> = {
  state: "That sign-in took too long or came from another browser. Try again.",
  provider: "The provider didn't let the sign-in through. Try again, or use another way.",
  unverified_email: "That account has no verified email address.",
  not_invited: "roll is invite-only for now. Ask for an invite, then open its link.",
  invite_email: "That account's email isn't the one the invite was sent to.",
  invite_invalid: "This invite was used, revoked or has run out.",
  link_required: "You have an account with that email. Sign in another way, then link it in your account settings.",
  linked_elsewhere: "That identity belongs to another roll account.",
  disabled: "This account is disabled.",
};

const safeReturn = (r: string | undefined) => (r && r.startsWith("/") && !r.startsWith("//") ? r : "/dashboard");

type Step = { kind: "start" } | { kind: "code"; email: string; challenge: string } | { kind: "mfa"; recovery: boolean };

/** `/signin`: GitHub, Google or a code by email, then the second factor when the account has one. */
export function SignInPage({ invite, step, returnTo, error }: { invite?: string; step?: string; returnTo?: string; error?: string }) {
  const navigate = useNavigate();
  const session = useSession();
  const { play, state } = useMarkState();
  const [providers, setProviders] = useState<{ email: boolean; github: boolean; google: boolean } | null>(null);
  const [invited, setInvited] = useState<{ email: string } | "invalid" | null>(null);
  const [current, setCurrent] = useState<Step>(step === "second_factor" ? { kind: "mfa", recovery: false } : { kind: "start" });
  const [problem, setProblem] = useState<string | null>(error ? (ERRORS[error] ?? "That didn't work. Try again.") : null);
  const [busy, setBusy] = useState(false);
  const to = safeReturn(returnTo);

  useEffect(() => {
    accountsApi.info().then((i) => setProviders(i.providers), () => setProviders({ email: false, github: false, google: false }));
  }, []);
  useEffect(() => {
    if (invite) accountsApi.peekInvite(invite).then((i) => setInvited({ email: i.email }), () => setInvited("invalid"));
  }, [invite]);
  // Signed in already (or just now): on to where they were going. Owing a second factor: ask for it.
  useEffect(() => {
    if (session.status === "signed_in") navigate({ href: to });
    if (session.status === "mfa_required" && current.kind !== "mfa") setCurrent({ kind: "mfa", recovery: false });
  }, [session.status]); // eslint-disable-line react-hooks/exhaustive-deps

  const run = async (f: () => Promise<void>) => {
    setBusy(true);
    setProblem(null);
    play([["thinking", 900]]);
    try {
      await f();
    } catch (e) {
      play([["error", 700]], "idle");
      setProblem(e instanceof AccountsError && e.code === "bad_code" ? "That code isn't right, or it ran out." : e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
  const done = (a: SignInAnswer) => {
    if (a.status === "mfa_required") setCurrent({ kind: "mfa", recovery: false });
    else play([["success", 900]], "idle");
    acceptSignIn(a);
  };
  const oauth = (p: Provider) =>
    run(async () => {
      const { url } = await accountsApi.oauthStart(p, { invite, return_to: to });
      window.location.assign(url);
    });

  const inviteBad = invited === "invalid";
  return (
    <main className="relative grid min-h-svh place-items-center overflow-hidden bg-bg px-4 py-10" data-agents>
      <Link href="/" aria-label="roll home" className="absolute top-5 left-5">
        <RollLogo state={state} size={18} />
      </Link>
      <div className="rise w-full max-w-[380px]">
        <div className="mb-6 flex flex-col items-center text-center">
          <RollMark size={88} state={busy ? "thinking" : state} />
          <h1 className="mt-5 text-[22px] font-semibold tracking-[-0.045em]">{current.kind === "mfa" ? "One more step" : "Sign in to roll"}</h1>
          <p className="mt-1 text-[13px] text-muted">
            {current.kind === "mfa"
              ? "Enter the code from your authenticator app."
              : invited && invited !== "invalid"
                ? `You're invited as ${invited.email}.`
                : "Databases, storage, agents: your projects in one place."}
          </p>
        </div>

        <div className="rounded-2xl border border-line-2 bg-sheet p-5 shadow-sheet inset-shadow-highlight">
          {problem && <p className="mb-4 rounded-lg border border-danger/40 bg-danger/10 px-3 py-2 text-[12.5px] leading-relaxed text-danger">{problem}</p>}
          {inviteBad && !problem && <p className="mb-4 rounded-lg border border-line-2 px-3 py-2 text-[12.5px] text-muted">{ERRORS.invite_invalid}</p>}

          {current.kind === "start" && (
            <Start
              providers={providers}
              busy={busy}
              defaultEmail={invited && invited !== "invalid" ? invited.email : ""}
              onOauth={oauth}
              onEmail={(email) =>
                run(async () => {
                  const { challenge } = await accountsApi.emailStart(email, invite);
                  setCurrent({ kind: "code", email, challenge });
                })
              }
            />
          )}
          {current.kind === "code" && (
            <Code
              key={current.challenge}
              label={
                <>
                  We sent a code to <b className="font-medium text-fg">{current.email}</b> if it has an account or an invite. It works for 10 minutes.
                </>
              }
              busy={busy}
              length={6}
              onSubmit={(code) => run(async () => done(await accountsApi.emailVerify(current.challenge, code)))}
              onBack={() => setCurrent({ kind: "start" })}
            />
          )}
          {current.kind === "mfa" && (
            <>
              <Code
                key={String(current.recovery)}
                label={current.recovery ? "A recovery code, xxxx-xxxx-xxxx-xxxx. Each works once." : "The 6-digit code your authenticator app shows for roll."}
                busy={busy}
                length={current.recovery ? 19 : 6}
                recovery={current.recovery}
                onSubmit={(code) => run(async () => done(current.recovery ? await accountsApi.recoverySignin(code) : await accountsApi.totpSignin(code)))}
              />
              <button onClick={() => setCurrent({ kind: "mfa", recovery: !current.recovery })} className="mt-3 text-[12px] text-muted hover:text-fg">
                {current.recovery ? "Use the authenticator app" : "Lost your phone? Use a recovery code"}
              </button>
            </>
          )}
        </div>

        <div className="mt-5 flex flex-col items-center gap-2 text-center text-[11.5px] text-subtle">
          roll is invite-only for now.
          <TokenButton label="Use an admin token" />
        </div>
      </div>
    </main>
  );
}

const button =
  "flex h-10 w-full items-center justify-center gap-2.5 rounded-lg border border-line-2 bg-raised text-[13px] font-medium inset-shadow-highlight transition-colors hover:border-line-3 disabled:opacity-50";

function Start({
  providers,
  busy,
  defaultEmail,
  onOauth,
  onEmail,
}: {
  providers: { email: boolean; github: boolean; google: boolean } | null;
  busy: boolean;
  defaultEmail: string;
  onOauth: (p: Provider) => void;
  onEmail: (email: string) => void;
}) {
  const [email, setEmail] = useState(defaultEmail);
  useEffect(() => setEmail((e) => e || defaultEmail), [defaultEmail]);
  if (!providers)
    return (
      <div className="grid h-32 place-items-center">
        <LoaderCircle className="size-4 animate-spin text-subtle motion-reduce:animate-none" />
      </div>
    );
  const social = providers.github || providers.google;
  if (!social && !providers.email) return <p className="text-[12.5px] leading-relaxed text-muted">Sign-in isn't set up on this roll yet. An admin can still use their token below.</p>;
  return (
    <div className="space-y-2.5">
      {providers.github && (
        <button disabled={busy} onClick={() => onOauth("github")} className={button}>
          <GitHubMark className="size-4" /> Continue with GitHub
        </button>
      )}
      {providers.google && (
        <button disabled={busy} onClick={() => onOauth("google")} className={button}>
          <GoogleMark className="size-4" /> Continue with Google
        </button>
      )}
      {social && providers.email && (
        <div className="flex items-center gap-3 py-1 text-[11px] text-subtle">
          <span className="h-px flex-1 bg-line" /> or <span className="h-px flex-1 bg-line" />
        </div>
      )}
      {providers.email && (
        <form
          className="space-y-2.5"
          onSubmit={(e) => {
            e.preventDefault();
            if (email.includes("@")) onEmail(email.trim());
          }}
        >
          <input
            type="email"
            autoComplete="email"
            required
            value={email}
            onChange={(e) => setEmail(e.target.value)}
            placeholder="you@company.com"
            className="h-10 w-full rounded-lg border border-line-2 bg-canvas px-3 text-[13px] outline-none transition-colors focus:border-accent/60"
          />
          <button type="submit" disabled={busy || !email.includes("@")} className="flex h-10 w-full items-center justify-center gap-2 rounded-lg bg-fg text-[13px] font-medium text-bg transition-opacity hover:opacity-90 disabled:opacity-40">
            <Mail className="size-4" /> Email me a code
          </button>
        </form>
      )}
    </div>
  );
}

/** A one-time code: sent as soon as it's whole (6 digits), or on Enter for a recovery code. */
function Code({
  label,
  busy,
  length,
  recovery = false,
  onSubmit,
  onBack,
}: {
  label: ReactNode;
  busy: boolean;
  length: number;
  recovery?: boolean;
  onSubmit: (code: string) => void;
  onBack?: () => void;
}) {
  const [code, setCode] = useState("");
  const input = useRef<HTMLInputElement>(null);
  useEffect(() => input.current?.focus(), []);
  const clean = (v: string) => (recovery ? v.toLowerCase().replace(/[^a-z2-7-]/g, "").slice(0, length) : v.replace(/\D/g, "").slice(0, length));
  return (
    <form
      onSubmit={(e) => {
        e.preventDefault();
        if (code.length === length) onSubmit(code);
      }}
    >
      <p className="mb-3 text-[12.5px] leading-relaxed text-muted">{label}</p>
      <input
        ref={input}
        inputMode={recovery ? "text" : "numeric"}
        autoComplete="one-time-code"
        value={code}
        disabled={busy}
        onChange={(e) => {
          const v = clean(e.target.value);
          setCode(v);
          if (!recovery && v.length === length) onSubmit(v);
        }}
        placeholder={recovery ? "xxxx-xxxx-xxxx-xxxx" : "000000"}
        className={`h-12 w-full rounded-lg border border-line-2 bg-canvas px-3 text-center font-mono outline-none transition-colors focus:border-accent/60 ${recovery ? "text-[15px] tracking-[0.08em]" : "text-[22px] tracking-[0.5em]"}`}
      />
      <div className="mt-3 flex items-center justify-between">
        {onBack ? (
          <button type="button" onClick={onBack} className="flex items-center gap-1 text-[12px] text-muted hover:text-fg">
            <ArrowLeft className="size-3.5" /> Another way
          </button>
        ) : (
          <span />
        )}
        <button type="submit" disabled={busy || code.length !== length} className="h-8 rounded-lg bg-fg px-3.5 text-[12.5px] font-medium text-bg transition-opacity hover:opacity-90 disabled:opacity-40">
          {busy ? "Checking…" : "Sign in"}
        </button>
      </div>
    </form>
  );
}
