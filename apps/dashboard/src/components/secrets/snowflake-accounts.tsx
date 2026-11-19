import { Plus, Snowflake, Trash2 } from "lucide-react";
import { useState } from "react";
import type { SnowflakeAccount, SnowflakeAuth } from "@/lib/secrets-api";
import { Segmented } from "../agents/ui/toggle-group";
import { useSecrets } from "./secrets-page";
import { ActorMark, ago, button, card, dangerButton, ErrorNote, errorText, field, ghost, primary, Section, textareaField, useLoad } from "./ui";

/** An account identifier: myorg-myaccount, or a locator such as xy12345.us-east-2.aws. */
const ACCOUNT = /^[A-Za-z0-9_]([A-Za-z0-9_-]*[A-Za-z0-9_])?(\.[A-Za-z0-9_]([A-Za-z0-9_-]*[A-Za-z0-9_])?)*$/;
/** A name Snowflake reads unquoted. */
const IDENT = /^[A-Za-z_][A-Za-z0-9_$]{0,254}$/;

/**
 * Snowflake accounts (docs/secrets.md, "Snowflake"): roll signs in as an admin user with its key
 * pair. Managed: a service user's two key slots take turns, so the key in use keeps working through
 * the grace period. Dynamic: each lease gets a user of its own with the link's roles, dropped when
 * the lease ends.
 */
