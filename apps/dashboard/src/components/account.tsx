import { Building2, Check, Copy, Fingerprint, KeyRound, LoaderCircle, Monitor, ShieldCheck, UserPlus } from "lucide-react";
import { useCallback, useEffect, useState, type ReactNode } from "react";
import { accountsApi, activityText, phishingResistant, sessionMethod, refreshSession, useSession, findPeople, dormant, DORMANT_DAYS, type AccountPolicy, type Activity, type Invite, type NetworkPolicy, type Passkey, type PasskeyPolicy, type Provider, type ResetRequest, type Session, type User, type PeopleFilter } from "@/lib/accounts-api";
import { creationOptions, credentialJSON, passkeyProblem, passkeysSupported } from "@/lib/webauthn";
import { Link } from "./link";
import { GitHubMark, GoogleMark } from "./agents/brand-icons";
import { SsoSection } from "./account-sso";
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
          <Passkeys />
          <Sessions />
          <RecentActivity me={user.id} />
          <YourData handle={user.handle} />
          <DeleteAccount user={user} />
          {user.admin && <Invites />}
          {user.admin && <Resets me={user.id} />}
          {user.admin && <Users me={user.id} />}
          {user.admin && <SsoSection />}
          {user.admin && <HardwareKeys />}
          {user.admin && <EveryoneSecondFactor />}
          {user.admin && <Networks />}
        </div>
      )}
    </AppShell>
  );
}

function Profile({ user, onChange }: { user: User; onChange: (u: User) => void }) {
  const [name, setName] = useState(user.name);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [moving, setMoving] = useState(false);
  return (
    <Section title="Profile">
      <div className={`${card} flex flex-wrap items-end gap-3 p-4`}>
        <label className="min-w-48 flex-1">
          <span className="mb-1 block text-[11.5px] font-medium text-subtle">Name</span>
          <input value={name} maxLength={100} onChange={(e) => setName(e.target.value)} className={`${field} w-full`} />
        </label>
        <div className="text-[12px] leading-relaxed text-muted">
          <div>
            <span className="text-subtle">Email</span> {user.email}{" "}
            {!moving && (
              <button onClick={() => setMoving(true)} className="ml-1 text-accent hover:underline">
                Change
              </button>
            )}
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
      {moving && (
        <EmailChange
          onDone={(u) => {
            setMoving(false);
            if (u) {
              onChange(u);
              void refreshSession();
            }
          }}
        />
      )}
    </Section>
  );
}

