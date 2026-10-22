"use client";

import { Ban, KeyRound, Pencil, Plus, ShieldCheck, Trash2, X } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import {
  agoMs,
  durationLabel,
  formatDate,
  MAX_SESSION_OPTIONS,
  PERMISSIONS,
  policyError,
  sessionDurations,
  sessionEnv,
  storageApi,
  trustSummary,
  type AccessKey,
  type Bucket,
  type OidcProvider,
  type Permission,
  type Role,
  type RoleSpec,
  type Session,
} from "@/lib/storage-api";
import { iconBtn } from "../tasks/ui";
import { btn, CopyButton, ErrorNote, input, Panel, primary, useCopy } from "../ui";
import { Field, InlineConfirm, Segmented, useAction } from "./shared";

const csv = (v: string[]) => v.join(", ");
const parseCsv = (v: string) =>
  v
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean);

/** Roles: short-lived, scoped credentials for browsers, CI and services (STS, or minted here). */
export function RolesTab({ buckets }: { buckets: Bucket[] }) {
  const [roles, setRoles] = useState<Role[] | null>(null);
  const [keys, setKeys] = useState<AccessKey[]>([]);
  const [loadErr, setLoadErr] = useState<string | null>(null);
  const refresh = useCallback(async () => {
    try {
      const [r, k] = await Promise.all([storageApi.roles(), storageApi.keys()]);
      setRoles(r);
      setKeys(k);
      setLoadErr(null);
    } catch (e) {
      setLoadErr(e instanceof Error ? e.message : String(e));
    }
  }, []);
  useEffect(() => {
    refresh();
  }, [refresh]);
  const { busy, error, run } = useAction(refresh);
  const [editing, setEditing] = useState<Role | null>(null);
  const [minting, setMinting] = useState<string | null>(null);
  const [confirming, setConfirming] = useState<{ name: string; what: "delete" | "revoke" } | null>(null);
  const [session, setSession] = useState<Session | null>(null);

  return (
    <div className="space-y-4">
      <ErrorNote error={error ?? loadErr} />
      {session && <NewSession s={session} onDone={() => setSession(null)} />}
      <div className="grid gap-4 lg:grid-cols-5">
        <Panel title="Roles" aside={<span className="font-mono text-[12px] text-subtle">{roles?.length ?? "—"}</span>} className="lg:col-span-3">
          {roles === null ? (
            <p className="px-5 py-6 text-[13px] text-subtle">
              <span className="shimmer">Loading roles…</span>
            </p>
          ) : roles.length === 0 ? (
            <p className="px-5 py-6 text-[13px] text-muted">
              No roles yet. A role hands out short-lived credentials (<span className="font-mono">AWS_SESSION_TOKEN</span>) instead of a
              long-lived key: to a browser through your backend, to CI through OIDC, or to another key through AssumeRole.
            </p>
          ) : (
            <ul>
              {roles.map((r) => (
                <li key={r.id} className="rise border-b border-line px-5 py-3 last:border-0">
                  <div className="flex items-start gap-3">
                    <ShieldCheck className="mt-0.5 size-4 shrink-0 text-subtle" />
                    <div className="min-w-0 flex-1 space-y-0.5">
                      <p className="flex items-center gap-2 truncate text-[13px]">
                        <span className="font-mono">{r.name}</span>
                        <span className="rounded-md border border-line px-1.5 py-px font-mono text-[10px] text-muted">{r.permission}</span>
                        {r.description && <span className="truncate text-[12px] text-subtle">{r.description}</span>}
                      </p>
                      <p className="truncate font-mono text-[11px] text-subtle" title={r.arn}>
                        {r.arn}
                      </p>
                      <p className="truncate text-[12px] text-muted">
                        {r.buckets ? r.buckets.join(", ") : "all buckets"} · {trustSummary(r.trust)} · up to {durationLabel(r.max_session_s)}
                        {r.policy && " · inline policy"}
                        {r.revoked_before > 0 && ` · revoked ${agoMs(r.revoked_before)}`}
                      </p>
                    </div>
                    {confirming?.name === r.name ? (
                      <InlineConfirm
                        label={confirming.what === "delete" ? "Delete" : "Revoke sessions"}
                        busy={busy !== null}
                        onConfirm={async () => {
                          await run<unknown>(`${confirming.what}-${r.name}`, () =>
                            confirming.what === "delete" ? storageApi.deleteRole(r.name) : storageApi.revokeRole(r.name),
                          );
                          if (confirming.what === "delete" && editing?.name === r.name) setEditing(null);
                          setConfirming(null);
                        }}
                        onCancel={() => setConfirming(null)}
                      />
                    ) : (
                      <span className="flex shrink-0 items-center gap-0.5">
                        <button type="button" onClick={() => setMinting(minting === r.name ? null : r.name)} className={btn}>
                          <KeyRound className="size-3.5" /> Session
                        </button>
                        <button type="button" onClick={() => setEditing(r)} className={iconBtn} title="Edit" aria-label={`Edit ${r.name}`}>
                          <Pencil className="size-3.5" />
                        </button>
                        <button
                          type="button"
                          onClick={() => setConfirming({ name: r.name, what: "revoke" })}
                          className={`${iconBtn} hover:text-danger`}
                          title="Revoke its sessions"
                          aria-label={`Revoke sessions of ${r.name}`}
                        >
                          <Ban className="size-3.5" />
                        </button>
                        <button
                          type="button"
                          onClick={() => setConfirming({ name: r.name, what: "delete" })}
                          className={`${iconBtn} hover:text-danger`}
                          title="Delete role"
                          aria-label={`Delete ${r.name}`}
                        >
                          <Trash2 className="size-3.5" />
                        </button>
                      </span>
                    )}
                  </div>
                  {confirming?.name === r.name && (
                    <p className="rise mt-2 text-[12px] text-muted">
                      {confirming.what === "delete"
                        ? "Its sessions stop working, and the role can't be assumed any more."
                        : "Every session issued until now stops working; new ones can still be minted."}
                    </p>
                  )}
                  {minting === r.name && (
                    <MintSession
                      role={r}
                      busy={busy === `mint-${r.name}`}
                      onCancel={() => setMinting(null)}
                      onMint={async (req) => {
                        const s = await run(`mint-${r.name}`, () => storageApi.mintSession(r.name, req));
                        if (s) {
                          setSession(s);
                          setMinting(null);
                        }
                      }}
                    />
                  )}
                </li>
              ))}
            </ul>
          )}
        </Panel>
        <RoleEditor
          key={editing?.name ?? "new"}
          role={editing}
          buckets={buckets}
          keys={keys}
          busy={busy === "save"}
          onCancel={editing ? () => setEditing(null) : undefined}
          onSave={async (name, spec) => {
            const r = await run("save", () => (editing ? storageApi.updateRole(name, spec) : storageApi.createRole(name, spec)));
            if (r) setEditing(null);
            return !!r;
          }}
        />
      </div>
      <SessionKeys />
    </div>
  );
}

