import { Check, Copy, KeyRound, LoaderCircle, Monitor, ShieldCheck, UserPlus } from "lucide-react";
import { useCallback, useEffect, useState, type ReactNode } from "react";
import { accountsApi, refreshSession, useSession, type Invite, type Provider, type Session, type User } from "@/lib/accounts-api";
import { GitHubMark, GoogleMark } from "./agents/brand-icons";
import { AppShell } from "./app-shell";

const card = "rounded-xl border border-line-2 bg-sheet inset-shadow-highlight";
const btn = "flex h-7 items-center gap-1.5 rounded-md border border-line-2 bg-raised px-2.5 text-[12px] font-medium inset-shadow-highlight transition-colors hover:border-line-3 disabled:opacity-50";
const primary = "h-8 rounded-lg bg-fg px-3.5 text-[12.5px] font-medium text-bg transition-opacity hover:opacity-90 disabled:opacity-40";
const field = "h-8 rounded-lg border border-line-2 bg-canvas px-2.5 text-[12.5px] outline-none transition-colors focus:border-accent/60";
const errorText = (e: unknown) => (e instanceof Error ? e.message : String(e));
const when = (ms: number) => new Date(ms).toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" });

function Section({ title, lead, children, id }: { title: string; lead?: string; children: ReactNode; id?: string }) {
  return (
    <section id={id} className="rise scroll-mt-16">
      <h2 className="text-[15px] font-semibold tracking-[-0.03em]">{title}</h2>
      {lead && <p className="mt-0.5 text-[12.5px] text-muted">{lead}</p>}
      <div className="mt-3">{children}</div>
    </section>
  );
}

/** `/dashboard/account`: who you are, how you sign in, your second factor and sessions; for admins, invites and users. */
export function AccountPage() {
  const session = useSession();
  const [user, setUser] = useState<User | null>(null);
  const reload = useCallback(() => accountsApi.me().then(setUser, () => setUser(null)), []);
  useEffect(() => {
    if (session.status === "signed_in") void reload();
  }, [session.status, reload]);

  return (
    <AppShell crumb="Account">
      {session.status !== "signed_in" ? (
        <p className="text-[13px] text-muted">{session.status === "loading" ? "" : "Sign in with your account (not an admin token) to manage it."}</p>
      ) : !user ? null : (
        <div className="mx-auto max-w-3xl space-y-10">
          <Profile user={user} onChange={setUser} />
          <Methods user={user} />
          <TwoFactor user={user} onChange={reload} />
          <Sessions />
          {user.admin && <Invites />}
          {user.admin && <Users me={user.id} />}
        </div>
      )}
    </AppShell>
  );
}

function Profile({ user, onChange }: { user: User; onChange: (u: User) => void }) {
  const [name, setName] = useState(user.name);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  return (
    <Section title="Profile">
      <div className={`${card} flex flex-wrap items-end gap-3 p-4`}>
        <label className="min-w-48 flex-1">
          <span className="mb-1 block text-[11.5px] font-medium text-subtle">Name</span>
          <input value={name} maxLength={100} onChange={(e) => setName(e.target.value)} className={`${field} w-full`} />
        </label>
        <div className="text-[12px] leading-relaxed text-muted">
          <div>
            <span className="text-subtle">Email</span> {user.email}
          </div>
          <div>
            <span className="text-subtle">Handle</span> <span className="font-mono">{user.handle}</span>
          </div>
        </div>
        <button
          disabled={busy || !name.trim() || name === user.name}
          onClick={async () => {
            setBusy(true);
            setError(null);
            try {
              onChange(await accountsApi.updateMe(name.trim()));
              void refreshSession();
            } catch (e) {
              setError(errorText(e));
            } finally {
              setBusy(false);
            }
          }}
          className={primary}
        >
          Save
        </button>
      </div>
      {error && <p className="mt-2 text-[12px] text-danger">{error}</p>}
    </Section>
  );
}

const PROVIDER: Record<Provider, { label: string; Mark: typeof GitHubMark }> = { github: { label: "GitHub", Mark: GitHubMark }, google: { label: "Google", Mark: GoogleMark } };

