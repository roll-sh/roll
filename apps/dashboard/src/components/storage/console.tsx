"use client";

import { HorizonMark } from "@roll/brand";
import { ArrowLeft, LogIn, LogOut, ShieldCheck, UserRound } from "lucide-react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { useEffect, useState, useSyncExternalStore, type ReactNode } from "react";
import { cleanCode, passwordProblems } from "@/lib/storage-iam";
import { inMs } from "@/lib/storage-api";
import {
  accountLabel,
  accountsApi,
  acknowledgeEnded,
  clearSession,
  consoleApi,
  errorCode,
  errorMessage,
  getAccount,
  getScope,
  getSession,
  sessionEnded,
  setAccount,
  signinStepFor,
  StorageError,
  subscribe,
  type ConsoleSession,
  type SigninStep,
  type StorageAccount,
} from "@/lib/storage-session";
import { AppShell } from "../app-shell";
import { useMarkState } from "../mark-state";
import { btn, EmptyPanel, ErrorNote, input, panel, primary } from "../ui";
import { Field } from "./shared";

export const SIGNIN_HREF = "/dashboard/storage/-/signin";
export const IAM_HREF = "/dashboard/storage/-/iam";
export const ACCOUNTS_HREF = "/dashboard/storage/-/accounts";

/** The console session (IAM user signed in), or `null` in operator mode. */
export function useConsoleSession() {
  return useSyncExternalStore(subscribe, () => getSession(), () => null);
}

/** The account the operator acts in (`null`: the default). */
export function useActingAccount() {
  return useSyncExternalStore(subscribe, getAccount, () => null);
}

/** Whom requests act as, as a string that changes with it: key data and effects on it. */
export function useScope() {
  return useSyncExternalStore(subscribe, getScope, () => "default");
}

/** When the session ends (a 401, or its expiry passes), go to the sign-in page. */
function useSessionEndedRedirect(session: ConsoleSession | null) {
  const router = useRouter();
  const ended = useSyncExternalStore(subscribe, sessionEnded, () => false);
  useEffect(() => {
    if (ended) router.replace(SIGNIN_HREF);
  }, [ended, router]);
  useEffect(() => {
    if (!session) return;
    // setTimeout's limit is ~24.8 days; sessions are hours.
    const t = setTimeout(() => clearSession(true), Math.min(2 ** 31 - 1, Math.max(0, session.expires_at - Date.now())));
    return () => clearTimeout(t);
  }, [session]);
}

