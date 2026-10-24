"use client";

import { KeyRound, Plus, RefreshCw, ShieldCheck, Users, Wand2 } from "lucide-react";
import Link from "next/link";
import { useState, type ReactNode } from "react";
import {
  cleanCode,
  generatePassword,
  iamApi,
  mfaCodesError,
  mfaSeed,
  passwordProblems,
  qrDataUrl,
  bool,
  type IamGroup,
  type NewAccessKey,
  type PasswordPolicy,
  type VirtualMfaDevice,
} from "@/lib/storage-iam";
import { iconBtn } from "../../tasks/ui";
import { btn, CopyButton, ErrorNote, input, Panel, primary, SecretField, useCopy } from "../../ui";
import { SIGNIN_HREF } from "../console";
import { Check, Field, useAction } from "../shared";
import {
  AttachedPolicies,
  Badge,
  DeleteButton,
  DetailHeader,
  EntityList,
  InlinePolicies,
  Loading,
  MasterDetail,
  Muted,
  NameForm,
  NothingSelected,
  Row,
  Section,
  TagsSection,
  useLoad,
  When,
} from "./common";

/** IAM users: people and programs with their own keys, password, MFA and permissions. */
export function UsersTab({ accountId }: { accountId: string }) {
  const users = useLoad("users", iamApi.listUsers);
  // `undefined`: unknown (not allowed to read it); `null`: none, IAM's default rules.
  const policy = useLoad("password-policy", () => iamApi.getPasswordPolicy().catch(() => undefined));
  const [selected, setSelected] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const { busy, error, run } = useAction(users.reload);
  const current = users.data?.some((u) => u.UserName === selected) ? selected : null;

  return (
    <>
      <ErrorNote error={error ?? users.error} />
      <div className="mt-4">
        <MasterDetail
          list={
            <Panel
              title="Users"
              aside={
                <span className="flex items-center gap-2">
                  <span className="font-mono text-[12px] text-subtle">{users.data?.length ?? "—"}</span>
                  {!creating && (
                    <button type="button" onClick={() => setCreating(true)} className={btn}>
                      <Plus className="size-3.5" /> New
                    </button>
                  )}
                </span>
              }
            >
              {creating && (
                <NameForm
                  kind="user"
                  placeholder="alice"
                  busy={busy === "create"}
                  onCancel={() => setCreating(false)}
                  onCreate={async (name) => {
                    const r = await run("create", () => iamApi.createUser(name));
                    if (!r) return false;
                    setCreating(false);
                    setSelected(name);
                    return true;
                  }}
                />
              )}
              <EntityList
                items={users.data}
                keyOf={(u) => u.UserName}
                selected={current}
                onSelect={setSelected}
                title={(u) => u.UserName}
                meta={(u) => (
                  <>
                    created <When iso={u.CreateDate} />
                    {u.PasswordLastUsed ? (
                      <>
                        {" "}
                        · signed in <When iso={u.PasswordLastUsed} />
                      </>
                    ) : null}
                  </>
                )}
                empty="No IAM users yet. A user has its own access keys, console password and MFA device, and only the permissions its policies grant."
              />
            </Panel>
          }
          detail={
            current ? (
              <UserDetail
                key={current}
                name={current}
                accountId={accountId}
                policy={policy.data}
                onDeleted={async () => {
                  setSelected(null);
                  await users.reload();
                }}
              />
            ) : (
              <NothingSelected>Choose a user to see its keys, password, MFA, groups and policies.</NothingSelected>
            )
          }
        />
      </div>
    </>
  );
}