function Methods({ user }: { user: User }) {
  const [providers, setProviders] = useState<{ github: boolean; google: boolean } | null>(null);
  const [error, setError] = useState<string | null>(new URLSearchParams(typeof window === "undefined" ? "" : window.location.search).get("error"));
  useEffect(() => {
    accountsApi.info().then((i) => setProviders(i.providers), () => setProviders({ github: false, google: false }));
  }, []);
  return (
    <Section title="Sign-in methods" lead="Codes by email always work. Link GitHub or Google to sign in with them too.">
      <div className={card}>
        {(["github", "google"] as const)
          .filter((p) => providers?.[p] || user.identities.some((i) => i.provider === p))
          .map((p) => {
            const linked = user.identities.find((i) => i.provider === p);
            const { label, Mark } = PROVIDER[p];
            return (
              <div key={p} className="flex items-center gap-3 border-t border-line px-4 py-3 first:border-t-0">
                <Mark className="size-4" />
                <span className="min-w-0 flex-1 text-[12.5px]">
                  {label}
                  {linked && <span className="ml-2 text-subtle">{linked.login || linked.email}</span>}
                </span>
                {linked ? (
                  <button
                    onClick={() => accountsApi.unlink(p).then(() => refreshSession().then(() => window.location.reload()), (e) => setError(errorText(e)))}
                    className={btn}
                  >
                    Unlink
                  </button>
                ) : (
                  <button
                    onClick={() => accountsApi.oauthStart(p, { intent: "link", return_to: "/dashboard/account" }).then(({ url }) => window.location.assign(url), (e) => setError(errorText(e)))}
                    className={btn}
                  >
                    Link
                  </button>
                )}
              </div>
            );
          })}
        <div className="flex items-center gap-3 border-t border-line px-4 py-3 first:border-t-0">
          <KeyRound className="size-4 text-muted" />
          <span className="flex-1 text-[12.5px]">
            Email code <span className="ml-2 text-subtle">{user.email}</span>
          </span>
        </div>
      </div>
      {error && <p className="mt-2 text-[12px] text-danger">{error === "linked_elsewhere" ? "That identity belongs to another roll account." : error}</p>}
    </Section>
  );
}

function Codes({ codes }: { codes: string[] }) {
  const [copied, setCopied] = useState(false);
  return (
    <div className="rounded-lg border border-accent/40 bg-accent/[0.05] p-3">
      <p className="text-[12px] text-muted">Recovery codes: keep them somewhere safe. Each works once, and they're shown only now.</p>
      <div className="mt-2 grid grid-cols-2 gap-x-6 gap-y-1 font-mono text-[12.5px]">
        {codes.map((c) => (
          <span key={c}>{c}</span>
        ))}
      </div>
      <button onClick={() => navigator.clipboard.writeText(codes.join("\n")).then(() => setCopied(true))} className={`${btn} mt-3`}>
        {copied ? <Check className="size-3.5 text-ok" /> : <Copy className="size-3.5" />} Copy all
      </button>
    </div>
  );
}