function MintSession({
  role,
  busy,
  onMint,
  onCancel,
}: {
  role: Role;
  busy: boolean;
  onMint: (req: { duration_s: number; permission?: Permission; session_name?: string }) => void;
  onCancel: () => void;
}) {
  const durations = sessionDurations(role.max_session_s);
  const [duration, setDuration] = useState(durations.includes(3600) ? 3600 : (durations.at(-1) ?? role.max_session_s));
  const allowed = PERMISSIONS.slice(0, PERMISSIONS.findIndex((p) => p.value === role.permission) + 1);
  const [permission, setPermission] = useState<Permission>(role.permission);
  const [name, setName] = useState("");
  return (
    <form
      className="rise mt-3 space-y-3 rounded-lg border border-line bg-bg p-4"
      onSubmit={(e) => {
        e.preventDefault();
        onMint({
          duration_s: duration,
          ...(permission !== role.permission ? { permission } : {}),
          ...(name.trim() ? { session_name: name.trim() } : {}),
        });
      }}
    >
      <div className="grid gap-3 sm:grid-cols-3">
        <Field label="Lasts">
          <select value={duration} onChange={(e) => setDuration(Number(e.target.value))} className={`${input} w-full`}>
            {durations.map((d) => (
              <option key={d} value={d}>
                {durationLabel(d)}
              </option>
            ))}
          </select>
        </Field>
        <Field label="Permission" hint="at most the role's">
          <Segmented value={permission} options={allowed.map((p) => ({ label: p.label, value: p.value }))} onChange={setPermission} />
        </Field>
        <Field label="Session name" hint="shown in its ARN">
          <input
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="roll-control-plane"
            spellCheck={false}
            className={`${input} w-full font-mono text-[12px]`}
          />
        </Field>
      </div>
      <div className="flex gap-2">
        <button type="submit" disabled={busy} className={primary}>
          {busy ? <span className="shimmer">Minting…</span> : "Mint credentials"}
        </button>
        <button type="button" onClick={onCancel} className={btn}>
          Cancel
        </button>
      </div>
    </form>
  );
}

