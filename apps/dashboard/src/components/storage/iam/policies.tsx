"use client";

import { Eye, Plus, Star, X } from "lucide-react";
import { useState } from "react";
import {
  bool,
  iamApi,
  iamNameError,
  iamPathError,
  isAwsManaged,
  num,
  policyDocumentError,
  prettyPolicy,
  starterPolicy,
  type ManagedPolicy,
} from "@/lib/storage-iam";
import { iconBtn } from "../../tasks/ui";
import { btn, ErrorNote, input, Panel, primary } from "../../ui";
import { Check, Field, Segmented, useAction } from "../shared";
import {
  Badge,
  DeleteButton,
  DetailHeader,
  EntityList,
  Loading,
  MasterDetail,
  Muted,
  NothingSelected,
  PolicyDocForm,
  PolicyEditor,
  Section,
  useLoad,
  When,
} from "./common";

type Scope = "Local" | "AWS";

/** Managed policies: the account's own (versioned, editable) and AWS's (read-only). */
export function PoliciesTab() {
  const [scope, setScope] = useState<Scope>("Local");
  const policies = useLoad(`policies:${scope}`, () => iamApi.listPolicies(scope));
  const [selected, setSelected] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const current = policies.data?.some((p) => p.Arn === selected) ? selected : null;
  return (
    <>
      <ErrorNote error={policies.error} />
      <div className="mt-4">
        <MasterDetail
          list={
            <Panel
              title="Policies"
              aside={
                scope === "Local" &&
                !creating && (
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
                )
              }
            >
              <div className="border-b border-line px-5 py-3">
                <Segmented
                  value={scope}
                  options={[
                    { label: "customer managed", value: "Local" },
                    { label: "AWS managed", value: "AWS" },
                  ]}
                  onChange={(s) => {
                    setScope(s);
                    setSelected(null);
                    setCreating(false);
                  }}
                />
              </div>
              <EntityList
                items={policies.data}
                keyOf={(p) => p.Arn}
                selected={current}
                onSelect={(k) => {
                  setSelected(k);
                  setCreating(false);
                }}
                title={(p) => p.PolicyName}
                meta={(p) => (
                  <>
                    {p.DefaultVersionId} · {num(p.AttachmentCount) ?? 0} attached
                    {p.Description ? ` · ${p.Description}` : ""}
                  </>
                )}
                empty={
                  scope === "Local"
                    ? "No customer managed policies yet. A managed policy is written once and attached to any number of users, groups and roles."
                    : "No AWS managed policies."
                }
              />
            </Panel>
          }
          detail={
            creating ? (
              <CreatePolicy
                onCancel={() => setCreating(false)}
                onCreated={async (p) => {
                  setCreating(false);
                  await policies.reload();
                  setSelected(p.Arn);
                }}
              />
            ) : current ? (
              <PolicyDetail
                key={current}
                arn={current}
                onChanged={policies.reload}
                onDeleted={async () => {
                  setSelected(null);
                  await policies.reload();
                }}
              />
            ) : (
              <NothingSelected>Choose a policy to see its document, versions and where it&apos;s attached.</NothingSelected>
            )
          }
        />
      </div>
    </>
  );
}

