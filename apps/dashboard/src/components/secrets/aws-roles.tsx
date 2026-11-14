import { Cloud, KeyRound, Plus, Trash2 } from "lucide-react";
import { useState } from "react";
import type { AwsRole, AwsUser } from "@/lib/secrets-api";
import { useSecrets } from "./secrets-page";
import { ActorMark, ago, button, card, dangerButton, ErrorNote, errorText, field, ghost, primary, Section, useLoad } from "./ui";

/**
 * AWS roles (docs/secrets.md, "AWS"): a role in the project's own AWS account that roll assumes for
 * each lease of a dynamic link, so a service gets AWS credentials that end with it. Registering one
 * assumes it once, and refuses a role whose trust policy doesn't pin this project's ExternalId.
 */
export function AwsRoles() {
  const { api, project } = useSecrets();
  const list = useLoad(() => api.awsRoles(), [api]);
  const [adding, setAdding] = useState(false);
  const [name, setName] = useState("");
  const [arn, setArn] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [deleting, setDeleting] = useState<string | null>(null);
  const externalId = `roll-secrets:${project.id}`;
  const ok = /^[A-Za-z0-9_-]{1,64}$/.test(name) && /^arn:aws[a-z-]*:iam::\d{12}:role\/.+/.test(arn.trim());

  const add = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.addAwsRole({ name, role_arn: arn.trim() });
      setAdding(false);
      setName("");
      setArn("");
      list.reload();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  const del = (r: AwsRole) =>
    api.deleteAwsRole(r.id).then(
      () => (setDeleting(null), list.reload()),
      (e) => (setDeleting(null), setError(errorText(e))),
    );

  return (
    <>
    <Section
      title="AWS roles"
      aside={
        <button className={button} onClick={() => setAdding((a) => !a)}>
          <Plus className="size-3.5" /> Add a role
        </button>
      }
    >
      <p className="mb-2 text-[11.5px] text-muted">
        Short-lived AWS credentials for each lease: link <span className="font-mono">AWS_ACCESS_KEY_ID</span>, <span className="font-mono">AWS_SECRET_ACCESS_KEY</span> and{" "}
        <span className="font-mono">AWS_SESSION_TOKEN</span> to a role (Link a resource, dynamic) and every resolve gets a session of its own. The role&apos;s trust policy lets roll&apos;s role assume
        it with the condition <span className="font-mono">sts:ExternalId</span> = <span className="font-mono select-all">{externalId}</span>.
      </p>
      <ErrorNote error={error ?? list.error} onDismiss={error ? () => setError(null) : undefined} className="mb-2" />
      {adding && (
        <form
          className="rise mb-3 flex flex-wrap items-end gap-2 rounded-xl border border-line-2 bg-panel p-3"
          onSubmit={(e) => {
            e.preventDefault();
            if (ok && !busy) void add();
          }}
        >
          <label className="w-44">
            <span className="mb-1 block text-[11px] font-medium text-subtle">Name</span>
            <input autoFocus value={name} onChange={(e) => setName(e.target.value)} placeholder="deploy" spellCheck={false} className={`${field} font-mono text-[12px]`} />
          </label>
          <label className="min-w-64 flex-1">
            <span className="mb-1 block text-[11px] font-medium text-subtle">Role ARN</span>
            <input value={arn} onChange={(e) => setArn(e.target.value)} placeholder="arn:aws:iam::123456789012:role/roll-deploy" spellCheck={false} className={`${field} font-mono text-[12px]`} />
          </label>
          <button type="submit" className={primary} disabled={!ok || busy}>
            {busy ? "Assuming it…" : "Add role"}
          </button>
          <button type="button" className={ghost} onClick={() => setAdding(false)}>
            Cancel
          </button>
        </form>
      )}
      {list.data && list.data.length === 0 && !adding ? (
        <p className="rounded-xl border border-dashed border-line-2 px-3 py-4 text-[12px] text-muted">No AWS roles. Add one to give services AWS credentials that end with their lease.</p>
      ) : (
        list.data && (
          <div className={`${card} divide-y divide-line`}>
            {list.data.map((r) => (
              <div key={r.id} className="flex flex-wrap items-center gap-x-3 gap-y-1 px-3 py-2">
                <Cloud className="size-3.5 text-muted" />
                <span className="font-mono text-[12px]">{r.name}</span>
                <span className="min-w-0 truncate font-mono text-[11.5px] text-subtle">{r.role_arn}</span>
                <span className="ml-auto text-[11px] text-subtle">
                  added {ago(r.created_at)} · <ActorMark who={r.created_by} />
                </span>
                {deleting === r.id ? (
                  <span className="flex gap-1">
                    <button className={dangerButton} onClick={() => del(r)}>
                      Delete
                    </button>
                    <button className={ghost} onClick={() => setDeleting(null)}>
                      Keep
                    </button>
                  </span>
                ) : (
                  <button className={ghost} onClick={() => setDeleting(r.id)} aria-label={`Delete the AWS role ${r.name}`}>
                    <Trash2 className="size-3.5" />
                  </button>
                )}
              </div>
            ))}
          </div>
        )
      )}
    </Section>
    {list.data && list.data.length > 0 && <AwsUsers roles={list.data} />}
    </>
  );
}