function UserDetail({
  name,
  accountId,
  policy,
  onDeleted,
}: {
  name: string;
  accountId: string;
  policy: PasswordPolicy | null | undefined;
  onDeleted: () => Promise<void>;
}) {
  const user = useLoad(`user:${name}`, () => iamApi.getUser(name));
  const { busy, error, run } = useAction(async () => {});
  return (
    <section className="rounded-xl border border-line bg-panel">
      <DetailHeader
        name={name}
        arn={user.data?.Arn ?? "…"}
        meta={
          user.data && (
            <>
              <span className="font-mono text-[11px] text-subtle">{user.data.UserId}</span> · created <When iso={user.data.CreateDate} />
            </>
          )
        }
        busy={busy === "delete"}
        deleteHint="IAM only deletes a user without keys, password, MFA devices, groups or policies."
        onDelete={async () => {
          const r = await run("delete", () => iamApi.deleteUser(name));
          if (r) await onDeleted();
        }}
      />
      {(error || user.error) && (
        <div className="px-5 pb-1">
          <ErrorNote error={error ?? user.error} />
        </div>
      )}
      <AccessKeys user={name} />
      <ConsolePassword user={name} accountId={accountId} policy={policy} />
      <MfaDevices user={name} />
      <UserGroups user={name} />
      <AttachedPolicies target={{ kind: "user", name }} />
      <InlinePolicies target={{ kind: "user", name }} />
      <TagsSection target={{ kind: "user", name }} />
    </section>
  );
}

// --- Access keys -------------------------------------------------------------------------------

function AccessKeys({ user }: { user: string }) {
  const keys = useLoad(`keys:${user}`, async () => {
    const list = await iamApi.listAccessKeys(user);
    const used = await Promise.all(list.map((k) => iamApi.accessKeyLastUsed(k.AccessKeyId).catch(() => null)));
    return list.map((k, i) => ({ ...k, lastUsed: used[i] }));
  });
  const [created, setCreated] = useState<NewAccessKey | null>(null);
  const { busy, error, run } = useAction(keys.reload);
  return (
    <Section
      title="Access keys"
      aside={
        <button
          type="button"
          disabled={busy !== null}
          onClick={async () => {
            const k = await run("create", () => iamApi.createAccessKey(user));
            if (k) setCreated(k);
          }}
          className={btn}
        >
          {busy === "create" ? (
            <span className="shimmer">Creating…</span>
          ) : (
            <>
              <Plus className="size-3.5" /> Create key
            </>
          )}
        </button>
      }
    >
      <ErrorNote error={error ?? keys.error} />
      {created && <NewKey k={created} onDone={() => setCreated(null)} />}
      {keys.data === null ? (
        <Loading />
      ) : keys.data.length === 0 ? (
        <Muted>No access keys. S3 clients sign requests with one (SigV4); IAM allows two per user.</Muted>
      ) : (
        <ul>
          {keys.data.map((k) => {
            const active = k.Status === "Active";
            return (
              <Row
                key={k.AccessKeyId}
                actions={
                  <>
                    <button
                      type="button"
                      disabled={busy !== null}
                      onClick={() => run(`status-${k.AccessKeyId}`, () => iamApi.updateAccessKey(user, k.AccessKeyId, active ? "Inactive" : "Active"))}
                      className={btn}
                    >
                      {busy === `status-${k.AccessKeyId}` ? <span className="shimmer">Saving…</span> : active ? "Deactivate" : "Activate"}
                    </button>
                    <DeleteButton
                      label={`Delete ${k.AccessKeyId}`}
                      busy={busy === `del-${k.AccessKeyId}`}
                      onConfirm={() => run(`del-${k.AccessKeyId}`, () => iamApi.deleteAccessKey(user, k.AccessKeyId))}
                    />
                  </>
                }
              >
                <p className="flex items-center gap-2 text-[13px]">
                  <KeyRound className="size-3.5 shrink-0 text-subtle" />
                  <span className="truncate font-mono">{k.AccessKeyId}</span>
                  <Badge tone={active ? "ok" : "muted"}>{active ? "active" : "inactive"}</Badge>
                </p>
                <p className="truncate pl-5.5 text-[11px] text-subtle">
                  created <When iso={k.CreateDate} /> ·{" "}
                  {k.lastUsed?.LastUsedDate ? (
                    <>
                      used <When iso={k.lastUsed.LastUsedDate} />
                      {k.lastUsed.ServiceName && k.lastUsed.ServiceName !== "N/A" ? ` (${k.lastUsed.ServiceName})` : ""}
                    </>
                  ) : (
                    "never used"
                  )}
                </p>
              </Row>
            );
          })}
        </ul>
      )}
    </Section>
  );
}

