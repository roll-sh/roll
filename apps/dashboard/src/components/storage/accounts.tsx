"use client";

import { Building2, Pencil, Plus, Trash2 } from "lucide-react";
import Link from "next/link";
import { useState } from "react";
import { agoMs, formatDate } from "@/lib/storage-api";
import { accountsApi, setAccount, type CreatedAccount, type StorageAccount } from "@/lib/storage-session";
import { AppShell } from "../app-shell";
import { iconBtn } from "../tasks/ui";
import { btn, Connecting, ErrorNote, input, Offline, Panel, primary, SecretField, useCopy } from "../ui";
import { IAM_HREF, OperatorOnlyNote, StorageIdentity, useAccounts, useActingAccount, useConsoleSession } from "./console";
import { Check, Field, InlineConfirm, useAction } from "./shared";

/** Operator: the deployment's storage accounts — create, rename, delete, and which one the dashboard acts in. */
export function StorageAccounts() {
  const session = useConsoleSession();
  const acting = useActingAccount();
  const { accounts, error: loadErr, offline, reload } = useAccounts(!session);
  const { busy, error, run } = useAction(async () => reload());
  const [creating, setCreating] = useState(false);
  const [created, setCreated] = useState<CreatedAccount | null>(null);
  const [editing, setEditing] = useState<string | null>(null);
  const [confirming, setConfirming] = useState<string | null>(null);
  const actingId = acting ?? accounts?.find((a) => a.default)?.id ?? null;

  return (
    <AppShell
      crumb={
        <>
          <Link href="/dashboard/storage" className="text-muted hover:text-fg">
            storage
          </Link>{" "}
          <span className="text-subtle">/</span> accounts
        </>
      }
      actions={
        !session && (
          <button onClick={() => setCreating((c) => !c)} disabled={!accounts} className={primary}>
            <Plus className="size-3.5" /> New account
          </button>
        )
      }
    >
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <h1 className="text-2xl font-semibold tracking-[-0.04em]">Accounts</h1>
          <p className="mt-1 text-[13px] text-muted">
            Each account owns its buckets, keys and IAM users, like an AWS account. Bucket names are shared by all of them.
          </p>
        </div>
        <StorageIdentity />
      </div>

      {session ? (
        <OperatorOnlyNote what="Managing accounts" />
      ) : offline ? (
        <Offline />
      ) : accounts === null && !loadErr ? (
        <Connecting />
      ) : (
        <div className="mt-6 space-y-4">
          <ErrorNote error={error ?? loadErr} />
          {created && <NewAccount a={created} onDone={() => setCreated(null)} />}
          {creating && (
            <CreateAccount
              busy={busy === "create"}
              onCancel={() => setCreating(false)}
              onCreate={async (name, email, rootKey) => {
                const a = await run("create", () => accountsApi.create(name, { email, root_key: rootKey }));
                if (a) {
                  setCreating(false);
                  setCreated(a);
                }
              }}
            />
          )}
          {accounts && (
            <Panel title="Accounts" aside={<span className="font-mono text-[12px] text-subtle">{accounts.length}</span>}>
              <ul>
                {accounts.map((a) => (
                  <li key={a.id} className="rise border-b border-line px-5 py-3 last:border-0">
                    <div className="flex items-start gap-3">
                      <span
                        className={`mt-1.5 inline-block size-2 shrink-0 rounded-full ${a.id === actingId ? "bg-accent" : "border border-line-2"}`}
                        title={a.id === actingId ? "The dashboard acts in this account" : undefined}
                      />
                      <div className="min-w-0 flex-1 space-y-0.5">
                        <p className="flex flex-wrap items-center gap-2 text-[13px]">
                          <span className="font-medium">{a.name}</span>
                          {a.default && <span className="rounded-md border border-line px-1.5 py-px font-mono text-[10px] text-muted">default</span>}
                          {a.alias && <span className="font-mono text-[12px] text-muted">{a.alias}</span>}
                        </p>
                        <p className="truncate font-mono text-[11px] text-subtle">
                          {a.id} · {a.email ?? "no email"} · created <span title={formatDate(a.created_at)}>{agoMs(a.created_at)}</span>
                        </p>
                        <p className="truncate font-mono text-[11px] text-subtle" title="Canonical user id, as in ACLs">
                          {a.canonical}
                        </p>
                      </div>
                      {confirming === a.id ? (
                        <InlineConfirm
                          label="Delete account"
                          busy={busy === `del-${a.id}`}
                          onConfirm={async () => {
                            const r = await run(`del-${a.id}`, () => accountsApi.remove(a.id));
                            if (r && acting === a.id) setAccount(null);
                            setConfirming(null);
                          }}
                          onCancel={() => setConfirming(null)}
                        />
                      ) : (
                        <span className="flex shrink-0 items-center gap-0.5">
                          {a.id !== actingId && (
                            <button type="button" onClick={() => setAccount(a.default ? null : a.id)} className={btn}>
                              Act in
                            </button>
                          )}
                          {a.id === actingId && (
                            <Link href={IAM_HREF} className={btn}>
                              IAM
                            </Link>
                          )}
                          <button
                            type="button"
                            onClick={() => setEditing(editing === a.id ? null : a.id)}
                            className={iconBtn}
                            title="Edit"
                            aria-label={`Edit ${a.name}`}
                          >
                            <Pencil className="size-3.5" />
                          </button>
                          {!a.default && (
                            <button
                              type="button"
                              onClick={() => setConfirming(a.id)}
                              className={`${iconBtn} hover:text-danger`}
                              title="Delete account"
                              aria-label={`Delete ${a.name}`}
                            >
                              <Trash2 className="size-3.5" />
                            </button>
                          )}
                        </span>
                      )}
                    </div>
                    {confirming === a.id && (
                      <p className="rise mt-2 pl-5 text-[12px] text-muted">
                        Only an account without buckets can be deleted; its keys, roles and IAM users go with it.
                      </p>
                    )}
                    {editing === a.id && (
                      <EditAccount
                        a={a}
                        busy={busy === `edit-${a.id}`}
                        onCancel={() => setEditing(null)}
                        onSave={async (patch) => {
                          const r = await run(`edit-${a.id}`, () => accountsApi.update(a.id, patch));
                          if (r) setEditing(null);
                        }}
                      />
                    )}
                  </li>
                ))}
              </ul>
              <p className="border-t border-line px-5 py-3 text-[12px] text-subtle">
                The dashboard acts in the account marked <span className="inline-block size-1.5 rounded-full bg-accent align-middle" /> (sent as{" "}
                <span className="font-mono">x-roll-account</span>). Aliases are set in each account&apos;s{" "}
                <Link href={`${IAM_HREF}#account`} className="text-accent hover:underline">
                  IAM settings
                </Link>
                .
              </p>
            </Panel>
          )}
        </div>
      )}
    </AppShell>
  );
}