/** A session's credentials: shown here once. */
function NewSession({ s, onDone }: { s: Session; onDone: () => void }) {
  const { copied, copy } = useCopy();
  const env = sessionEnv(s);
  return (
    <section className="rise rounded-xl border border-accent/40 bg-panel">
      <div className="flex min-h-14 flex-wrap items-center justify-between gap-3 border-b border-line px-5 py-3">
        <div>
          <h2 className="text-[15px] font-medium">Temporary credentials</h2>
          <p className="text-[12px] text-muted">
            {s.permission} · {s.buckets ? s.buckets.join(", ") : "all the role's buckets"} · until {formatDate(s.expiration)}. Copy them
            now — they aren&apos;t shown again.
          </p>
        </div>
        <button onClick={onDone} className={btn}>
          Done
        </button>
      </div>
      <div className="space-y-3 px-5 py-4">
        <p className="truncate font-mono text-[11px] text-subtle" title={s.assumed_role_arn}>
          {s.assumed_role_arn}
        </p>
        <div className="overflow-hidden rounded-lg border border-line bg-bg">
          <div className="flex items-center justify-between border-b border-line px-3 py-1.5">
            <span className="font-mono text-[11px] text-subtle">.env</span>
            <CopyButton id="session-env" text={`${env}\n`} copied={copied} onCopy={copy} />
          </div>
          <pre className="overflow-x-auto px-4 py-3 font-mono text-[12px] leading-relaxed text-muted">{env}</pre>
        </div>
      </div>
    </section>
  );
}

interface OidcDraft {
  issuer: string;
  audiences: string;
  subject: string;
  jwks_uri: string;
}

const toOidcDraft = (p: OidcProvider): OidcDraft => ({ issuer: p.issuer, audiences: csv(p.audiences), subject: p.subject ?? "", jwks_uri: p.jwks_uri ?? "" });
const fromOidcDraft = (d: OidcDraft): OidcProvider => ({
  issuer: d.issuer.trim(),
  audiences: parseCsv(d.audiences),
  ...(d.subject.trim() ? { subject: d.subject.trim() } : {}),
  ...(d.jwks_uri.trim() ? { jwks_uri: d.jwks_uri.trim() } : {}),
});