/** A new key's secret: shown here once. */
function NewKey({ k, onDone }: { k: NewAccessKey; onDone: () => void }) {
  const { copied, copy } = useCopy();
  const env = `AWS_ACCESS_KEY_ID=${k.AccessKeyId}\nAWS_SECRET_ACCESS_KEY=${k.SecretAccessKey}\n`;
  return (
    <div className="rise mb-3 space-y-3 rounded-lg border border-accent/40 bg-bg p-4">
      <div className="flex items-start justify-between gap-3">
        <p className="text-[12px] text-muted">Copy the secret now — it isn&apos;t shown again.</p>
        <button type="button" onClick={onDone} className={btn}>
          Done
        </button>
      </div>
      <Field label="Access key id">
        <SecretField id="iam-kid" value={k.AccessKeyId} masked={k.AccessKeyId} copied={copied} onCopy={copy} />
      </Field>
      <Field label="Secret access key">
        <SecretField id="iam-secret" value={k.SecretAccessKey} masked={`${k.SecretAccessKey.slice(0, 4)}${"•".repeat(16)}`} copied={copied} onCopy={copy} />
      </Field>
      <button type="button" onClick={() => copy("iam-env", env)} className={btn}>
        {copied === "iam-env" ? "Copied .env" : "Copy as .env"}
      </button>
    </div>
  );
}

// --- Console password --------------------------------------------------------------------------

function ConsolePassword({ user, accountId, policy }: { user: string; accountId: string; policy: PasswordPolicy | null | undefined }) {
  const profile = useLoad(`login:${user}`, () => iamApi.getLoginProfile(user));
  const [editing, setEditing] = useState(false);
  const { busy, error, run } = useAction(profile.reload);
  const p = profile.data;
  const resetRequired = bool(p?.PasswordResetRequired);
  return (
    <Section
      title="Console password"
      aside={
        p && !editing ? (
          <span className="flex items-center gap-1">
            <button type="button" onClick={() => setEditing(true)} className={btn}>
              Change
            </button>
            <DeleteButton label="Remove the password" confirm="Remove" busy={busy === "delete"} onConfirm={() => run("delete", () => iamApi.deleteLoginProfile(user))} />
          </span>
        ) : null
      }
    >
      <ErrorNote error={error ?? profile.error} />
      {!profile.loaded ? (
        !profile.error && <Loading />
      ) : !p ? (
        <PasswordForm
          policy={policy}
          busy={busy === "create"}
          submitLabel="Set password"
          intro={<Muted>No console password: the user can&apos;t sign in to the dashboard. Set one to let them.</Muted>}
          onSubmit={(pw, reset) => run("create", () => iamApi.createLoginProfile(user, pw, reset))}
        />
      ) : (
        <div className="space-y-3">
          <p className="text-[13px] text-muted">
            Can sign in at{" "}
            <Link href={SIGNIN_HREF} className="text-accent hover:underline">
              storage sign-in
            </Link>{" "}
            with account <span className="font-mono text-[12px] text-fg">{accountId}</span> · set <When iso={p.CreateDate} />
          </p>
          <Check
            checked={resetRequired}
            disabled={busy !== null}
            onChange={(v) => run("reset", () => iamApi.updateLoginProfile(user, { PasswordResetRequired: v }))}
            label="Require a new password at next sign-in"
          />
          {editing && (
            <PasswordForm
              policy={policy}
              busy={busy === "update"}
              submitLabel="Change password"
              onCancel={() => setEditing(false)}
              onSubmit={async (pw, reset) => {
                const r = await run("update", () => iamApi.updateLoginProfile(user, { Password: pw, PasswordResetRequired: reset }));
                if (r) setEditing(false);
                return r;
              }}
            />
          )}
        </div>
      )}
    </Section>
  );
}