function CreateAccount({
  busy,
  onCreate,
  onCancel,
}: {
  busy: boolean;
  onCreate: (name: string, email: string, rootKey: boolean) => void;
  onCancel: () => void;
}) {
  const [name, setName] = useState("");
  const [email, setEmail] = useState("");
  const [rootKey, setRootKey] = useState(true);
  const emailErr = email.trim() && !/^[^\s@]+@[^\s@]+$/.test(email.trim()) ? "Enter an email address, or leave it empty." : null;
  const valid = name.trim().length > 0 && !emailErr;
  return (
    <Panel title="New account">
      <form
        className="space-y-4 px-5 py-4"
        onSubmit={(e) => {
          e.preventDefault();
          if (valid) onCreate(name.trim(), email, rootKey);
        }}
      >
        <div className="grid gap-3 sm:grid-cols-2">
          <Field label="Name" hint="Shown as the owner in ACLs and listings">
            <input autoFocus value={name} onChange={(e) => setName(e.target.value)} placeholder="acme-prod" className={`${input} w-full`} />
          </Field>
          <Field label="Email" hint="optional; ACL grants by email resolve to it">
            <input value={email} onChange={(e) => setEmail(e.target.value)} placeholder="ops@acme.dev" type="email" className={`${input} w-full`} />
          </Field>
        </div>
        {emailErr && <p className="text-[12px] text-danger">{emailErr}</p>}
        <Check checked={rootKey} onChange={setRootKey} label="Create a root access key" hint="admin over the account's buckets; the secret is shown once" />
        <div className="flex gap-2">
          <button type="submit" disabled={busy || !valid} className={primary}>
            {busy ? <span className="shimmer">Creating…</span> : "Create account"}
          </button>
          <button type="button" onClick={onCancel} className={btn}>
            Cancel
          </button>
        </div>
      </form>
    </Panel>
  );
}