function RoleEditor({
  role,
  buckets,
  keys,
  busy,
  onSave,
  onCancel,
}: {
  role: Role | null;
  buckets: Bucket[];
  keys: AccessKey[];
  busy: boolean;
  onSave: (name: string, spec: RoleSpec) => Promise<boolean>;
  onCancel?: () => void;
}) {
  const [name, setName] = useState(role?.name ?? "");
  const [description, setDescription] = useState(role?.description ?? "");
  const [permission, setPermission] = useState<Permission>(role?.permission ?? "read");
  const [scoped, setScoped] = useState(!!role?.buckets);
  const [picked, setPicked] = useState<string[]>(role?.buckets ?? []);
  const [trustKeys, setTrustKeys] = useState<string[]>(role?.trust.keys ?? []);
  const [oidc, setOidc] = useState<OidcDraft[]>(role?.trust.oidc.map(toOidcDraft) ?? []);
  const [externalId, setExternalId] = useState(role?.trust.external_id ?? "");
  const [maxSession, setMaxSession] = useState(role?.max_session_s ?? 3600);
  const [policy, setPolicy] = useState(role?.policy ? JSON.stringify(role.policy, null, 2) : "");
  const [showPolicy, setShowPolicy] = useState(!!role?.policy);

  const pErr = policyError(policy);
  const oidcValid = oidc.every((p) => p.issuer.trim() && parseCsv(p.audiences).length > 0);
  const valid = name.trim().length > 0 && (!scoped || picked.length > 0) && !pErr && oidcValid;
  const setO = (i: number, patch: Partial<OidcDraft>) => setOidc((x) => x.map((p, j) => (j === i ? { ...p, ...patch } : p)));
  const options = MAX_SESSION_OPTIONS.includes(maxSession) ? MAX_SESSION_OPTIONS : [...MAX_SESSION_OPTIONS, maxSession].sort((a, b) => a - b);

  return (
    <Panel
      title={role ? `Edit ${role.name}` : "New role"}
      aside={
        onCancel && (
          <button type="button" onClick={onCancel} className={iconBtn} aria-label="Stop editing">
            <X className="size-3.5" />
          </button>
        )
      }
      className="lg:col-span-2"
    >
      <form
        className="space-y-4 px-5 py-4"
        onSubmit={async (e) => {
          e.preventDefault();
          if (!valid) return;
          const spec: RoleSpec = {
            description: description.trim(),
            permission,
            buckets: scoped ? picked : null,
            trust: {
              keys: trustKeys,
              oidc: oidc.map(fromOidcDraft),
              ...(externalId.trim() ? { external_id: externalId.trim() } : {}),
            },
            max_session_s: maxSession,
            policy: policy.trim() ? (JSON.parse(policy) as Record<string, unknown>) : null,
          };
          if ((await onSave(name.trim(), spec)) && !role) {
            setName("");
            setDescription("");
            setPicked([]);
            setScoped(false);
            setTrustKeys([]);
            setOidc([]);
            setExternalId("");
            setPolicy("");
            setShowPolicy(false);
          }
        }}
      >
        <div className="grid gap-3 sm:grid-cols-2">
          <Field label="Name">
            <input
              value={name}
              onChange={(e) => setName(e.target.value)}
              disabled={!!role}
              placeholder="uploads-from-browser"
              spellCheck={false}
              className={`${input} w-full font-mono text-[12px] disabled:opacity-60`}
            />
          </Field>
          <Field label="Longest session">
            <select value={maxSession} onChange={(e) => setMaxSession(Number(e.target.value))} className={`${input} w-full`}>
              {options.map((d) => (
                <option key={d} value={d}>
                  {durationLabel(d)}
                </option>
              ))}
            </select>
          </Field>
        </div>
        <Field label="Description">
          <input value={description} onChange={(e) => setDescription(e.target.value)} placeholder="optional" className={`${input} w-full text-[12px]`} />
        </Field>
        <Field label="Permission" hint={PERMISSIONS.find((p) => p.value === permission)?.hint}>
          <Segmented value={permission} options={PERMISSIONS.map((p) => ({ label: p.label, value: p.value }))} onChange={setPermission} />
        </Field>
        <Field label="Buckets">
          <Segmented
            value={scoped ? "some" : "all"}
            options={[
              { label: "all buckets", value: "all" },
              { label: "only some", value: "some" },
            ]}
            onChange={(v) => setScoped(v === "some")}
          />
        </Field>
        {scoped && (
          <Picker
            empty="No buckets to pick yet."
            items={buckets.map((b) => ({ value: b.name, label: b.name }))}
            picked={picked}
            onChange={setPicked}
          />
        )}

        <div className="space-y-3 border-t border-line pt-4">
          <div>
            <p className="text-[13px]">Trust</p>
            <p className="text-[12px] text-subtle">Who may assume it through STS. Sessions minted here need no trust.</p>
          </div>
          <Field label="Access keys (AssumeRole)">
            <Picker
              empty="No access keys."
              items={keys.map((k) => ({ value: k.id, label: k.name, hint: k.id }))}
              picked={trustKeys}
              onChange={setTrustKeys}
            />
          </Field>
          {oidc.map((p, i) => (
            <div key={i} className="rise space-y-2 rounded-lg border border-line bg-bg p-3">
              <div className="flex items-center justify-between">
                <span className="font-mono text-[11px] text-subtle">OIDC provider {i + 1}</span>
                <button type="button" onClick={() => setOidc((x) => x.filter((_, j) => j !== i))} className={`${iconBtn} hover:text-danger`} aria-label={`Remove provider ${i + 1}`}>
                  <X className="size-3.5" />
                </button>
              </div>
              <Field label="Issuer" hint="exactly the tokens' iss">
                <input
                  value={p.issuer}
                  onChange={(e) => setO(i, { issuer: e.target.value })}
                  placeholder="https://token.actions.githubusercontent.com"
                  spellCheck={false}
                  className={`${input} w-full font-mono text-[12px]`}
                />
              </Field>
              <Field label="Audiences" hint="comma separated client ids">
                <input
                  value={p.audiences}
                  onChange={(e) => setO(i, { audiences: e.target.value })}
                  placeholder="sts.amazonaws.com"
                  spellCheck={false}
                  className={`${input} w-full font-mono text-[12px]`}
                />
              </Field>
              <div className="grid gap-2 sm:grid-cols-2">
                <Field label="Subject" hint="* and ? match; empty: any">
                  <input
                    value={p.subject}
                    onChange={(e) => setO(i, { subject: e.target.value })}
                    placeholder="repo:acme/web:*"
                    spellCheck={false}
                    className={`${input} w-full font-mono text-[12px]`}
                  />
                </Field>
                <Field label="JWKS URI" hint="default: discovery">
                  <input
                    value={p.jwks_uri}
                    onChange={(e) => setO(i, { jwks_uri: e.target.value })}
                    placeholder="optional"
                    spellCheck={false}
                    className={`${input} w-full font-mono text-[12px]`}
                  />
                </Field>
              </div>
            </div>
          ))}
          <button type="button" onClick={() => setOidc((x) => [...x, { issuer: "", audiences: "", subject: "", jwks_uri: "" }])} className={btn}>
            <Plus className="size-3.5" /> OIDC provider
          </button>
          <Field label="External id" hint="AssumeRole must pass it; for third parties">
            <input value={externalId} onChange={(e) => setExternalId(e.target.value)} placeholder="optional" spellCheck={false} className={`${input} w-full font-mono text-[12px]`} />
          </Field>
        </div>

        <div className="space-y-2 border-t border-line pt-4">
          {showPolicy ? (
            <Field label="Inline policy" hint="an IAM identity policy (no Principal) every session is also limited by">
              <textarea
                value={policy}
                onChange={(e) => setPolicy(e.target.value)}
                rows={8}
                spellCheck={false}
                placeholder={'{\n  "Version": "2012-10-17",\n  "Statement": [ … ]\n}'}
                className={`${input} block w-full resize-y font-mono text-[12px] leading-relaxed`}
              />
            </Field>
          ) : (
            <button type="button" onClick={() => setShowPolicy(true)} className={btn}>
              <Plus className="size-3.5" /> Inline policy
            </button>
          )}
          {pErr && <p className="text-[12px] text-danger">{pErr}</p>}
        </div>

        {!oidcValid && <p className="text-[12px] text-danger">Every OIDC provider needs an issuer and at least one audience.</p>}
        {role && <p className="text-[12px] text-subtle">Saving ends the role&apos;s current sessions.</p>}
        <button type="submit" disabled={busy || !valid} className={primary}>
          {busy ? (
            <span className="shimmer">Saving…</span>
          ) : role ? (
            "Save role"
          ) : (
            <>
              <Plus className="size-3.5" /> Create role
            </>
          )}
        </button>
      </form>
    </Panel>
  );
}

