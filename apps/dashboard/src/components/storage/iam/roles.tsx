"use client";

import { Plus, X } from "lucide-react";
import { useState } from "react";
import { durationLabel, MAX_SESSION_OPTIONS } from "@/lib/storage-api";
import { iamApi, iamNameError, num, policyDocumentError, prettyPolicy, starterTrustPolicy, type IamRole } from "@/lib/storage-iam";
import { iconBtn } from "../../tasks/ui";
import { btn, ErrorNote, input, Panel, primary } from "../../ui";
import { Field, SaveBar, useAction } from "../shared";
import {
  AttachedPolicies,
  DetailHeader,
  EntityList,
  InlinePolicies,
  Loading,
  MasterDetail,
  NothingSelected,
  PolicyDocForm,
  PolicyEditor,
  Section,
  TagsSection,
  useLoad,
  When,
} from "./common";

const sessionOptions = (current: number) =>
  MAX_SESSION_OPTIONS.includes(current) ? MAX_SESSION_OPTIONS : [...MAX_SESSION_OPTIONS, current].sort((a, b) => a - b);

/** IAM roles: identities with a trust policy (who may assume them) and permission policies. */
export function IamRolesTab({ accountId }: { accountId: string }) {
  const roles = useLoad("iam-roles", iamApi.listRoles);
  const [selected, setSelected] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const current = roles.data?.some((r) => r.RoleName === selected) ? selected : null;
  return (
    <>
      <ErrorNote error={roles.error} />
      <div className="mt-4">
        <MasterDetail
          list={
            <Panel
              title="Roles"
              aside={
                <span className="flex items-center gap-2">
                  <span className="font-mono text-[12px] text-subtle">{roles.data?.length ?? "—"}</span>
                  {!creating && (
                    <button
                      type="button"
                      onClick={() => {
                        setCreating(true);
                        setSelected(null);
                      }}
                      className={btn}
                    >
                      <Plus className="size-3.5" /> New
                    </button>
                  )}
                </span>
              }
            >
              <EntityList
                items={roles.data}
                keyOf={(r) => r.RoleName}
                selected={current}
                onSelect={(k) => {
                  setSelected(k);
                  setCreating(false);
                }}
                title={(r) => r.RoleName}
                meta={(r) => (
                  <>
                    {r.Description ? `${r.Description} · ` : ""}up to {durationLabel(num(r.MaxSessionDuration) ?? 3600)} · created <When iso={r.CreateDate} />
                  </>
                )}
                empty="No IAM roles yet. A role has no keys of its own: users, services and other accounts assume it (STS AssumeRole) for short-lived credentials."
              />
            </Panel>
          }
          detail={
            creating ? (
              <CreateRole
                accountId={accountId}
                onCancel={() => setCreating(false)}
                onCreated={async (r) => {
                  setCreating(false);
                  await roles.reload();
                  setSelected(r.RoleName);
                }}
              />
            ) : current ? (
              <RoleDetail
                key={current}
                name={current}
                onChanged={roles.reload}
                onDeleted={async () => {
                  setSelected(null);
                  await roles.reload();
                }}
              />
            ) : (
              <NothingSelected>Choose a role to see its trust policy and permissions.</NothingSelected>
            )
          }
        />
      </div>
    </>
  );
}

function CreateRole({ accountId, onCreated, onCancel }: { accountId: string; onCreated: (r: IamRole) => Promise<void>; onCancel: () => void }) {
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [maxSession, setMaxSession] = useState(3600);
  const [trust, setTrust] = useState(starterTrustPolicy(accountId));
  const { busy, error, setError, run } = useAction(async () => {});
  const nameErr = name ? iamNameError("role", name) : null;
  const ok = name && !nameErr && !policyDocumentError(trust);
  return (
    <Panel
      title="New role"
      aside={
        <button type="button" onClick={onCancel} className={iconBtn} aria-label="Cancel" title="Cancel">
          <X className="size-3.5" />
        </button>
      }
    >
      <form
        className="space-y-4 px-5 py-4"
        onSubmit={async (e) => {
          e.preventDefault();
          if (!ok) return;
          const r = await run("create", () =>
            iamApi.createRole({ RoleName: name, AssumeRolePolicyDocument: trust, Description: description.trim() || undefined, MaxSessionDuration: maxSession }),
          );
          if (r) await onCreated(r);
        }}
      >
        <div className="grid gap-3 sm:grid-cols-2">
          <Field label="Name">
            <input value={name} onChange={(e) => setName(e.target.value)} placeholder="ci-deploy" spellCheck={false} className={`${input} w-full font-mono text-[12px]`} />
          </Field>
          <Field label="Longest session">
            <select value={maxSession} onChange={(e) => setMaxSession(Number(e.target.value))} className={`${input} w-full`}>
              {sessionOptions(maxSession).map((s) => (
                <option key={s} value={s}>
                  {durationLabel(s)}
                </option>
              ))}
            </select>
          </Field>
        </div>
        {nameErr && <p className="text-[12px] text-danger">{nameErr}</p>}
        <Field label="Description" hint="optional">
          <input value={description} onChange={(e) => setDescription(e.target.value)} className={`${input} w-full`} />
        </Field>
        <div className="space-y-1">
          <p className="text-[11px] text-subtle">Trust policy — who may assume the role</p>
          <PolicyEditor
            value={trust}
            onChange={(v) => {
              setTrust(v);
              setError(null);
            }}
            serverError={error}
            label="Trust policy JSON"
          />
        </div>
        <p className="text-[12px] text-subtle">Attach permission policies once it exists.</p>
        <div className="flex gap-2">
          <button type="submit" disabled={!ok || busy !== null} className={primary}>
            {busy ? <span className="shimmer">Creating…</span> : "Create role"}
          </button>
          <button type="button" onClick={onCancel} className={btn}>
            Cancel
          </button>
        </div>
      </form>
    </Panel>
  );
}