function PasswordForm({
  policy,
  busy,
  submitLabel,
  intro,
  onSubmit,
  onCancel,
}: {
  policy: PasswordPolicy | null | undefined;
  busy: boolean;
  submitLabel: string;
  intro?: ReactNode;
  onSubmit: (password: string, resetRequired: boolean) => Promise<unknown>;
  onCancel?: () => void;
}) {
  const [pw, setPw] = useState("");
  const [reset, setReset] = useState(true);
  const [generated, setGenerated] = useState(false);
  const { copied, copy } = useCopy();
  const problems = pw && policy !== undefined ? passwordProblems(pw, policy) : [];
  return (
    <form
      className="rise space-y-3"
      onSubmit={async (e) => {
        e.preventDefault();
        if (!pw) return;
        const r = await onSubmit(pw, reset);
        if (r) {
          setPw("");
          setGenerated(false);
        }
      }}
    >
      {intro}
      <div className="flex gap-2">
        <input
          type={generated ? "text" : "password"}
          value={pw}
          onChange={(e) => {
            setPw(e.target.value);
            setGenerated(false);
          }}
          autoComplete="new-password"
          placeholder="Password"
          aria-label="Password"
          className={`${input} min-w-0 flex-1 ${generated ? "font-mono text-[12px]" : ""}`}
        />
        {generated && <CopyButton id="gen-pw" text={pw} copied={copied} onCopy={copy} />}
        <button
          type="button"
          onClick={() => {
            setPw(generatePassword(policy ?? null));
            setGenerated(true);
          }}
          className={btn}
          title="Generate a strong password"
        >
          <Wand2 className="size-3.5" /> Generate
        </button>
      </div>
      {problems.length > 0 && <p className="text-[12px] text-subtle">Needs {problems.join(", ")}.</p>}
      <Check checked={reset} onChange={setReset} label="Require a new password at first sign-in" hint="They choose their own when they sign in." />
      <div className="flex gap-2">
        <button type="submit" disabled={busy || !pw || problems.length > 0} className={primary}>
          {busy ? <span className="shimmer">Saving…</span> : submitLabel}
        </button>
        {onCancel && (
          <button type="button" onClick={onCancel} className={btn}>
            Cancel
          </button>
        )}
      </div>
    </form>
  );
}

// --- MFA ---------------------------------------------------------------------------------------

function MfaDevices({ user }: { user: string }) {
  const devices = useLoad(`mfa:${user}`, () => iamApi.listMfaDevices(user));
  const [pending, setPending] = useState<VirtualMfaDevice | null>(null);
  const [naming, setNaming] = useState(false);
  const [deviceName, setDeviceName] = useState(user);
  const [resync, setResync] = useState<string | null>(null);
  const { busy, error, run } = useAction(devices.reload);
  return (
    <Section
      title="MFA devices"
      aside={
        !pending &&
        !naming && (
          <button type="button" onClick={() => setNaming(true)} className={btn}>
            <ShieldCheck className="size-3.5" /> Assign virtual device
          </button>
        )
      }
    >
      <ErrorNote error={error ?? devices.error} />
      {naming && (
        <form
          className="rise mb-3 flex flex-wrap gap-2"
          onSubmit={async (e) => {
            e.preventDefault();
            const d = await run("create", () => iamApi.createVirtualMfaDevice(deviceName.trim()));
            if (d) {
              setPending(d);
              setNaming(false);
            }
          }}
        >
          <input
            autoFocus
            value={deviceName}
            onChange={(e) => setDeviceName(e.target.value)}
            spellCheck={false}
            aria-label="Device name"
            placeholder="device name"
            className={`${input} min-w-0 flex-1 font-mono text-[12px]`}
          />
          <button type="submit" disabled={!deviceName.trim() || busy !== null} className={primary}>
            {busy === "create" ? <span className="shimmer">Creating…</span> : "Continue"}
          </button>
          <button type="button" onClick={() => setNaming(false)} className={btn}>
            Cancel
          </button>
        </form>
      )}
      {pending && (
        <EnableDevice
          user={user}
          device={pending}
          onDone={async () => {
            setPending(null);
            await devices.reload();
          }}
          onCancel={async () => {
            // Don't leave an unassigned device behind.
            await iamApi.deleteVirtualMfaDevice(pending.SerialNumber).catch(() => {});
            setPending(null);
          }}
        />
      )}
      {devices.data === null ? (
        <Loading />
      ) : devices.data.length === 0 ? (
        !pending && <Muted>No MFA device. With one, signing in asks for a code, and policies can require aws:MultiFactorAuthPresent.</Muted>
      ) : (
        <ul>
          {devices.data.map((d) => (
            <li key={d.SerialNumber} className="border-b border-line last:border-0">
              <div className="flex min-h-9 items-center gap-3 py-1.5">
                <div className="min-w-0 flex-1">
                  <p className="truncate font-mono text-[12px]">{d.SerialNumber}</p>
                  <p className="text-[11px] text-subtle">
                    enabled <When iso={d.EnableDate} />
                  </p>
                </div>
                <button
                  type="button"
                  onClick={() => setResync(resync === d.SerialNumber ? null : d.SerialNumber)}
                  className={iconBtn}
                  title="Resync"
                  aria-label={`Resync ${d.SerialNumber}`}
                >
                  <RefreshCw className="size-3.5" />
                </button>
                <DeleteButton
                  label={`Remove ${d.SerialNumber}`}
                  confirm="Deactivate and delete"
                  busy={busy === `del-${d.SerialNumber}`}
                  onConfirm={() =>
                    run(`del-${d.SerialNumber}`, async () => {
                      await iamApi.deactivateMfaDevice(user, d.SerialNumber);
                      if (d.SerialNumber.includes(":mfa/")) await iamApi.deleteVirtualMfaDevice(d.SerialNumber);
                    })
                  }
                />
              </div>
              {resync === d.SerialNumber && (
                <CodesForm
                  label="Resync"
                  hint="Two consecutive codes from the app, to catch up with its clock."
                  busy={busy === "resync"}
                  onSubmit={async (a, b) => {
                    const r = await run("resync", () => iamApi.resyncMfaDevice(user, d.SerialNumber, a, b));
                    if (r) setResync(null);
                  }}
                  onCancel={() => setResync(null)}
                />
              )}
            </li>
          ))}
        </ul>
      )}
    </Section>
  );
}