function CreatePolicy({ onCreated, onCancel }: { onCreated: (p: ManagedPolicy) => Promise<void>; onCancel: () => void }) {
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [path, setPath] = useState("");
  const [doc, setDoc] = useState(starterPolicy());
  const { busy, error, setError, run } = useAction(async () => {});
  const nameErr = name ? iamNameError("policy", name) : null;
  const pathErr = iamPathError(path);
  const ok = name && !nameErr && !pathErr && !policyDocumentError(doc) && description.length <= 1000;
  return (
    <Panel
      title="New policy"
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
          const p = await run("create", () =>
            iamApi.createPolicy({ PolicyName: name, PolicyDocument: doc, Description: description.trim() || undefined, Path: path || undefined }),
          );
          if (p) await onCreated(p);
        }}
      >
        <div className="grid gap-3 sm:grid-cols-2">
          <Field label="Name">
            <input value={name} onChange={(e) => setName(e.target.value)} placeholder="reports-read" spellCheck={false} className={`${input} w-full font-mono text-[12px]`} />
          </Field>
          <Field label="Path" hint="optional, e.g. /teams/">
            <input value={path} onChange={(e) => setPath(e.target.value)} placeholder="/" spellCheck={false} className={`${input} w-full font-mono text-[12px]`} />
          </Field>
        </div>
        {(nameErr || pathErr) && <p className="text-[12px] text-danger">{nameErr ?? pathErr}</p>}
        <Field label="Description" hint="optional; can't be changed later">
          <input value={description} onChange={(e) => setDescription(e.target.value)} className={`${input} w-full`} />
        </Field>
        <PolicyEditor
          value={doc}
          onChange={(v) => {
            setDoc(v);
            setError(null);
          }}
          serverError={error}
          label="Policy document JSON"
          minRows={12}
        />
        <div className="flex gap-2">
          <button type="submit" disabled={!ok || busy !== null} className={primary}>
            {busy ? <span className="shimmer">Creating…</span> : "Create policy"}
          </button>
          <button type="button" onClick={onCancel} className={btn}>
            Cancel
          </button>
        </div>
      </form>
    </Panel>
  );
}