function TwoFactor({ user, onChange }: { user: User; onChange: () => void }) {
  const [setup, setSetup] = useState<{ secret: string; qr_png: string } | null>(null);
  const [code, setCode] = useState("");
  const [codes, setCodes] = useState<string[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const act = async (f: () => Promise<void>) => {
    setBusy(true);
    setError(null);
    try {
      await f();
      setCode("");
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  const codeField = (
    <input
      inputMode="numeric"
      autoComplete="one-time-code"
      placeholder="123456"
      value={code}
      onChange={(e) => setCode(e.target.value.replace(/[^0-9a-z-]/gi, "").slice(0, 19))}
      className={`${field} w-36 font-mono tracking-[0.2em]`}
    />
  );
  return (
    <Section title="Two-factor authentication" lead="An authenticator app's code after every sign-in.">
      <div className={`${card} space-y-3 p-4`}>
        <div className="flex items-center gap-2 text-[12.5px]">
          <ShieldCheck className={`size-4 ${user.totp ? "text-ok" : "text-subtle"}`} />
          {user.totp ? "On: an authenticator app" : "Off"}
        </div>
        {codes && <Codes codes={codes} />}
        {!user.totp && !setup && (
          <button disabled={busy} onClick={() => act(async () => setSetup(await accountsApi.totpBegin()))} className={primary}>
            Set up an authenticator app
          </button>
        )}
        {!user.totp && setup && (
          <div className="flex flex-wrap gap-4">
            <img src={setup.qr_png} alt="QR code for your authenticator app" className="size-36 rounded-lg bg-white p-2" />
            <div className="min-w-48 flex-1 space-y-2 text-[12px] text-muted">
              <p>Scan it with your authenticator app (1Password, Google Authenticator, Authy…), or enter this key:</p>
              <p className="font-mono text-[12px] break-all text-fg">{setup.secret}</p>
              <p>Then type the code it shows:</p>
              <div className="flex gap-2">
                {codeField}
                <button
                  disabled={busy || code.length !== 6}
                  onClick={() =>
                    act(async () => {
                      setCodes((await accountsApi.totpConfirm(code)).recovery_codes);
                      setSetup(null);
                      onChange();
                    })
                  }
                  className={primary}
                >
                  Turn on
                </button>
              </div>
            </div>
          </div>
        )}
        {user.totp && (
          <div className="flex flex-wrap items-center gap-2">
            {codeField}
            <button disabled={busy || code.length < 6} onClick={() => act(async () => setCodes((await accountsApi.newRecoveryCodes(code)).recovery_codes))} className={btn}>
              New recovery codes
            </button>
            <button
              disabled={busy || code.length < 6}
              onClick={() =>
                act(async () => {
                  await accountsApi.totpDisable(code);
                  setCodes(null);
                  onChange();
                })
              }
              className={`${btn} text-danger`}
            >
              Turn off
            </button>
            <span className="text-[11.5px] text-subtle">Both need a code from the app.</span>
          </div>
        )}
        {error && <p className="text-[12px] text-danger">{error}</p>}
      </div>
    </Section>
  );
}

function Sessions() {
  const [list, setList] = useState<Session[] | null>(null);
  const load = useCallback(() => accountsApi.sessions().then(setList, () => setList([])), []);
  useEffect(() => void load(), [load]);
  return (
    <Section title="Sessions" lead="Where you're signed in. Signing one out takes effect at once.">
      <div className={card}>
        {!list ? (
          <div className="grid h-14 place-items-center">
            <LoaderCircle className="size-4 animate-spin text-subtle motion-reduce:animate-none" />
          </div>
        ) : (
          list.map((s) => (
            <div key={s.id} className="flex items-center gap-3 border-t border-line px-4 py-2.5 first:border-t-0">
              <Monitor className="size-4 text-muted" />
              <span className="min-w-0 flex-1 leading-tight">
                <span className="block truncate text-[12.5px]">{browser(s.user_agent)}</span>
                <span className="block text-[11px] text-subtle">
                  Signed in {when(s.created_at)} · last seen {when(s.seen_at)}
                </span>
              </span>
              {s.current ? (
                <span className="text-[11px] text-accent">This browser</span>
              ) : (
                <button onClick={() => accountsApi.revokeSession(s.id).then(load)} className={btn}>
                  Sign out
                </button>
              )}
            </div>
          ))
        )}
      </div>
    </Section>
  );
}

const browser = (ua: string) => {
  const name = /Edg\//.test(ua) ? "Edge" : /Chrome\//.test(ua) ? "Chrome" : /Firefox\//.test(ua) ? "Firefox" : /Safari\//.test(ua) ? "Safari" : "A browser";
  const os = /Mac OS X/.test(ua) ? "macOS" : /Windows/.test(ua) ? "Windows" : /Android/.test(ua) ? "Android" : /iPhone|iPad/.test(ua) ? "iOS" : /Linux/.test(ua) ? "Linux" : "";
  return os ? `${name} on ${os}` : name;
};

function Invites() {
  const [list, setList] = useState<Invite[] | null>(null);
  const [email, setEmail] = useState("");
  const [admin, setAdmin] = useState(false);
  const [made, setMade] = useState<{ email: string; url: string } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const load = useCallback(() => accountsApi.invites().then(setList, (e) => setError(errorText(e))), []);
  useEffect(() => void load(), [load]);
  return (
    <Section id="invites" title="Invites" lead="roll is invite-only. An invite is for one email address and works once, for 7 days.">
      <form
        className={`${card} flex flex-wrap items-center gap-2 p-3`}
        onSubmit={async (e) => {
          e.preventDefault();
          setError(null);
          try {
            const i = await accountsApi.invite(email.trim(), { admin });
            setMade({ email: i.email, url: i.url });
            setEmail("");
            setAdmin(false);
            void load();
          } catch (err) {
            setError(errorText(err));
          }
        }}
      >
        <UserPlus className="size-4 text-muted" />
        <input type="email" required value={email} onChange={(e) => setEmail(e.target.value)} placeholder="their@email.com" className={`${field} min-w-48 flex-1`} />
        <label className="flex items-center gap-1.5 text-[12px] text-muted">
          <input type="checkbox" checked={admin} onChange={(e) => setAdmin(e.target.checked)} /> Admin
        </label>
        <button type="submit" disabled={!email.includes("@")} className={primary}>
          Invite
        </button>
      </form>
      {made && (
        <div className="mt-2 rounded-lg border border-accent/40 bg-accent/[0.05] p-3 text-[12px] text-muted">
          Invited {made.email}. If mail is set up they have the link already; otherwise send it to them:
          <div className="mt-1.5 flex items-center gap-2">
            <span className="min-w-0 flex-1 truncate font-mono text-fg">{made.url}</span>
            <button onClick={() => navigator.clipboard.writeText(made.url)} className={btn}>
              <Copy className="size-3.5" /> Copy
            </button>
          </div>
        </div>
      )}
      {error && <p className="mt-2 text-[12px] text-danger">{error}</p>}
      {list && list.length > 0 && (
        <div className={`${card} mt-3`}>
          {list.map((i) => {
            const state = i.used_at ? "Used" : i.expires_at < Date.now() ? "Expired" : "Waiting";
            return (
              <div key={i.id} className="flex items-center gap-3 border-t border-line px-4 py-2.5 first:border-t-0 text-[12.5px]">
                <span className="min-w-0 flex-1 truncate">
                  {i.email}
                  {i.admin && <span className="ml-2 text-[11px] text-accent">admin</span>}
                </span>
                <span className="text-[11px] text-subtle">{state}</span>
                {state === "Waiting" && (
                  <button onClick={() => accountsApi.revokeInvite(i.id).then(load, (e) => setError(errorText(e)))} className={btn}>
                    Revoke
                  </button>
                )}
              </div>
            );
          })}
        </div>
      )}
    </Section>
  );
}

function Users({ me }: { me: string }) {
  const [list, setList] = useState<User[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const load = useCallback(() => accountsApi.users().then(setList, (e) => setError(errorText(e))), []);
  useEffect(() => void load(), [load]);
  const act = (p: Promise<unknown>) => p.then(load, (e) => setError(errorText(e)));
  return (
    <Section title="People" lead="Everyone with an account. Turning someone off signs them out everywhere at once.">
      {error && <p className="mb-2 text-[12px] text-danger">{error}</p>}
      <div className={card}>
        {(list ?? []).map((u) => (
          <div key={u.id} className={`flex items-center gap-3 border-t border-line px-4 py-2.5 first:border-t-0 ${u.disabled ? "opacity-60" : ""}`}>
            <span className="min-w-0 flex-1 leading-tight">
              <span className="block truncate text-[12.5px]">
                {u.name || u.handle} <span className="font-mono text-[11px] text-subtle">{u.handle}</span>
              </span>
              <span className="block truncate text-[11px] text-subtle">
                {u.email}
                {u.totp ? " · 2FA" : ""}
              </span>
            </span>
            {u.id !== me && (
              <>
                <button onClick={() => act(accountsApi.updateUser(u.id, { admin: !u.admin }))} className={btn}>
                  {u.admin ? "Remove admin" : "Make admin"}
                </button>
                <button onClick={() => act(accountsApi.signOutUser(u.id))} className={btn}>
                  Sign out
                </button>
                <button onClick={() => act(accountsApi.updateUser(u.id, { disabled: !u.disabled }))} className={`${btn} ${u.disabled ? "" : "text-danger"}`}>
                  {u.disabled ? "Turn on" : "Turn off"}
                </button>
              </>
            )}
            {u.id === me && <span className="text-[11px] text-accent">You{u.admin ? " · admin" : ""}</span>}
          </div>
        ))}
      </div>
    </Section>
  );
}