/** Move the account to another address: a code goes there, and the old address is told. */
function EmailChange({ onDone }: { onDone: (u: User | null) => void }) {
  const [email, setEmail] = useState("");
  const [challenge, setChallenge] = useState<string | null>(null);
  const [code, setCode] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const run = async (f: () => Promise<void>) => {
    setBusy(true);
    setError(null);
    try {
      await f();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <form
      className={`${card} mt-3 p-4`}
      onSubmit={(e) => {
        e.preventDefault();
        if (!challenge) void run(async () => setChallenge((await accountsApi.changeEmail(email.trim())).challenge));
        else void run(async () => onDone(await accountsApi.confirmEmail(challenge, code.trim())));
      }}
    >
      <p className="text-[12.5px] text-muted">
        {challenge ? `We sent a code to ${email.trim()} (unless it has a roll account already). It works once, for 10 minutes.` : "Sign-in codes and roll's mail go to the new address once you confirm a code sent there. Your current address is told."}
      </p>
      <div className="mt-3 flex flex-wrap gap-2">
        {challenge ? (
          <input autoFocus inputMode="numeric" autoComplete="one-time-code" aria-label="Code from the new address" placeholder="Code from the new address" value={code} onChange={(e) => setCode(e.target.value.replace(/\D/g, "").slice(0, 6))} className={`${field} min-w-48 flex-1 font-mono tracking-[0.15em]`} />
        ) : (
          <input autoFocus type="email" required aria-label="New email address" placeholder="new@address.com" value={email} onChange={(e) => setEmail(e.target.value)} className={`${field} min-w-48 flex-1`} />
        )}
        <button type="submit" disabled={busy || (challenge ? code.length !== 6 : !email.includes("@"))} className={primary}>
          {challenge ? "Confirm" : "Send code"}
        </button>
        <button type="button" onClick={() => onDone(null)} className="px-1 text-[12px] text-muted hover:text-fg">
          Cancel
        </button>
      </div>
      {error && <p className="mt-2 text-[12px] text-danger">{error}</p>}
    </form>
  );
}

/** Delete your own account: typed handle, then sudo mode's confirmation; signed out after. */
function DeleteAccount({ user }: { user: User }) {
  const [typed, setTyped] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  return (
    <Section title="Delete your account" lead="Your sessions, sign-in links, second factors, passkeys and tokens stop at once, and this address no longer signs in. It can't be undone: download your data first if you want it.">
      <form
        className={`${card} flex flex-wrap items-center gap-2 p-4`}
        onSubmit={async (e) => {
          e.preventDefault();
          setBusy(true);
          setError(null);
          try {
            await accountsApi.deleteMe(typed.trim());
            window.location.assign("/signin");
          } catch (err) {
            setError(errorText(err));
            setBusy(false);
          }
        }}
      >
        <input aria-label="Type your handle to confirm" placeholder={`Type ${user.handle} to confirm`} value={typed} onChange={(e) => setTyped(e.target.value)} className={`${field} min-w-48 flex-1 font-mono`} />
        <button type="submit" disabled={busy || typed.trim() !== user.handle} className="h-8 rounded-lg border border-danger/50 px-3.5 text-[12.5px] font-medium text-danger transition-colors hover:bg-danger/10 disabled:opacity-40">
          Delete my account
        </button>
      </form>
      {error && <p className="mt-2 text-[12px] text-danger">{error}</p>}
    </Section>
  );
}

const PROVIDER: Record<Provider, { label: string; Mark: typeof GitHubMark }> = { github: { label: "GitHub", Mark: GitHubMark }, google: { label: "Google", Mark: GoogleMark } };

function Methods({ user }: { user: User }) {
  const [providers, setProviders] = useState<{ github: boolean; google: boolean; sso?: boolean } | null>(null);
  const [error, setError] = useState<string | null>(new URLSearchParams(typeof window === "undefined" ? "" : window.location.search).get("error"));
  useEffect(() => {
    accountsApi.info().then((i) => setProviders(i.providers), () => setProviders({ github: false, google: false }));
  }, []);
  const sso = user.identities.find((i) => i.provider.startsWith("sso:"));
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
        {(providers?.sso || sso) && (
          <div className="flex items-center gap-3 border-t border-line px-4 py-3 first:border-t-0">
            <Building2 className="size-4 text-muted" strokeWidth={1.5} />
            <span className="min-w-0 flex-1 text-[12.5px]">
              Your company&apos;s sign-in
              {sso && <span className="ml-2 text-subtle">{sso.email}</span>}
            </span>
            {sso ? (
              <button onClick={() => accountsApi.unlink(sso.provider).then(() => refreshSession().then(() => window.location.reload()), (e) => setError(errorText(e)))} className={btn}>
                Unlink
              </button>
            ) : (
              <button onClick={() => accountsApi.ssoLink().then(({ url }) => window.location.assign(url), (e) => setError(errorText(e)))} className={btn}>
                Link
              </button>
            )}
          </div>
        )}
        <div className="flex items-center gap-3 border-t border-line px-4 py-3 first:border-t-0">
          <KeyRound className="size-4 text-muted" />
          <span className="flex-1 text-[12.5px]">
            Email code <span className="ml-2 text-subtle">{user.email}</span>
          </span>
        </div>
      </div>
      {error && <p className="mt-2 text-[12px] text-danger">{error === "linked_elsewhere"
            ? "That identity belongs to another roll account."
            : error === "link_email"
              ? "Your company's sign-in signed in another address than this account's."
              : error}</p>}
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

/** Your passkeys (WebAuthn): sign in with one on its own, or use one as the second factor. */
function Passkeys() {
  const [on, setOn] = useState<boolean | null>(null);
  const [list, setList] = useState<Passkey[] | null>(null);
  const [renaming, setRenaming] = useState<{ id: string; name: string } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const load = useCallback(() => accountsApi.passkeys().then(setList, (e) => setError(errorText(e))), []);
  useEffect(() => {
    accountsApi.info().then(
      (i) => {
        const yes = Boolean(i.providers.passkey) && passkeysSupported();
        setOn(yes);
        if (yes) void load();
      },
      () => setOn(false),
    );
  }, [load]);
  if (on === false) return null;
  const add = async () => {
    setBusy(true);
    setError(null);
    try {
      const o = await accountsApi.passkeyOptions();
      const made = (await navigator.credentials.create({ publicKey: creationOptions(o.publicKey) })) as PublicKeyCredential | null;
      if (!made) throw new Error("No passkey was made.");
      await accountsApi.addPasskey(o.id, browser(navigator.userAgent), credentialJSON(made));
      await load();
    } catch (e) {
      setError(passkeyProblem(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Section title="Passkeys" lead="Sign in with your fingerprint, face or device PIN instead of a code. A passkey can't be phished: it only works on roll.">
      <div className={card}>
        {!list ? (
          <div className="grid h-14 place-items-center">
            <LoaderCircle className="size-4 animate-spin text-subtle motion-reduce:animate-none" />
          </div>
        ) : (
          list.map((p) => (
            <div key={p.id} className="flex items-center gap-3 border-t border-line px-4 py-2.5 first:border-t-0">
              <Fingerprint className="size-4 text-muted" />
              <span className="min-w-0 flex-1 leading-tight">
                {renaming?.id === p.id ? (
                  <form
                    className="flex gap-2"
                    onSubmit={(e) => {
                      e.preventDefault();
                      accountsApi.renamePasskey(p.id, renaming.name).then(() => (setRenaming(null), load()), (err) => setError(errorText(err)));
                    }}
                  >
                    <input autoFocus value={renaming.name} maxLength={64} onChange={(e) => setRenaming({ id: p.id, name: e.target.value })} className={`${field} w-56`} />
                    <button type="submit" className={btn}>
                      Save
                    </button>
                  </form>
                ) : (
                  <span className="block truncate text-[12.5px]">{p.name}</span>
                )}
                <span className="block text-[11px] text-subtle">
                  {p.model ? `${p.model} (verified)` : p.synced ? "Synced across your devices" : "On one device"} · added {when(p.created_at)}
                  {p.last_used_at ? ` · last used ${when(p.last_used_at)}` : " · not used yet"}
                </span>
              </span>
              {renaming?.id !== p.id && (
                <button onClick={() => setRenaming({ id: p.id, name: p.name })} className="text-[12px] text-muted hover:text-fg">
                  Rename
                </button>
              )}
              <button onClick={() => accountsApi.removePasskey(p.id).then(load, (e) => setError(errorText(e)))} className={`${btn} text-danger`}>
                Remove
              </button>
            </div>
          ))
        )}
        <div className="flex items-center gap-3 border-t border-line px-4 py-3 first:border-t-0">
          <button disabled={busy || on === null} onClick={add} className={primary}>
            Add a passkey
          </button>
          <span className="text-[11.5px] text-subtle">{list?.length ? "With one, you can skip the authenticator app's code too." : "Your browser or phone keeps it, synced if you use iCloud Keychain or Google Password Manager."}</span>
        </div>
        {error && <p className="px-4 pb-3 text-[12px] text-danger">{error}</p>}
      </div>
    </Section>
  );
}

/** Admins: hardware keys only (docs/accounts.md, "Passkeys"): only listed models, proved by their maker. */
function HardwareKeys() {
  const [policy, setPolicy] = useState<PasskeyPolicy | null>(null);
  const [draft, setDraft] = useState({ aaguid: "", name: "", root: "" });
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    accountsApi.passkeyPolicy().then(setPolicy, () => setPolicy(null));
  }, []);
  if (!policy) return null;
  const save = (p: PasskeyPolicy) =>
    accountsApi.setPasskeyPolicy(p).then(
      (saved) => {
        setPolicy(saved);
        setError(null);
      },
      (e) => setError(errorText(e)),
    );
  return (
    <Section id="hardware-keys" title="Hardware keys only" lead="Require your people's passkeys to be on authenticator models you choose, proved by their maker's attestation. Off: any passkey works.">
      <div className={`${card} space-y-3 p-4`}>
        <label className="flex items-center gap-2 text-[12.5px]">
          <input type="checkbox" checked={policy.require} onChange={(e) => void save({ ...policy, require: e.target.checked })} />
          Only the models below (passkeys on anything else stop working for sign-in)
        </label>
        {policy.models.map((m) => (
          <div key={m.aaguid} className="flex items-center gap-3 text-[12.5px]">
            <KeyRound className="size-4 text-muted" />
            <span className="flex-1">
              {m.name} <span className="font-mono text-[11px] text-subtle">{m.aaguid}</span>
            </span>
            <button onClick={() => void save({ ...policy, models: policy.models.filter((x) => x.aaguid !== m.aaguid) })} className={`${btn} text-danger`}>
              Remove
            </button>
          </div>
        ))}
        <div className="grid gap-2 sm:grid-cols-2">
          <input value={draft.name} onChange={(e) => setDraft({ ...draft, name: e.target.value })} placeholder="YubiKey 5 NFC" className={field} />
          <input value={draft.aaguid} onChange={(e) => setDraft({ ...draft, aaguid: e.target.value })} placeholder="its AAGUID (8-4-4-4-12)" spellCheck={false} className={`${field} font-mono`} />
          <textarea
            value={draft.root}
            onChange={(e) => setDraft({ ...draft, root: e.target.value })}
            rows={3}
            spellCheck={false}
            placeholder="-----BEGIN CERTIFICATE----- its maker's attestation root"
            className="rounded-lg border border-line-2 bg-canvas px-2.5 py-1.5 font-mono text-[11.5px] outline-none focus:border-accent/60 sm:col-span-2"
          />
        </div>
        <button
          disabled={!draft.name.trim() || !draft.aaguid.trim() || !draft.root.trim()}
          onClick={() => void save({ ...policy, models: [...policy.models, { aaguid: draft.aaguid.trim(), name: draft.name.trim(), roots: [draft.root.trim()] }] }).then(() => setDraft({ aaguid: "", name: "", root: "" }))}
          className={btn}
        >
          Add a model
        </button>
        {error && <p className="text-[12px] text-danger">{error}</p>}
      </div>
    </Section>
  );
}

/** Admins: the company's session policy (docs/accounts.md): a second factor for everyone, an idle timeout. */
function EveryoneSecondFactor() {
  const [policy, setPolicy] = useState<AccountPolicy | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    accountsApi.accountPolicy().then(setPolicy, () => setPolicy(null));
  }, []);
  if (!policy) return null;
  const save = (p: AccountPolicy) => accountsApi.setAccountPolicy(p).then((saved) => (setPolicy(saved), setError(null)), (err) => setError(errorText(err)));
  return (
    <Section id="require-2fa" title="Sessions and second factors" lead="For everyone signed in: a second factor (people without one can use only their account page until they set one up; SSO sign-ins are its provider's to check), and how long an unused session lasts.">
      <div className={`${card} space-y-2 p-4`}>
        <label className="flex items-center gap-2 text-[12.5px]">
          <input type="checkbox" checked={policy.require_second_factor} onChange={(e) => void save({ ...policy, require_second_factor: e.target.checked })} />
          Require a second factor
        </label>
        <label className="flex items-center gap-2 text-[12.5px]">
          Phishing-resistant sign-in (a passkey, or your SSO) for
          <select
            value={policy.phishing_resistant ?? ""}
            onChange={(e) => void save({ ...policy, phishing_resistant: (e.target.value || undefined) as AccountPolicy["phishing_resistant"] })}
            className="rounded-md border border-line-2 bg-canvas px-1.5 py-0.5 text-[12px]"
          >
            <option value="">nobody</option>
            <option value="admins">admins</option>
            <option value="everyone">everyone</option>
          </select>
        </label>
        <label className="flex items-center gap-2 text-[12.5px]">
          A session unused for
          <select
            value={policy.idle_minutes ?? 0}
            onChange={(e) => void save({ ...policy, idle_minutes: Number(e.target.value) || undefined })}
            className="rounded-md border border-line-2 bg-canvas px-1.5 py-0.5 text-[12px]"
          >
            {[15, 30, 60, 240, 480, 1440, 0].map((m) => (
              <option key={m} value={m}>
                {m === 0 ? "a week" : m < 60 ? `${m} minutes` : m === 60 ? "an hour" : m < 1440 ? `${m / 60} hours` : "a day"}
              </option>
            ))}
          </select>
          ends
        </label>
        <label className="flex items-center gap-2 text-[12.5px]">
          Every session ends
          <select
            value={policy.max_session_hours ?? 0}
            onChange={(e) => void save({ ...policy, max_session_hours: Number(e.target.value) || undefined })}
            className="rounded-md border border-line-2 bg-canvas px-1.5 py-0.5 text-[12px]"
          >
            {[8, 12, 24, 72, 168, 0, ...(policy.max_session_hours && ![8, 12, 24, 72, 168].includes(policy.max_session_hours) ? [policy.max_session_hours] : [])].map((h) => (
              <option key={h} value={h}>
                {h === 0 ? "30 days" : h < 24 ? `${h} hours` : h === 24 ? "a day" : h === 168 ? "a week" : `${h / 24} days`}
              </option>
            ))}
          </select>
          after its sign-in
        </label>
        {error && <p className="text-[12px] text-danger">{error}</p>}
      </div>
    </Section>
  );
}

/** Admins: allowed networks (docs/accounts.md): people's sessions work only from the company's networks. */
function Networks() {
  const [policy, setPolicy] = useState<NetworkPolicy | null>(null);
  const [text, setText] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  useEffect(() => {
    accountsApi.networkPolicy().then(
      (p) => {
        setPolicy(p);
        setText(p.networks.join("\n"));
      },
      () => setPolicy(null),
    );
  }, []);
  if (!policy) return null;
  const save = (on: boolean) =>
    accountsApi.setNetworkPolicy({ on, networks: text.split(/[\s,]+/).filter(Boolean) }).then(
      (p) => {
        setPolicy(p);
        setText(p.networks.join("\n"));
        setError(null);
        setSaved(true);
      },
      (e) => setError(errorText(e)),
    );
  return (
    <Section id="networks" title="Allowed networks" lead="People's sessions work only from these networks (your offices, your VPN). The operator's token and roll tokens keep their own lists.">
      <div className={`${card} space-y-3 p-4`}>
        <label className="flex items-center gap-2 text-[12.5px]">
          <input type="checkbox" checked={policy.on} onChange={(e) => void save(e.target.checked)} />
          Only from these networks
        </label>
        <textarea
          value={text}
          onChange={(e) => (setText(e.target.value), setSaved(false))}
          rows={4}
          spellCheck={false}
          placeholder={"203.0.113.0/24\n2001:db8::/32"}
          className="w-full rounded-lg border border-line-2 bg-canvas px-2.5 py-1.5 font-mono text-[12px] outline-none focus:border-accent/60"
        />
        <div className="flex items-center gap-3">
          <button onClick={() => void save(policy.on)} className={btn}>
            Save the list
          </button>
          {saved && !error && <span className="text-[11.5px] text-subtle">Saved.</span>}
          {error && <span className="text-[12px] text-danger">{error}</span>}
        </div>
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
                  Signed in {sessionMethod(s) ? `with ${sessionMethod(s)} ` : ""}
                  {when(s.created_at)} · last seen {when(s.seen_at)}
                  {phishingResistant(s) && <span className="text-accent"> · phishing-resistant</span>}
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
      {list && list.some((s) => !s.current) && (
        <div className="mt-2 flex flex-wrap items-center gap-3">
          <button onClick={() => accountsApi.endOtherSessions().then(load)} className={btn}>
            Sign out of all other sessions
          </button>
          <span className="text-[11.5px] text-subtle">
            Tokens and app connections keep working: check them on{" "}
            <Link href="/dashboard/access" className="text-accent hover:underline">
              MCP &amp; access
            </Link>
            .
          </span>
        </div>
      )}
    </Section>
  );
}

const browser = (ua: string) => {
  const name = /Edg\//.test(ua) ? "Edge" : /Chrome\//.test(ua) ? "Chrome" : /Firefox\//.test(ua) ? "Firefox" : /Safari\//.test(ua) ? "Safari" : "A browser";
  const os = /Mac OS X/.test(ua) ? "macOS" : /Windows/.test(ua) ? "Windows" : /Android/.test(ua) ? "Android" : /iPhone|iPad/.test(ua) ? "iOS" : /Linux/.test(ua) ? "Linux" : "";
  return os ? `${name} on ${os}` : name;
};

/** Your data (docs/accounts.md, "Your data"): everything roll keeps about you, as a file. */
function YourData({ handle }: { handle: string }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      const data = await accountsApi.exportData();
      const url = URL.createObjectURL(new Blob([JSON.stringify(data, null, 2)], { type: "application/json" }));
      const a = Object.assign(document.createElement("a"), { href: url, download: `roll-account-${handle}.json` });
      a.click();
      URL.revokeObjectURL(url);
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Section title="Your data" lead="Everything roll keeps about your account, as one JSON file: your sign-in methods, sessions, passkeys, security log and tokens. Never a secret.">
      <div className={`${card} flex flex-wrap items-center gap-3 p-4`}>
        <button disabled={busy} onClick={save} className={btn}>
          Export your data
        </button>
        {error && <span className="text-[12px] text-danger">{error}</span>}
      </div>
    </Section>
  );
}

