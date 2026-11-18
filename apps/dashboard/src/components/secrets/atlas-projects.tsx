import { Leaf, Plus, Trash2 } from "lucide-react";
import { useState } from "react";
import type { AtlasProject } from "@/lib/secrets-api";
import { useSecrets } from "./secrets-page";
import { ActorMark, ago, button, card, dangerButton, ErrorNote, errorText, field, ghost, primary, Section, useLoad } from "./ui";

/** An Atlas project id: 24 lower-case hex digits. */
const GROUP = /^[a-f0-9]{24}$/;
/** A service account's client id. */
const CLIENT = /^[A-Za-z0-9_-]{1,128}$/;
/** A database user roll accepts. */
const USER = /^[A-Za-z0-9._@+-]{1,128}$/;
/** role@db or role@db.collection. */
const ROLE = /^[A-Za-z0-9_-]{1,64}@[A-Za-z0-9_-]{1,64}(\.[A-Za-z0-9_.-]{1,120})?$/;

const words = (s: string) =>
  s
    .split(/[\s,]+/)
    .map((w) => w.trim())
    .filter(Boolean);

/**
 * MongoDB Atlas projects (docs/secrets.md, "MongoDB Atlas"): Atlas manages database users only
 * through its Admin API, so roll calls it as an organization service account. Managed: two users
 * take turns, so the one in use keeps working while Atlas applies the other's new password.
 * Dynamic: each lease gets a user of its own, which Atlas deletes after the lease even if roll
 * doesn't.
 */
export function AtlasProjects() {
  const { api } = useSecrets();
  const list = useLoad(() => api.atlasProjects(), [api]);
  const [adding, setAdding] = useState(false);
  const [name, setName] = useState("");
  const [group, setGroup] = useState("");
  const [clientId, setClientId] = useState("");
  const [secret, setSecret] = useState("");
  const [users, setUsers] = useState("");
  const [roles, setRoles] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [deleting, setDeleting] = useState<string | null>(null);

  const userList = words(users);
  const roleList = words(roles);
  const ok =
    /^[A-Za-z0-9_-]{1,64}$/.test(name) &&
    GROUP.test(group.trim()) &&
    CLIENT.test(clientId.trim()) &&
    secret.trim() !== "" &&
    (userList.length === 0 || (userList.length === 2 && userList[0] !== userList[1] && userList.every((u) => USER.test(u)))) &&
    roleList.every((r) => ROLE.test(r) && !r.startsWith("atlasAdmin@")) &&
    roleList.length <= 16 &&
    (userList.length > 0 || roleList.length > 0);

  const reset = () => {
    setName("");
    setGroup("");
    setClientId("");
    setSecret("");
    setUsers("");
    setRoles("");
  };
  const add = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.addAtlasProject({ name, group_id: group.trim(), client_id: clientId.trim(), client_secret: secret.trim(), managed_users: userList, roles: roleList });
      setAdding(false);
      reset();
      list.reload();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  const del = (a: AtlasProject) =>
    api.deleteAtlasProject(a.id).then(
      () => (setDeleting(null), list.reload()),
      (e) => (setDeleting(null), setError(errorText(e))),
    );

  return (
    <Section
      title="MongoDB Atlas"
      aside={
        <button className={button} onClick={() => setAdding((a) => !a)}>
          <Plus className="size-3.5" /> Add a project
        </button>
      }
    >
      <p className="mb-2 text-[11.5px] text-muted">
        Atlas manages database users only through its Admin API, so roll calls it as an organization service account with Project Database Access Admin. Managed: two users take turns, so the one in use keeps working while Atlas applies the other's new
        password (about a minute). Dynamic: each lease gets a user of its own, which Atlas deletes after the lease even if roll doesn't. Link <span className="font-mono">MONGODB_USERNAME</span> and{" "}
        <span className="font-mono">MONGODB_PASSWORD</span> to it, and <span className="font-mono">MONGODB_CONNECTION_STRING</span> with a cluster.
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
            <input autoFocus value={name} onChange={(e) => setName(e.target.value)} placeholder="shop-atlas" spellCheck={false} className={`${field} font-mono text-[12px]`} />
          </label>
          <label>
            <span className="mb-1 block text-[11px] font-medium text-subtle">Atlas project id</span>
            <input value={group} onChange={(e) => setGroup(e.target.value)} placeholder="5f1d3c2b4a6e7f8091a2b3c4" spellCheck={false} autoComplete="off" className={`${field} font-mono text-[12px]`} />
          </label>
          <div className="sm:col-start-2 grid gap-2 sm:grid-cols-2">
            <label>
              <span className="mb-1 block text-[11px] font-medium text-subtle">Service account client id</span>
              <input value={clientId} onChange={(e) => setClientId(e.target.value)} placeholder="mdb_sa_id_…" spellCheck={false} autoComplete="off" className={`${field} font-mono text-[12px]`} />
            </label>
            <label>
              <span className="mb-1 block text-[11px] font-medium text-subtle">Client secret (kept sealed)</span>
              <input type="password" value={secret} onChange={(e) => setSecret(e.target.value)} placeholder="mdb_sa_sk_…" spellCheck={false} autoComplete="off" className={`${field} font-mono text-[12px]`} />
            </label>
          </div>
          <div className="sm:col-start-2 grid gap-2 sm:grid-cols-2">
            <label>
              <span className="mb-1 block text-[11px] font-medium text-subtle">Managed users (two, taking turns)</span>
              <input value={users} onChange={(e) => setUsers(e.target.value)} placeholder="app_a, app_b" spellCheck={false} className={`${field} font-mono text-[12px]`} />
            </label>
            <label>
              <span className="mb-1 block text-[11px] font-medium text-subtle">Roles for dynamic users</span>
              <input value={roles} onChange={(e) => setRoles(e.target.value)} placeholder="readWrite@shop, read@analytics" spellCheck={false} className={`${field} font-mono text-[12px]`} />
            </label>
          </div>
          <div className="sm:col-start-2 flex gap-2">
            <button type="submit" className={primary} disabled={!ok || busy}>
              {busy ? "Checking with Atlas…" : "Add project"}
            </button>
            <button type="button" className={ghost} onClick={() => setAdding(false)}>
              Cancel
            </button>
          </div>
        </form>
      )}
      {list.data && list.data.length === 0 && !adding ? (
        <p className="rounded-xl border border-dashed border-line-2 px-3 py-4 text-[12px] text-muted">No Atlas projects. Add one to have roll rotate two database users in turn, or make a user per lease.</p>
      ) : (
        list.data && (
          <div className={`${card} divide-y divide-line`}>
            {list.data.map((a) => (
              <div key={a.id} className="flex flex-wrap items-center gap-x-3 gap-y-1 px-3 py-2">
                <Leaf className="size-3.5 text-muted" />
                <span className="font-mono text-[12px]">{a.name}</span>
                <span className="min-w-0 truncate font-mono text-[11.5px] text-subtle">{a.group_id}</span>
                <span className="min-w-0 truncate font-mono text-[11px] text-subtle">{a.client_id}</span>
                {a.managed_users.length > 0 && <span className="min-w-0 truncate font-mono text-[11px] text-muted">{a.managed_users.join(" ⇄ ")}</span>}
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
                  <button className={ghost} onClick={() => setDeleting(a.id)} aria-label={`Delete the Atlas project ${a.name}`}>
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