function PolicyDetail({ arn, onChanged, onDeleted }: { arn: string; onChanged: () => Promise<void>; onDeleted: () => Promise<void> }) {
  const policy = useLoad(`policy:${arn}`, () => iamApi.getPolicy(arn));
  const versions = useLoad(`versions:${arn}`, () => iamApi.listPolicyVersions(arn));
  const def = policy.data?.DefaultVersionId;
  const document = useLoad(`doc:${arn}:${def ?? ""}`, () => (def ? iamApi.getPolicyVersion(arn, def).then((v) => prettyPolicy(v.Document)) : Promise.resolve(null)));
  const entities = useLoad(`entities:${arn}`, () => iamApi.entitiesForPolicy(arn));
  const [viewing, setViewing] = useState<{ id: string; text: string | null } | null>(null);
  const [setDefault, setSetDefault] = useState(true);
  const readOnly = isAwsManaged(arn);
  const refreshAll = async () => {
    await Promise.all([policy.reload(), versions.reload(), onChanged()]);
  };
  const { busy, error, run } = useAction(refreshAll);
  const p = policy.data;
  const view = async (id: string) => {
    if (viewing?.id === id) {
      setViewing(null);
      return;
    }
    setViewing({ id, text: null });
    try {
      const v = await iamApi.getPolicyVersion(arn, id);
      setViewing({ id, text: prettyPolicy(v.Document) });
    } catch (e) {
      setViewing({ id, text: `// ${e instanceof Error ? e.message : String(e)}` });
    }
  };
  const attached = entities.data ? entities.data.users.length + entities.data.groups.length + entities.data.roles.length : null;

  return (
    <section className="rounded-xl border border-line bg-panel">
      <DetailHeader
        name={p?.PolicyName ?? arn.split("/").pop() ?? arn}
        arn={arn}
        meta={
          p && (
            <>
              {p.Description && <span className="text-muted">{p.Description} · </span>}
              {readOnly && <Badge>AWS managed</Badge>} updated <When iso={p.UpdateDate ?? p.CreateDate} />
            </>
          )
        }
        busy={busy === "delete"}
        deleteHint="IAM only deletes a policy that isn't attached and has no versions besides the default."
        onDelete={
          readOnly
            ? undefined
            : async () => {
                const r = await run("delete", () => iamApi.deletePolicy(arn));
                if (r) await onDeleted();
              }
        }
      />
      {(error || policy.error) && (
        <div className="px-5 pb-1">
          <ErrorNote error={error ?? policy.error} />
        </div>
      )}
      <Section title={<>Document {def && <span className="ml-1 font-mono text-[11px] font-normal text-subtle">{def}</span>}</>}>
        <ErrorNote error={document.error} />
        {document.data == null ? (
          !document.error && <Loading />
        ) : readOnly ? (
          <PolicyEditor value={document.data} readOnly label="Policy document" />
        ) : (
          <PolicyDocForm
            key={`${def}:${document.data}`}
            initial={document.data}
            label="Policy document JSON"
            saveLabel={setDefault ? "Save as new default version" : "Save as new version"}
            onSave={async (doc) => {
              await iamApi.createPolicyVersion(arn, doc, setDefault);
              await refreshAll();
            }}
          >
            <Check
              checked={setDefault}
              onChange={setSetDefault}
              label="Make it the default version"
              hint="IAM keeps up to five versions; delete an old one to save a sixth."
            />
          </PolicyDocForm>
        )}
      </Section>
      <Section title="Versions">
        <ErrorNote error={versions.error} />
        {versions.data === null ? (
          !versions.error && <Loading />
        ) : (
          <ul>
            {versions.data.map((v) => {
              const isDefault = bool(v.IsDefaultVersion);
              return (
                <li key={v.VersionId} className="border-b border-line last:border-0">
                  <div className="flex min-h-9 items-center gap-3 py-1.5">
                    <p className="flex min-w-0 flex-1 items-center gap-2 text-[12px]">
                      <span className="font-mono">{v.VersionId}</span>
                      {isDefault && <Badge tone="accent">default</Badge>}
                      <span className="text-subtle">
                        <When iso={v.CreateDate} />
                      </span>
                    </p>
                    <button type="button" onClick={() => view(v.VersionId)} className={iconBtn} title="View" aria-label={`View ${v.VersionId}`}>
                      {viewing?.id === v.VersionId ? <X className="size-3.5" /> : <Eye className="size-3.5" />}
                    </button>
                    {!readOnly && !isDefault && (
                      <>
                        <button
                          type="button"
                          disabled={busy !== null}
                          onClick={() => run(`default-${v.VersionId}`, () => iamApi.setDefaultPolicyVersion(arn, v.VersionId))}
                          className={iconBtn}
                          title="Make default"
                          aria-label={`Make ${v.VersionId} the default`}
                        >
                          <Star className="size-3.5" />
                        </button>
                        <DeleteButton
                          label={`Delete ${v.VersionId}`}
                          busy={busy === `del-${v.VersionId}`}
                          onConfirm={() => run(`del-${v.VersionId}`, () => iamApi.deletePolicyVersion(arn, v.VersionId))}
                        />
                      </>
                    )}
                  </div>
                  {viewing?.id === v.VersionId && (
                    <div className="pb-3">{viewing.text === null ? <Loading /> : <PolicyEditor value={viewing.text} readOnly label={`${v.VersionId} document`} />}</div>
                  )}
                </li>
              );
            })}
          </ul>
        )}
      </Section>
      <Section title={<>Attached to {attached != null && <span className="ml-1 font-mono text-[11px] font-normal text-subtle">{attached}</span>}</>}>
        <ErrorNote error={entities.error} />
        {entities.data === null ? (
          !entities.error && <Loading />
        ) : attached === 0 ? (
          <Muted>Not attached to any user, group or role.</Muted>
        ) : (
          <dl className="space-y-1.5 text-[12px]">
            {(
              [
                ["users", entities.data.users],
                ["groups", entities.data.groups],
                ["roles", entities.data.roles],
              ] as const
            ).map(
              ([label, names]) =>
                names.length > 0 && (
                  <div key={label} className="flex gap-3">
                    <dt className="w-14 shrink-0 text-subtle">{label}</dt>
                    <dd className="min-w-0 font-mono">{names.join(", ")}</dd>
                  </div>
                ),
            )}
          </dl>
        )}
      </Section>
    </section>
  );
}