/** A new virtual device: scan the QR code (or type the seed), then prove it with two consecutive codes. */
function EnableDevice({
  user,
  device,
  onDone,
  onCancel,
}: {
  user: string;
  device: VirtualMfaDevice;
  onDone: () => Promise<void>;
  onCancel: () => Promise<void>;
}) {
  const { copied, copy } = useCopy();
  const { busy, error, run } = useAction(async () => {});
  let seed: string | null = null;
  try {
    seed = device.Base32StringSeed ? mfaSeed(device.Base32StringSeed) : null;
  } catch {
    seed = null;
  }
  return (
    <div className="rise mb-3 space-y-4 rounded-lg border border-accent/40 bg-bg p-4">
      <p className="text-[12px] text-muted">
        Scan this with an authenticator app (1Password, Google Authenticator, …), or enter the key by hand. It&apos;s shown only now.
      </p>
      <div className="flex flex-wrap items-start gap-4">
        {device.QRCodePNG && (
          // A data URL from the API: next/image adds nothing here.
          <img
            src={qrDataUrl(device.QRCodePNG)}
            alt={`QR code for ${device.SerialNumber}`}
            width={160}
            height={160}
            className="size-40 rounded-lg border border-line"
            style={{ imageRendering: "pixelated" }}
          />
        )}
        <div className="min-w-0 flex-1 space-y-3">
          <Field label="Serial number">
            <p className="truncate font-mono text-[12px]">{device.SerialNumber}</p>
          </Field>
          {seed && (
            <Field label="Secret key">
              <SecretField id="mfa-seed" value={seed.replace(/ /g, "")} masked={`${seed.slice(0, 4)} ${"•".repeat(12)}`} copied={copied} onCopy={copy} />
            </Field>
          )}
        </div>
      </div>
      <ErrorNote error={error} />
      <CodesForm
        label="Enable"
        hint="Enter a code, wait for the next one, and enter it too."
        busy={busy === "enable"}
        onSubmit={async (a, b) => {
          const r = await run("enable", () => iamApi.enableMfaDevice(user, device.SerialNumber, a, b));
          if (r) await onDone();
        }}
        onCancel={onCancel}
      />
    </div>
  );
}