/**
 * IAM users (docs/secrets.md, "AWS"): users whose access keys roll rotates, acting as one of the
 * roles above, for what can't use a role. Their two key slots take turns: a rotation makes the new
 * key, and the old one is deleted when its grace period ends.
 */
export function AwsUsers({ roles }: { roles: AwsRole[] }) {
  const { api } = useSecrets();
  const list = useLoad(() => api.awsUsers(), [api]);
  const [adding, setAdding] = useState(false);
  const [name, setName] = useState("");
  const [role, setRole] = useState("");
  const [user, setUser] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [deleting, setDeleting] = useState<string | null>(null);
  const chosen = role || roles[0]?.id || "";
  const ok = /^[A-Za-z0-9_-]{1,64}$/.test(name) && /^[\w+=,.@-]{1,64}$/.test(user) && chosen !== "";

  const add = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.addAwsUser({ name, role: chosen, user });
      setAdding(false);
      setName("");
      setUser("");
      list.reload();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  const del = (u: AwsUser) =>
    api.deleteAwsUser(u.id).then(
      () => (setDeleting(null), list.reload()),
      (e) => (setDeleting(null), setError(errorText(e))),
    );
  const roleName = (id: string) => roles.find((r) => r.id === id)?.name ?? id;

  return (
    <Section
      title="IAM users"
      aside={
        <button className={button} disabled={roles.length === 0} onClick={() => setAdding((a) => !a)} title={roles.length === 0 ? "Add an AWS role first: roll manages the user's keys as it" : undefined}>
          <Plus className="size-3.5" /> Add a user
        </button>
      }
    >
      <p className="mb-2 text-[11.5px] text-muted">
        For what can&apos;t use a role&apos;s sessions: roll rotates an IAM user&apos;s access keys as one of the roles above (its policy allows <span className="font-mono">iam:ListAccessKeys</span>,{" "}
        <span className="font-mono">CreateAccessKey</span> and <span className="font-mono">DeleteAccessKey</span> on the user). Link <span className="font-mono">AWS_ACCESS_KEY_ID</span> and{" "}
        <span className="font-mono">AWS_SECRET_ACCESS_KEY</span> to it, managed; the old key is deleted when its grace period ends.
      </p>
      <ErrorNote error={error ?? list.error} onDismiss={error ? () => setError(null) : undefined} className="mb-2" />
      {adding && (
        <form
          className="rise mb-3 flex flex-wrap items-end gap-2 rounded-xl border border-line-2 bg-panel p-3"
          onSubmit={(e) => {
            e.preventDefault();
            if (ok && !busy) void add();
          }}
        >
          <label className="w-44">
            <span className="mb-1 block text-[11px] font-medium text-subtle">Name</span>
            <input autoFocus value={name} onChange={(e) => setName(e.target.value)} placeholder="legacy-ci" spellCheck={false} className={`${field} font-mono text-[12px]`} />
          </label>
          <label className="w-44">
            <span className="mb-1 block text-[11px] font-medium text-subtle">IAM user</span>
            <input value={user} onChange={(e) => setUser(e.target.value)} placeholder="ci-deployer" spellCheck={false} className={`${field} font-mono text-[12px]`} />
          </label>
          <label className="w-44">
            <span className="mb-1 block text-[11px] font-medium text-subtle">As role</span>
            <select value={chosen} onChange={(e) => setRole(e.target.value)} className={`${field} font-mono text-[12px]`}>
              {roles.map((r) => (
                <option key={r.id} value={r.id}>
                  {r.name}
                </option>
              ))}
            </select>
          </label>
          <button type="submit" className={primary} disabled={!ok || busy}>
            {busy ? "Checking…" : "Add user"}
          </button>
          <button type="button" className={ghost} onClick={() => setAdding(false)}>
            Cancel
          </button>
        </form>
      )}
      {list.data && list.data.length === 0 && !adding ? (
        <p className="rounded-xl border border-dashed border-line-2 px-3 py-4 text-[12px] text-muted">No IAM users. Add one to have roll rotate a user&apos;s access keys.</p>
      ) : (
        list.data && (
          <div className={`${card} divide-y divide-line`}>
            {list.data.map((u) => (
              <div key={u.id} className="flex flex-wrap items-center gap-x-3 gap-y-1 px-3 py-2">
                <KeyRound className="size-3.5 text-muted" />
                <span className="font-mono text-[12px]">{u.name}</span>
                <span className="font-mono text-[11.5px] text-subtle">{u.user}</span>
                <span className="font-mono text-[11px] text-muted">as {roleName(u.role)}</span>
                <span className="ml-auto text-[11px] text-subtle">
                  added {ago(u.created_at)} · <ActorMark who={u.created_by} />
                </span>
                {deleting === u.id ? (
                  <span className="flex gap-1">
                    <button className={dangerButton} onClick={() => del(u)}>
                      Delete
                    </button>
                    <button className={ghost} onClick={() => setDeleting(null)}>
                      Keep
                    </button>
                  </span>
                ) : (
                  <button className={ghost} onClick={() => setDeleting(u.id)} aria-label={`Delete the IAM user ${u.name}`}>
                    <Trash2 className="size-3.5" />
                  </button>
                )}
              </div>
            ))}
          </div>
        )
      )}
    </Section>
  );
}