function RoleDetail({ name, onChanged, onDeleted }: { name: string; onChanged: () => Promise<void>; onDeleted: () => Promise<void> }) {
  const role = useLoad(`iam-role:${name}`, () => iamApi.getRole(name));
  const { busy, error, run } = useAction(async () => {
    await role.reload();
    await onChanged();
  });
  const r = role.data;
  return (
    <section className="rounded-xl border border-line bg-panel">
      <DetailHeader
        name={name}
        arn={r?.Arn ?? "…"}
        meta={
          r && (
            <>
              <span className="font-mono text-[11px] text-subtle">{r.RoleId}</span> · created <When iso={r.CreateDate} />
            </>
          )
        }
        busy={busy === "delete"}
        deleteHint="IAM only deletes a role without policies."
        onDelete={async () => {
          const x = await run("delete", () => iamApi.deleteRole(name));
          if (x) await onDeleted();
        }}
      />
      {(error || role.error) && (
        <div className="px-5 pb-1">
          <ErrorNote error={error ?? role.error} />
        </div>
      )}
      {!r ? (
        !role.error && (
          <div className="px-5 py-4">
            <Loading />
          </div>
        )
      ) : (
        <>
          <RoleSettings key={`${r.Description}:${r.MaxSessionDuration}`} role={r} busy={busy === "settings"} onSave={(p) => run("settings", () => iamApi.updateRole(name, p))} />
          <Section title="Trust policy">
            <PolicyDocForm
              key={r.AssumeRolePolicyDocument ?? ""}
              initial={prettyPolicy(r.AssumeRolePolicyDocument)}
              label="Trust policy JSON"
              saveLabel="Save trust policy"
              onSave={async (doc) => {
                await iamApi.updateAssumeRolePolicy(name, doc);
                await role.reload();
              }}
            />
          </Section>
        </>
      )}
      <AttachedPolicies target={{ kind: "role", name }} />
      <InlinePolicies target={{ kind: "role", name }} />
      <TagsSection target={{ kind: "role", name }} />
    </section>
  );
}

function RoleSettings({ role, busy, onSave }: { role: IamRole; busy: boolean; onSave: (p: { Description: string; MaxSessionDuration: number }) => Promise<unknown> }) {
  const savedDescription = role.Description ?? "";
  const savedMax = num(role.MaxSessionDuration) ?? 3600;
  const [description, setDescription] = useState(savedDescription);
  const [maxSession, setMaxSession] = useState(savedMax);
  const dirty = description !== savedDescription || maxSession !== savedMax;
  return (
    <Section title="Settings">
      <form
        className="space-y-4"
        onSubmit={(e) => {
          e.preventDefault();
          onSave({ Description: description, MaxSessionDuration: maxSession });
        }}
      >
        <div className="grid gap-3 sm:grid-cols-2">
          <Field label="Description">
            <input value={description} onChange={(e) => setDescription(e.target.value)} className={`${input} w-full`} />
          </Field>
          <Field label="Longest session">
            <select value={maxSession} onChange={(e) => setMaxSession(Number(e.target.value))} className={`${input} w-full`}>
              {sessionOptions(maxSession).map((s) => (
                <option key={s} value={s}>
                  {durationLabel(s)}
                </option>
              ))}
            </select>
          </Field>
        </div>
        <SaveBar
          dirty={dirty}
          busy={busy}
          onReset={() => {
            setDescription(savedDescription);
            setMaxSession(savedMax);
          }}
        />
      </form>
    </Section>
  );
}