function EditAccount({
  a,
  busy,
  onSave,
  onCancel,
}: {
  a: StorageAccount;
  busy: boolean;
  onSave: (patch: { name?: string; email?: string | null }) => void;
  onCancel: () => void;
}) {
  const [name, setName] = useState(a.name);
  const [email, setEmail] = useState(a.email ?? "");
  const patch: { name?: string; email?: string | null } = {};
  if (name.trim() !== a.name) patch.name = name.trim();
  if ((email.trim() || null) !== a.email) patch.email = email.trim() || null;
  const dirty = Object.keys(patch).length > 0;
  return (
    <form
      className="rise mt-3 ml-5 space-y-3 rounded-lg border border-line bg-bg p-4"
      onSubmit={(e) => {
        e.preventDefault();
        if (dirty && name.trim()) onSave(patch);
      }}
    >
      <div className="grid gap-3 sm:grid-cols-2">
        <Field label="Name">
          <input value={name} onChange={(e) => setName(e.target.value)} className={`${input} w-full`} />
        </Field>
        <Field label="Email" hint="empty: none">
          <input value={email} onChange={(e) => setEmail(e.target.value)} type="email" className={`${input} w-full`} />
        </Field>
      </div>
      <div className="flex gap-2">
        <button type="submit" disabled={busy || !dirty || !name.trim()} className={primary}>
          {busy ? <span className="shimmer">Saving…</span> : "Save"}
        </button>
        <button type="button" onClick={onCancel} className={btn}>
          Cancel
        </button>
      </div>
    </form>
  );
}

/** The new account, with its root key's secret: shown once. */
function NewAccount({ a, onDone }: { a: CreatedAccount; onDone: () => void }) {
  const { copied, copy } = useCopy();
  const k = a.root_key;
  return (
    <section className="rise rounded-xl border border-accent/40 bg-panel">
      <div className="flex min-h-14 flex-wrap items-center justify-between gap-3 border-b border-line px-5 py-3">
        <div className="flex items-center gap-3">
          <Building2 className="size-4 text-subtle" />
          <div>
            <h2 className="text-[15px] font-medium">Account {a.name} created</h2>
            <p className="text-[12px] text-muted">
              <span className="font-mono">{a.id}</span>
              {k && " · copy the root key's secret now — it isn't shown again."}
            </p>
          </div>
        </div>
        <span className="flex gap-2">
          <button
            type="button"
            onClick={() => {
              setAccount(a.default ? null : a.id);
              onDone();
            }}
            className={btn}
          >
            Act in it
          </button>
          <button type="button" onClick={onDone} className={btn}>
            Done
          </button>
        </span>
      </div>
      {k && (
        <div className="grid gap-3 px-5 py-4 sm:grid-cols-2">
          <Field label="Root access key id">
            <SecretField id="root-kid" value={k.id} masked={k.id} copied={copied} onCopy={copy} />
          </Field>
          <Field label="Secret access key">
            <SecretField
              id="root-secret"
              value={k.secret_access_key}
              masked={`${k.secret_access_key.slice(0, 4)}${"•".repeat(16)}`}
              copied={copied}
              onCopy={copy}
            />
          </Field>
        </div>
      )}
    </section>
  );
}