/** Your security log (GitHub's): sign-ins, refused ones too, and changes to your account. */
/** Admins: one person's sessions (each can be ended) and security log, as they see it themselves. */
function PersonLog({ user }: { user: User }) {
  const [sessions, setSessions] = useState<Session[] | null>(null);
  const [events, setEvents] = useState<Activity[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const load = useCallback(() => {
    accountsApi.userSessions(user.id).then(setSessions, (e) => setError(errorText(e)));
    accountsApi.userActivity(user.id).then(({ events }) => setEvents(events), (e) => setError(errorText(e)));
  }, [user.id]);
  useEffect(() => load(), [load]);
  const who = user.name || user.handle;
  return (
    <div className="space-y-3 border-t border-line bg-canvas/40 px-4 py-3 text-[12px]">
      {error && <p className="text-danger">{error}</p>}
      <div>
        <p className="mb-1 text-[11px] font-medium text-subtle">Sessions</p>
        {sessions && sessions.length === 0 && <p className="text-subtle">{who} isn&apos;t signed in anywhere.</p>}
        {(sessions ?? []).map((s) => (
          <div key={s.id} className="flex items-center gap-3 py-1">
            <span className="min-w-0 flex-1 truncate">{browser(s.user_agent)}</span>
            <span className="shrink-0 text-[11px] text-subtle">since {when(s.created_at)} · last {when(s.seen_at)}</span>
            <button onClick={() => accountsApi.endUserSession(user.id, s.id).then(load, (e) => setError(errorText(e)))} className={btn}>
              End
            </button>
          </div>
        ))}
      </div>
      <div>
        <p className="mb-1 text-[11px] font-medium text-subtle">Recent activity, as {who} sees it</p>
        {events && events.length === 0 && <p className="text-subtle">Nothing yet.</p>}
        {(events ?? []).slice(0, 20).map((e, i) => (
          <div key={`${e.at}-${i}`} className="flex items-baseline gap-3 py-0.5">
            <span className={`min-w-0 flex-1 ${e.ok ? "" : "text-danger"}`}>{activityText(e, user.id)}</span>
            <span className="hidden truncate font-mono text-[11px] text-subtle sm:inline">{[e.ip, e.user_agent ? browser(e.user_agent) : null].filter(Boolean).join(" · ")}</span>
            <span className="shrink-0 text-[11px] text-subtle">{when(e.at)}</span>
          </div>
        ))}
        {events && events.length > 20 && (
          <Link href={`/dashboard/access/audit?product=accounts&resource=${encodeURIComponent(user.id)}`} className="text-accent hover:underline">
            Everything in the audit log
          </Link>
        )}
      </div>
    </div>
  );
}

function RecentActivity({ me }: { me: string }) {
  const [list, setList] = useState<Activity[] | null>(null);
  const [more, setMore] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const load = useCallback(
    (before?: number) =>
      accountsApi.activity(before).then(
        ({ events }) => {
          setList((l) => (before && l ? [...l, ...events] : events));
          setMore(events.length >= 50);
        },
        (e) => setError(errorText(e)),
      ),
    [],
  );
  useEffect(() => void load(), [load]);
  return (
    <Section id="activity" title="Recent activity" lead="Sign-ins to your account, refused ones too, and every change to it. Something you don't recognise? End your other sessions and tell an admin.">
      {error && <p className="text-[12px] text-danger">{error}</p>}
      {list && list.length === 0 && <p className="text-[12.5px] text-subtle">Nothing yet.</p>}
      {list && list.length > 0 && (
        <div className={card}>
          {list.map((e, i) => (
            <div key={`${e.at}-${i}`} className="flex items-baseline gap-3 border-t border-line px-4 py-2.5 first:border-t-0 text-[12.5px]">
              <span className={`min-w-0 flex-1 ${e.ok ? "" : "text-danger"}`}>{activityText(e, me)}</span>
              <span className="hidden truncate font-mono text-[11px] text-subtle sm:inline">{[e.ip, e.user_agent ? browser(e.user_agent) : null].filter(Boolean).join(" · ")}</span>
              <span className="shrink-0 text-[11px] text-subtle">{when(e.at)}</span>
            </div>
          ))}
        </div>
      )}
      {more && list && (
        <button onClick={() => void load(list[list.length - 1].at)} className={`${btn} mt-2`}>
          Older
        </button>
      )}
    </Section>
  );
}

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
                {state !== "Used" && (
                  <button
                    onClick={() =>
                      accountsApi.resendInvite(i.id).then(
                        (r) => {
                          setMade({ email: r.email, url: r.url });
                          void load();
                        },
                        (e) => setError(errorText(e)),
                      )
                    }
                    className={btn}
                  >
                    Resend
                  </button>
                )}
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

/** Admins: second-factor resets people asked for (docs/accounts.md, "Recovery"). Shown when any were. */
function Resets({ me }: { me: string }) {
  const [list, setList] = useState<ResetRequest[]>([]);
  const [error, setError] = useState<string | null>(null);
  const load = useCallback(() => accountsApi.resetRequests().then(setList, () => setList([])), []);
  useEffect(() => void load(), [load]);
  if (list.length === 0) return null;
  const decide = (id: string, approve: boolean) => accountsApi.decideReset(id, approve).then(load, (e) => setError(errorText(e)));
  return (
    <Section id="recovery" title="Second-factor resets" lead="People who lost their authenticator app and recovery codes. Check it's them some other way first (a call, in person): their email code alone got them this far.">
      <div className={card}>
        {list.map((r) => (
          <div key={r.id} className="flex flex-wrap items-start gap-3 border-t border-line px-4 py-3 first:border-t-0">
            <ShieldCheck className={`mt-0.5 size-4 ${r.state === "pending" ? "text-accent" : "text-subtle"}`} />
            <span className="min-w-0 flex-1 leading-snug">
              <span className="block text-[12.5px]">
                {r.handle} <span className="text-subtle">{r.email}</span>
              </span>
              <span className="mt-0.5 block whitespace-pre-wrap text-[12px] text-muted">{r.reason}</span>
              <span className="mt-0.5 block text-[11px] text-subtle">
                Asked {when(r.created_at)}
                {r.ip ? ` from ${r.ip}` : ""}
                {r.user_agent ? ` · ${browser(r.user_agent)}` : ""}
                {r.state !== "pending" && r.decided_at ? ` · ${r.state} ${when(r.decided_at)}` : ""}
              </span>
            </span>
            {r.state === "pending" &&
              (r.user === me ? (
                <span className="text-[11.5px] text-subtle">Another admin decides yours</span>
              ) : (
                <span className="flex gap-2">
                  <button onClick={() => decide(r.id, true)} className={primary}>
                    Reset it
                  </button>
                  <button onClick={() => decide(r.id, false)} className={btn}>
                    Decline
                  </button>
                </span>
              ))}
          </div>
        ))}
        {error && <p className="px-4 pb-3 text-[12px] text-danger">{error}</p>}
      </div>
    </Section>
  );
}

function Users({ me }: { me: string }) {
  const [list, setList] = useState<User[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const load = useCallback(() => accountsApi.users().then(setList, (e) => setError(errorText(e))), []);
  useEffect(() => void load(), [load]);
  const [required, setRequired] = useState(false);
  useEffect(() => {
    accountsApi.accountPolicy().then((p) => setRequired(p.require_second_factor), () => setRequired(false));
  }, []);
  const act = (p: Promise<unknown>) => p.then(load, (e) => setError(errorText(e)));
  const [open, setOpen] = useState<string | null>(null);
  const without = (list ?? []).filter((u) => !u.disabled && !u.totp && !u.passkeys).length;
  const [query, setQuery] = useState("");
  const [filter, setFilter] = useState<PeopleFilter>("everyone");
  const shown = findPeople(list ?? [], query, filter);
  const idle = (list ?? []).filter((u) => dormant(u)).length;
  return (
    <Section title="People" lead="Everyone with an account. Turning someone off signs them out everywhere at once.">
      <p className="-mt-1 mb-2 text-[12px] text-muted">
        Everyone&apos;s sign-ins and account changes are in the{" "}
        <Link href="/dashboard/access/audit?product=accounts" className="text-accent hover:underline">
          sign-in report
        </Link>{" "}
        (the audit log, as CSV for a spreadsheet).
      </p>
      {error && <p className="mb-2 text-[12px] text-danger">{error}</p>}
      {list && idle > 0 && (
        <p className="mb-2 text-[12px] text-muted">
          {idle === 1 ? "One person hasn't" : `${idle} people haven't`} used roll in {DORMANT_DAYS} days: turning off accounts nobody uses closes a way in.
        </p>
      )}
      {list && without > 0 && (
        <p className={`mb-2 text-[12px] ${required ? "text-danger" : "text-muted"}`}>
          {without === 1 ? "One person has" : `${without} people have`} no second factor{required ? ": they can use only their account page until they set one up." : "."}
        </p>
      )}
      {list && list.length > 8 && (
        <div className="mb-2 flex flex-wrap items-center gap-2">
          <input id="people-search" type="search" value={query} onChange={(e) => setQuery(e.target.value)} placeholder="Find by name, handle or address" aria-label="Find people" className={`${field} min-w-0 flex-1 basis-48`} />
          <select id="people-filter" value={filter} onChange={(e) => setFilter(e.target.value as PeopleFilter)} aria-label="Show" className={field}>
            <option value="everyone">Everyone</option>
            <option value="admins">Admins</option>
            <option value="no-second-factor">No second factor</option>
            <option value="turned-off">Turned off</option>
          </select>
          <span className="shrink-0 text-[11.5px] tabular-nums text-subtle">
            {shown.length} of {list.length}
          </span>
        </div>
      )}
      {list && shown.length === 0 && <p className="mb-2 text-[12.5px] text-subtle">Nobody matches.</p>}
      <div className={card}>
        {shown.map((u) => (
          <div key={u.id} className="border-t border-line first:border-t-0">
          <div className={`flex flex-wrap items-center gap-3 px-4 py-2.5 ${u.disabled ? "opacity-60" : ""}`}>
            <span className="min-w-0 flex-1 leading-tight">
              <span className="block truncate text-[12.5px]">
                {u.name || u.handle} <span className="font-mono text-[11px] text-subtle">{u.handle}</span>
              </span>
              <span className="block truncate text-[11px] text-subtle">
                {u.email}
                {u.totp || u.passkeys ? ` · ${[u.totp ? "authenticator app" : "", u.passkeys ? `${u.passkeys} passkey${u.passkeys === 1 ? "" : "s"}` : ""].filter(Boolean).join(", ")}` : ""}
                {!u.totp && !u.passkeys && <span className={required ? "text-danger" : ""}> · no second factor</span>}
                {" · "}
                {dormant(u) && <span className="font-medium text-fg">dormant · </span>}
                {u.last_active_at ? `active ${when(u.last_active_at)}` : "no activity recorded"}
              </span>
            </span>
            {u.id !== me && (
              <>
                <button onClick={() => setOpen(open === u.id ? null : u.id)} aria-expanded={open === u.id} className={btn}>
                  Security log
                </button>
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
          {open === u.id && <PersonLog user={u} />}
          </div>
        ))}
      </div>
    </Section>
  );
}