function Picker({
  items,
  picked,
  onChange,
  empty,
}: {
  items: { value: string; label: string; hint?: string }[];
  picked: string[];
  onChange: (v: string[]) => void;
  empty: string;
}) {
  if (items.length === 0) return <p className="text-[12px] text-subtle">{empty}</p>;
  return (
    <div className="max-h-40 space-y-1 overflow-y-auto rounded-lg border border-line bg-bg p-2">
      {items.map((it) => (
        <label key={it.value} className="flex cursor-pointer items-center gap-2 rounded px-1.5 py-1 text-[12px] hover:bg-panel-2">
          <input
            type="checkbox"
            checked={picked.includes(it.value)}
            onChange={(e) => onChange(e.target.checked ? [...picked, it.value] : picked.filter((x) => x !== it.value))}
            className="accent-accent"
          />
          <span className="font-mono">{it.label}</span>
          {it.hint && <span className="truncate font-mono text-[11px] text-subtle">{it.hint}</span>}
        </label>
      ))}
    </div>
  );
}

/** The deployment key that seals session tokens. */
function SessionKeys() {
  const { busy, error, run } = useAction(async () => {});
  const [confirming, setConfirming] = useState(false);
  const [rotated, setRotated] = useState<string | null>(null);
  return (
    <Panel title="Session-token key">
      <div className="space-y-3 px-5 py-4">
        <p className="text-[13px] text-muted">
          Session tokens are sealed with a deployment key, so nothing is stored per session. Rotating makes a new key: tokens of the
          previous key keep working, older ones stop. Rotate at most once per 36 hours to never cut a live session short.
        </p>
        <ErrorNote error={error} />
        {rotated && <p className="rise text-[13px] text-ok">Rotated: key {rotated} is current.</p>}
        {confirming ? (
          <InlineConfirm
            label="Rotate key"
            busy={busy === "rotate"}
            onConfirm={async () => {
              const r = await run("rotate", () => storageApi.rotateSessionKey());
              if (r) setRotated(String(r.key_id));
              setConfirming(false);
            }}
            onCancel={() => setConfirming(false)}
          />
        ) : (
          <button type="button" onClick={() => setConfirming(true)} className={btn}>
            Rotate session-token key
          </button>
        )}
      </div>
    </Panel>
  );
}