/** Operator: every account (for the switcher); `null` until loaded or when the API refuses. */
export function useAccounts(enabled: boolean) {
  const [accounts, setAccounts] = useState<StorageAccount[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [offline, setOffline] = useState(false);
  const [tick, setTick] = useState(0);
  useEffect(() => {
    if (!enabled) return;
    let alive = true;
    accountsApi.list().then(
      (a) => {
        if (!alive) return;
        setAccounts(a);
        setError(null);
        setOffline(false);
        // The selected account is gone: back to the default one.
        const sel = getAccount();
        if (sel && !a.some((x) => x.id === sel)) setAccount(null);
      },
      (e) => {
        if (!alive) return;
        setError(errorMessage(e));
        setOffline(!(e instanceof StorageError));
      },
    );
    return () => {
      alive = false;
    };
  }, [enabled, tick]);
  return { accounts, error, offline, reload: () => setTick((t) => t + 1) };
}

const mono = "font-mono text-[12px]";

/**
 * Who the storage pages act as, in a line: the signed-in IAM user (with sign out), or the operator
 * and the account they act in (with a switcher and sign in).
 */
export function StorageIdentity({ switcher = true }: { switcher?: boolean }) {
  const session = useConsoleSession();
  useSessionEndedRedirect(session);
  const acting = useActingAccount();
  const { accounts } = useAccounts(!session && switcher);
  const router = useRouter();

  if (session) {
    return (
      <div className="flex flex-wrap items-center gap-x-2 gap-y-1 text-[12px] text-muted">
        <UserRound className="size-3.5 text-subtle" />
        <span className={`${mono} text-fg`}>{session.user}</span>
        <span className="text-subtle">@</span>
        <span className={mono} title={session.arn}>
          {session.account_alias ?? session.account_id}
        </span>
        {session.account_alias && <span className={`${mono} text-subtle`}>{session.account_id}</span>}
        {session.mfa && (
          <span className="inline-flex items-center gap-1 rounded-md border border-line px-1.5 py-px text-[10px] text-muted" title="Signed in with MFA">
            <ShieldCheck className="size-3" /> MFA
          </span>
        )}
        <span className="text-subtle" title={new Date(session.expires_at).toLocaleString()}>
          · {inMs(session.expires_at)} left
        </span>
        <button
          type="button"
          onClick={() => {
            consoleApi.signout();
            router.push(SIGNIN_HREF);
          }}
          className="ml-1 inline-flex items-center gap-1 text-subtle transition hover:text-fg"
        >
          <LogOut className="size-3.5" /> Sign out
        </button>
      </div>
    );
  }

  const current = accounts?.find((a) => (acting ? a.id === acting : a.default));
  return (
    <div className="flex flex-wrap items-center gap-x-2 gap-y-1 text-[12px] text-muted">
      <span className="text-subtle">Operator, acting in</span>
      {switcher && accounts && accounts.length > 1 ? (
        <select
          value={current?.id ?? ""}
          onChange={(e) => {
            const a = accounts.find((x) => x.id === e.target.value);
            setAccount(a && !a.default ? a.id : null);
          }}
          className="rounded-md border border-line bg-panel px-1.5 py-0.5 font-mono text-[12px] outline-none transition focus:border-line-2"
          aria-label="Account to act in"
        >
          {accounts.map((a) => (
            <option key={a.id} value={a.id}>
              {accountLabel(a)} · {a.id}
              {a.default ? " (default)" : ""}
            </option>
          ))}
        </select>
      ) : (
        <span className={`${mono} text-fg`}>{current ? `${accountLabel(current)} · ${current.id}` : (acting ?? "the default account")}</span>
      )}
      <span className="text-subtle">·</span>
      <Link href={SIGNIN_HREF} className="inline-flex items-center gap-1 text-accent hover:underline">
        <LogIn className="size-3.5" /> Sign in as an IAM user
      </Link>
    </div>
  );
}

/** A page that needs operator access, while an IAM user is signed in. */
export function OperatorOnlyNote({ what }: { what: string }) {
  const router = useRouter();
  return (
    <EmptyPanel>
      <HorizonMark state="sleep" size={40} />
      <h2 className="text-[15px] font-medium">{what} needs operator access</h2>
      <p className="max-w-md text-[13px] text-muted">
        You&apos;re signed in as an IAM user, so the dashboard acts with your IAM permissions in your account. Sign out to manage
        rolld itself.
      </p>
      <button
        type="button"
        onClick={() => {
          consoleApi.signout();
          router.refresh();
        }}
        className={btn}
      >
        <LogOut className="size-3.5" /> Sign out
      </button>
    </EmptyPanel>
  );
}

// --- Sign-in -----------------------------------------------------------------------------------

/** The IAM user sign-in page: account (id or alias), user and password; then an MFA code or a new password when asked. */
export function StorageSignIn() {
  const router = useRouter();
  const session = useConsoleSession();
  const ended = useSyncExternalStore(subscribe, sessionEnded, () => false);
  const { setState, play } = useMarkState();
  const [account, setAccountField] = useState("");
  const [user, setUser] = useState("");
  const [password, setPassword] = useState("");
  const [code, setCode] = useState("");
  const [newPassword, setNewPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const [step, setStep] = useState<SigninStep>("password");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const last = consoleApi.lastSignin();
    if (last) {
      setAccountField(last.account);
      setUser(last.user);
    }
  }, []);

  const mismatch = step === "new-password" && confirm !== "" && confirm !== newPassword;
  // IAM's default rules (the account's own policy isn't readable before signing in); the server decides.
  const weak = step === "new-password" && newPassword ? passwordProblems(newPassword, null) : [];
  const ready =
    account.trim() &&
    user.trim() &&
    password &&
    (step !== "mfa" || code.length === 6) &&
    (step !== "new-password" || (newPassword && newPassword === confirm));

  const submit = async () => {
    if (!ready || busy) return;
    setBusy(true);
    setError(null);
    setState("thinking");
    try {
      await consoleApi.signin({
        account,
        user,
        password,
        mfa_code: step === "mfa" ? code : undefined,
        new_password: step === "new-password" ? newPassword : undefined,
      });
      acknowledgeEnded();
      play([["success", 1200]]);
      router.push("/dashboard/storage");
    } catch (e) {
      const next = signinStepFor(errorCode(e));
      if (next && next !== step) {
        // Not an error: the server wants one more thing.
        setStep(next);
        setState("idle");
      } else {
        setError(errorCode(e) === "InvalidMfaCode" ? "That code didn't work. Wait for the next one and try again." : errorMessage(e));
        if (errorCode(e) === "InvalidMfaCode") setCode("");
        play([["error", 1400]]);
      }
    } finally {
      setBusy(false);
    }
  };

  return (
    <AppShell
      crumb={
        <>
          <Link href="/dashboard/storage" className="text-muted hover:text-fg">
            storage
          </Link>{" "}
          <span className="text-subtle">/</span> sign in
        </>
      }
    >
      <div className="mx-auto max-w-sm pt-6">
        <h1 className="text-2xl font-semibold tracking-[-0.04em]">Sign in</h1>
        <p className="mt-1 text-[13px] text-muted">As an IAM user of a roll storage account. The dashboard then acts with your permissions.</p>

        {session ? (
          <div className={`${panel} rise mt-6 space-y-3 p-5`}>
            <p className="text-[13px] text-muted">You&apos;re signed in.</p>
            <StorageIdentity switcher={false} />
            <Link href="/dashboard/storage" className={btn}>
              <ArrowLeft className="size-3.5" /> Storage
            </Link>
          </div>
        ) : (
          <form
            className={`${panel} mt-6 space-y-4 p-5`}
            onSubmit={(e) => {
              e.preventDefault();
              submit();
            }}
          >
            {ended && <p className="rise text-[13px] text-muted">Your session ended. Sign in again to continue.</p>}
            <Field label="Account ID or alias">
              <input
                value={account}
                onChange={(e) => setAccountField(e.target.value)}
                autoFocus={!account}
                autoComplete="organization"
                spellCheck={false}
                placeholder="123456789012"
                disabled={step !== "password"}
                className={`${input} w-full font-mono text-[12px]`}
              />
            </Field>
            <Field label="User name">
              <input
                value={user}
                onChange={(e) => setUser(e.target.value)}
                autoFocus={!!account && !user}
                autoComplete="username"
                spellCheck={false}
                disabled={step !== "password"}
                className={`${input} w-full font-mono text-[12px]`}
              />
            </Field>
            <Field label="Password">
              <input
                type="password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                autoFocus={!!account && !!user}
                autoComplete="current-password"
                disabled={step !== "password"}
                className={`${input} w-full`}
              />
            </Field>

            {step === "mfa" && (
              <Step>
                <Field label="MFA code" hint="The 6-digit code from your authenticator app">
                  <input
                    value={code}
                    onChange={(e) => setCode(cleanCode(e.target.value))}
                    autoFocus
                    inputMode="numeric"
                    autoComplete="one-time-code"
                    placeholder="123456"
                    className={`${input} w-full font-mono tracking-[0.3em] tabular-nums`}
                  />
                </Field>
              </Step>
            )}

            {step === "new-password" && (
              <Step>
                <p className="text-[12px] text-muted">Your password must be changed before you continue.</p>
                <Field label="New password">
                  <input
                    type="password"
                    value={newPassword}
                    onChange={(e) => setNewPassword(e.target.value)}
                    autoFocus
                    autoComplete="new-password"
                    className={`${input} w-full`}
                  />
                </Field>
                <Field label="Confirm new password">
                  <input
                    type="password"
                    value={confirm}
                    onChange={(e) => setConfirm(e.target.value)}
                    autoComplete="new-password"
                    className={`${input} w-full`}
                  />
                </Field>
                {mismatch && <p className="text-[12px] text-danger">The passwords don&apos;t match.</p>}
                {!mismatch && weak.length > 0 && <p className="text-[12px] text-subtle">Usually needs {weak.join(", ")}.</p>}
              </Step>
            )}

            <ErrorNote error={error} />
            <div className="flex items-center gap-2">
              <button type="submit" disabled={!ready || busy} className={primary}>
                {busy ? <span className="shimmer">Signing in…</span> : step === "new-password" ? "Change password and sign in" : "Sign in"}
              </button>
              {step !== "password" && (
                <button
                  type="button"
                  onClick={() => {
                    setStep("password");
                    setCode("");
                    setNewPassword("");
                    setConfirm("");
                    setError(null);
                  }}
                  className={btn}
                >
                  Start over
                </button>
              )}
            </div>
          </form>
        )}
        <p className="mt-4 text-[12px] text-subtle">
          Running rolld yourself?{" "}
          <Link href="/dashboard/storage" className="text-accent hover:underline">
            Continue as the operator
          </Link>{" "}
          — no sign-in needed.
        </p>
      </div>
    </AppShell>
  );
}

function Step({ children }: { children: ReactNode }) {
  return <div className="rise space-y-3 border-t border-line pt-4">{children}</div>;
}