function CodesForm({
  label,
  hint,
  busy,
  onSubmit,
  onCancel,
}: {
  label: string;
  hint: string;
  busy: boolean;
  onSubmit: (a: string, b: string) => Promise<void>;
  onCancel: () => void;
}) {
  const [a, setA] = useState("");
  const [b, setB] = useState("");
  const err = a.length === 6 && b.length === 6 ? mfaCodesError(a, b) : null;
  const code = `${input} w-28 font-mono tracking-[0.2em] tabular-nums`;
  return (
    <form
      className="space-y-2 pb-2"
      onSubmit={async (e) => {
        e.preventDefault();
        if (mfaCodesError(a, b)) return;
        await onSubmit(a, b);
      }}
    >
      <div className="flex flex-wrap items-center gap-2">
        <input value={a} onChange={(e) => setA(cleanCode(e.target.value))} inputMode="numeric" placeholder="code 1" aria-label="First code" className={code} />
        <input value={b} onChange={(e) => setB(cleanCode(e.target.value))} inputMode="numeric" placeholder="code 2" aria-label="Second code" className={code} />
        <button type="submit" disabled={busy || !!mfaCodesError(a, b)} className={primary}>
          {busy ? <span className="shimmer">Checking…</span> : label}
        </button>
        <button type="button" onClick={onCancel} className={btn}>
          Cancel
        </button>
      </div>
      <p className={`text-[12px] ${err ? "text-danger" : "text-subtle"}`}>{err ?? hint}</p>
    </form>
  );
}

// --- Groups ------------------------------------------------------------------------------------

function UserGroups({ user }: { user: string }) {
  const groups = useLoad(`groups-of:${user}`, () => iamApi.groupsForUser(user));
  const [adding, setAdding] = useState(false);
  const [all, setAll] = useState<IamGroup[] | null>(null);
  const [pick, setPick] = useState("");
  const { busy, error, run } = useAction(groups.reload);
  const have = new Set((groups.data ?? []).map((g) => g.GroupName));
  const choices = (all ?? []).filter((g) => !have.has(g.GroupName));
  return (
    <Section
      title="Groups"
      aside={
        !adding && (
          <button
            type="button"
            onClick={() => {
              setAdding(true);
              iamApi.listGroups().then(setAll, () => setAll([]));
            }}
            className={btn}
          >
            <Plus className="size-3.5" /> Add to group
          </button>
        )
      }
    >
      <ErrorNote error={error ?? groups.error} />
      {adding && (
        <form
          className="rise mb-3 flex flex-wrap gap-2"
          onSubmit={async (e) => {
            e.preventDefault();
            if (!pick) return;
            const r = await run("add", () => iamApi.addUserToGroup(pick, user));
            if (r) {
              setPick("");
              setAdding(false);
            }
          }}
        >
          <select value={pick} onChange={(e) => setPick(e.target.value)} aria-label="Group" className={`${input} min-w-0 flex-1 font-mono text-[12px]`}>
            <option value="">{all === null ? "Loading groups…" : choices.length ? "Choose a group" : "No other groups"}</option>
            {choices.map((g) => (
              <option key={g.GroupName} value={g.GroupName}>
                {g.GroupName}
              </option>
            ))}
          </select>
          <button type="submit" disabled={!pick || busy !== null} className={primary}>
            {busy === "add" ? <span className="shimmer">Adding…</span> : "Add"}
          </button>
          <button type="button" onClick={() => setAdding(false)} className={btn}>
            Cancel
          </button>
        </form>
      )}
      {groups.data === null ? (
        <Loading />
      ) : groups.data.length === 0 ? (
        <Muted>Not in any group.</Muted>
      ) : (
        <ul>
          {groups.data.map((g) => (
            <Row
              key={g.GroupName}
              actions={
                <DeleteButton
                  label={`Remove from ${g.GroupName}`}
                  confirm="Remove"
                  busy={busy === `rm-${g.GroupName}`}
                  onConfirm={() => run(`rm-${g.GroupName}`, () => iamApi.removeUserFromGroup(g.GroupName, user))}
                />
              }
            >
              <p className="flex items-center gap-2 truncate font-mono text-[13px]">
                <Users className="size-3.5 shrink-0 text-subtle" />
                {g.GroupName}
              </p>
            </Row>
          ))}
        </ul>
      )}
    </Section>
  );
}