export function SnowflakeAccounts() {
  const { api } = useSecrets();
  const list = useLoad(() => api.snowflakeAccounts(), [api]);
  const [adding, setAdding] = useState(false);
  const [name, setName] = useState("");
  const [account, setAccount] = useState("");
  const [privatelink, setPrivatelink] = useState(false);
  const [adminUser, setAdminUser] = useState("");
  const [adminRole, setAdminRole] = useState("");
  const [key, setKey] = useState("");
  const [auth, setAuth] = useState<SnowflakeAuth>("key_pair");
  const [managedUser, setManagedUser] = useState("");
  const [roles, setRoles] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [deleting, setDeleting] = useState<string | null>(null);

  const roleList = roles
    .split(/[\s,]+/)
    .map((r) => r.trim())
    .filter(Boolean);
  const ok =
    /^[A-Za-z0-9_-]{1,64}$/.test(name) &&
    ACCOUNT.test(account.trim()) &&
    !/snowflakecomputing/i.test(account) &&
    IDENT.test(adminUser.trim()) &&
    (adminRole.trim() === "" || IDENT.test(adminRole.trim())) &&
    key.includes("PRIVATE KEY-----") &&
    (managedUser.trim() === "" || (IDENT.test(managedUser.trim()) && managedUser.trim().toUpperCase() !== adminUser.trim().toUpperCase())) &&
    roleList.every((r) => IDENT.test(r)) &&
    roleList.length <= 16 &&
    (managedUser.trim() !== "" || roleList.length > 0);

  const reset = () => {
    setName("");
    setAccount("");
    setPrivatelink(false);
    setAdminUser("");
    setAdminRole("");
    setKey("");
    setManagedUser("");
    setRoles("");
  };
  const add = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.addSnowflakeAccount({
        name,
        account: account.trim(),
        privatelink,
        admin_user: adminUser.trim(),
        admin_role: adminRole.trim() || undefined,
        admin_private_key: key,
        auth,
        managed_user: managedUser.trim() || undefined,
        roles: roleList,
      });
      setAdding(false);
      reset();
      list.reload();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  const del = (a: SnowflakeAccount) =>
    api.deleteSnowflakeAccount(a.id).then(
      () => (setDeleting(null), list.reload()),
      (e) => (setDeleting(null), setError(errorText(e))),
    );

  return (
    <Section
      title="Snowflake"
      aside={
        <button className={button} onClick={() => setAdding((a) => !a)}>
          <Plus className="size-3.5" /> Add an account
        </button>
      }
    >
      <p className="mb-2 text-[11.5px] text-muted">
        roll signs in as an admin user with its key pair. Managed: a service user's two key slots take turns, so the key in use keeps working through the grace period. Dynamic: each lease gets a user of its own with your roles, dropped when the lease ends.
        Link <span className="font-mono">SNOWFLAKE_USER</span>, <span className="font-mono">SNOWFLAKE_ACCOUNT</span> and <span className="font-mono">SNOWFLAKE_PRIVATE_KEY</span> to it.
      </p>
      <ErrorNote error={error ?? list.error} onDismiss={error ? () => setError(null) : undefined} className="mb-2" />
      {adding && (
        <form
          className="rise mb-3 grid gap-2 rounded-xl border border-line-2 bg-panel p-3 sm:grid-cols-[11rem_1fr]"
          onSubmit={(e) => {
            e.preventDefault();
            if (ok && !busy) void add();
          }}
        >
          <label>
            <span className="mb-1 block text-[11px] font-medium text-subtle">Name</span>
            <input autoFocus value={name} onChange={(e) => setName(e.target.value)} placeholder="warehouse" spellCheck={false} className={`${field} font-mono text-[12px]`} />
          </label>
          <div className="grid gap-2 sm:grid-cols-[1fr_auto] sm:items-end">
            <label>
              <span className="mb-1 block text-[11px] font-medium text-subtle">Account identifier</span>
              <input value={account} onChange={(e) => setAccount(e.target.value)} placeholder="myorg-myaccount" spellCheck={false} autoComplete="off" className={`${field} font-mono text-[12px]`} />
            </label>
            <label className="flex h-8 items-center gap-1.5 text-[12px] text-muted">
              <input type="checkbox" checked={privatelink} onChange={(e) => setPrivatelink(e.target.checked)} />
              PrivateLink
            </label>
          </div>
          <div className="sm:col-start-2 grid gap-2 sm:grid-cols-2">
            <label>
              <span className="mb-1 block text-[11px] font-medium text-subtle">Admin user (roll signs in as it)</span>
              <input value={adminUser} onChange={(e) => setAdminUser(e.target.value)} placeholder="ROLL_ADMIN" spellCheck={false} className={`${field} font-mono text-[12px]`} />
            </label>
            <label>
              <span className="mb-1 block text-[11px] font-medium text-subtle">Admin role (optional)</span>
              <input value={adminRole} onChange={(e) => setAdminRole(e.target.value)} placeholder="ROLL_ADMIN" spellCheck={false} className={`${field} font-mono text-[12px]`} />
            </label>
          </div>
          <label className="sm:col-start-2">
            <span className="mb-1 block text-[11px] font-medium text-subtle">Admin private key, PEM (kept sealed)</span>
            <textarea value={key} onChange={(e) => setKey(e.target.value)} rows={4} placeholder="-----BEGIN PRIVATE KEY-----" spellCheck={false} autoComplete="off" className={`${textareaField} font-mono text-[11px]`} />
          </label>
          <div className="sm:col-start-2">
            <Segmented
              label="Users sign in with"
              value={auth}
              onChange={setAuth}
              options={[
                { value: "key_pair", label: <span className="px-1.5 text-[11.5px]">Key pair</span> },
                { value: "password", label: <span className="px-1.5 text-[11.5px]">Password</span> },
              ]}
            />
          </div>
          <div className="sm:col-start-2 grid gap-2 sm:grid-cols-2">
            <label>
              <span className="mb-1 block text-[11px] font-medium text-subtle">Managed user (its key rotates)</span>
              <input value={managedUser} onChange={(e) => setManagedUser(e.target.value)} placeholder="SVC_APP" spellCheck={false} className={`${field} font-mono text-[12px]`} />
            </label>
            <label>
              <span className="mb-1 block text-[11px] font-medium text-subtle">Roles for dynamic users (first is default)</span>
              <input value={roles} onChange={(e) => setRoles(e.target.value)} placeholder="ANALYST, LOADER" spellCheck={false} className={`${field} font-mono text-[12px]`} />
            </label>
          </div>
          <div className="sm:col-start-2 flex gap-2">
            <button type="submit" className={primary} disabled={!ok || busy}>
              {busy ? "Signing in…" : "Add account"}
            </button>
            <button type="button" className={ghost} onClick={() => setAdding(false)}>
              Cancel
            </button>
          </div>
        </form>
      )}
      {list.data && list.data.length === 0 && !adding ? (
        <p className="rounded-xl border border-dashed border-line-2 px-3 py-4 text-[12px] text-muted">No Snowflake accounts. Add one to have roll rotate a service user's key, or make a user per lease.</p>
      ) : (
        list.data && (
          <div className={`${card} divide-y divide-line`}>
            {list.data.map((a) => (
              <div key={a.id} className="flex flex-wrap items-center gap-x-3 gap-y-1 px-3 py-2">
                <Snowflake className="size-3.5 text-muted" />
                <span className="font-mono text-[12px]">{a.name}</span>
                <span className="min-w-0 truncate font-mono text-[11.5px] text-subtle">{a.host}</span>
                <span className="rounded border border-line-2 px-1 text-[10px] text-subtle">{a.auth === "password" ? "Password" : "Key pair"}</span>
                {a.managed_user && <span className="min-w-0 truncate font-mono text-[11px] text-muted">{a.managed_user}</span>}
                {a.roles.length > 0 && <span className="min-w-0 truncate font-mono text-[11px] text-muted">dynamic as {a.roles.join(", ")}</span>}
                <span className="ml-auto text-[11px] text-subtle">
                  added {ago(a.created_at)} · <ActorMark who={a.created_by} />
                </span>
                {deleting === a.id ? (
                  <span className="flex gap-1">
                    <button className={dangerButton} onClick={() => del(a)}>
                      Delete
                    </button>
                    <button className={ghost} onClick={() => setDeleting(null)}>
                      Keep
                    </button>
                  </span>
                ) : (
                  <button className={ghost} onClick={() => setDeleting(a.id)} aria-label={`Delete the Snowflake account ${a.name}`}>
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
