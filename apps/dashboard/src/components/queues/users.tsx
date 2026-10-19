"use client";

import { Plus, Trash2, UserRound } from "lucide-react";
import { useMemo, useState } from "react";
import { type Permission, qapi, type User } from "@/lib/queues-api";
import { useAction, usePoll } from "./hooks";
import { QueuesShell } from "./shell";
import { Badge, btn, Empty, ErrorNote, Field, ICON, input, Panel, Pending, primary, VhostSelect } from "./ui";

const TAGS = ["administrator", "monitoring", "policymaker", "management", "impersonator"];

export function Users() {
  const poll = usePoll("users", qapi.users);
  const vhostsPoll = usePoll("vhosts", qapi.vhosts, 15_000);
  const vhosts = useMemo(() => vhostsPoll.data?.map((v) => v.name) ?? [], [vhostsPoll.data]);
  const { busy, error, setError, run } = useAction();
  const [selected, setSelected] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const users = poll.data ?? [];
  const user = users.find((u) => u.name === selected) ?? null;

  const remove = (u: User) => {
    if (!confirm(`Delete user ${u.name}? Their open connections keep working until they reconnect.`)) return;
    const before = poll.data;
    poll.mutate((d) => d.filter((x) => x.name !== u.name));
    setSelected(null);
    run(`del-${u.name}`, async () => {
      await qapi.deleteUser(u.name);
      await poll.refresh();
    }, { rollback: () => before && poll.mutate(() => before) });
  };

  return (
    <QueuesShell
      title="Users"
      crumb="users"
      description="Broker users, their tags and per-vhost permissions (configure, write and read patterns)."
      loading={poll.loading && !poll.error}
      offline={poll.offline && !poll.data}
      error={poll.error}
      actions={
        <button onClick={() => setCreating((c) => !c)} className={primary}>
          <Plus {...ICON} /> New user
        </button>
      }
    >
      {creating && (
        <CreateUser
          vhosts={vhosts}
          onCancel={() => setCreating(false)}
          onCreated={(name) => {
            setCreating(false);
            setSelected(name);
            poll.refresh();
          }}
        />
      )}
      <ErrorNote error={error} onClose={() => setError(null)} />
      <div className="grid gap-4 lg:grid-cols-5">
        <Panel title="Users" meta={poll.data ? String(users.length) : undefined} className="lg:col-span-2">
          {users.length === 0 ? (
            <Empty>No users.</Empty>
          ) : (
            <ul>
              {users.map((u) => (
                <li
                  key={u.name}
                  onClick={() => setSelected(u.name)}
                  className={`flex cursor-pointer items-center gap-3 border-b border-line px-5 py-3 transition last:border-0 ${
                    u.name === selected ? "bg-panel-2" : "hover:bg-panel-2/60"
                  }`}
                >
                  <UserRound {...ICON} className="size-4 text-subtle" />
                  <div className="min-w-0 flex-1">
                    <p className="truncate font-mono text-[12px]">{u.name}</p>
                    <p className="truncate text-[11px] text-subtle">
                      {u.permissions.length ? u.permissions.map((p) => p.vhost).join(", ") : "no vhost access"}
                    </p>
                  </div>
                  <div className="flex flex-wrap justify-end gap-1">
                    {u.tags.map((t) => (
                      <Badge key={t} tone={t === "administrator" ? "accent" : "muted"}>
                        {t}
                      </Badge>
                    ))}
                  </div>
                </li>
              ))}
            </ul>
          )}
        </Panel>
        <div className="lg:col-span-3">
          {user ? (
            <UserEditor key={user.name} user={user} vhosts={vhosts} busy={busy} onDelete={() => remove(user)} onSaved={() => poll.refresh()} />
          ) : (
            <Panel title="Permissions">
              <Empty>Select a user to edit their tags, password and permissions.</Empty>
            </Panel>
          )}
        </div>
      </div>
    </QueuesShell>
  );
}

function TagPicker({ tags, onChange }: { tags: string[]; onChange: (t: string[]) => void }) {
  return (
    <div className="flex flex-wrap gap-1.5">
      {TAGS.map((t) => {
        const on = tags.includes(t);
        return (
          <button
            key={t}
            type="button"
            onClick={() => onChange(on ? tags.filter((x) => x !== t) : [...tags, t])}
            className={`rounded-full border px-2.5 py-0.5 text-[12px] transition ${on ? "border-line-2 bg-panel-2 text-fg" : "border-line text-subtle hover:text-fg"}`}
          >
            {t}
          </button>
        );
      })}
    </div>
  );
}

function UserEditor({
  user,
  vhosts,
  busy: outerBusy,
  onDelete,
  onSaved,
}: {
  user: User;
  vhosts: string[];
  busy: string | null;
  onDelete: () => void;
  onSaved: () => void;
}) {
  const { busy, error, setError, run } = useAction();
  const [tags, setTags] = useState(user.tags);
  const [password, setPassword] = useState("");
  const [perms, setPerms] = useState<Permission[]>(user.permissions);
  const [addVhost, setAddVhost] = useState("");
  const dirtyTags = tags.join(",") !== user.tags.join(",");
  const free = vhosts.filter((v) => !perms.some((p) => p.vhost === v));
  const newVhost = addVhost && free.includes(addVhost) ? addVhost : free[0];

  const savePerm = (p: Permission) =>
    run(`perm-${p.vhost}`, async () => {
      await qapi.putPermissions(p.vhost, user.name, { configure: p.configure, write: p.write, read: p.read });
      onSaved();
    });

  return (
    <Panel
      title={<span className="font-mono">{user.name}</span>}
      actions={
        <button onClick={onDelete} disabled={outerBusy !== null || busy !== null} className={`${btn} hover:text-danger`}>
          <Trash2 {...ICON} /> Delete user
        </button>
      }
    >
      <form
        className="space-y-3 border-b border-line px-5 py-4"
        onSubmit={async (e) => {
          e.preventDefault();
          const ok = await run("user", async () => {
            await qapi.putUser(user.name, { tags, password: password || undefined });
            onSaved();
            return true;
          });
          if (ok) setPassword("");
        }}
      >
        <Field label="Tags">
          <TagPicker tags={tags} onChange={setTags} />
        </Field>
        <div className="flex flex-wrap items-end gap-2">
          <Field label="New password" className="min-w-48 flex-1">
            <input type="password" value={password} onChange={(e) => setPassword(e.target.value)} placeholder="unchanged" autoComplete="new-password" className={`${input} w-full`} />
          </Field>
          <button type="submit" disabled={busy !== null || (!dirtyTags && !password)} className={btn}>
            {busy === "user" ? <Pending>Saving…</Pending> : "Save"}
          </button>
        </div>
      </form>

      <div className="px-5 py-4">
        <p className="mb-2 text-[11px] text-subtle">Permissions · regular expressions matched against resource names</p>
        <div className="space-y-1.5">
          <div className="grid grid-cols-[7rem_1fr_1fr_1fr_auto] gap-1.5 text-[10px] text-subtle">
            <span>vhost</span>
            <span>configure</span>
            <span>write</span>
            <span>read</span>
            <span className="w-12" />
          </div>
          {perms.map((p, i) => {
            const orig = user.permissions.find((x) => x.vhost === p.vhost);
            const dirty = !orig || orig.configure !== p.configure || orig.write !== p.write || orig.read !== p.read;
            const set = (patch: Partial<Permission>) => setPerms((ps) => ps.map((x, j) => (j === i ? { ...x, ...patch } : x)));
            return (
              <div key={p.vhost} className="grid grid-cols-[7rem_1fr_1fr_1fr_auto] items-center gap-1.5">
                <span className="truncate font-mono text-[12px]">{p.vhost}</span>
                {(["configure", "write", "read"] as const).map((k) => (
                  <input
                    key={k}
                    value={p[k]}
                    onChange={(e) => set({ [k]: e.target.value })}
                    aria-label={`${k} on ${p.vhost}`}
                    spellCheck={false}
                    className={`${input} min-w-0 font-mono text-[12px]`}
                  />
                ))}
                <button type="button" disabled={!dirty || busy !== null} onClick={() => savePerm(p)} className={`${btn} w-12 justify-center`}>
                  {busy === `perm-${p.vhost}` ? <Pending>…</Pending> : "Set"}
                </button>
              </div>
            );
          })}
          {perms.length === 0 && <p className="py-2 text-[12px] text-subtle">No access to any vhost yet.</p>}
        </div>
        {free.length > 0 && (
          <div className="mt-3 flex items-center gap-2">
            <VhostSelect value={newVhost ?? ""} vhosts={free} onChange={setAddVhost} />
            <button
              type="button"
              onClick={() => newVhost && setPerms((ps) => [...ps, { vhost: newVhost, configure: ".*", write: ".*", read: ".*" }])}
              className={btn}
            >
              <Plus {...ICON} /> Grant access
            </button>
          </div>
        )}
        <div className="mt-3">
          <ErrorNote error={error} onClose={() => setError(null)} />
        </div>
      </div>
    </Panel>
  );
}

function CreateUser({ vhosts, onCancel, onCreated }: { vhosts: string[]; onCancel: () => void; onCreated: (name: string) => void }) {
  const { busy, error, setError, run } = useAction();
  const [name, setName] = useState("");
  const [password, setPassword] = useState("");
  const [tags, setTags] = useState<string[]>(["management"]);
  const [vhost, setVhost] = useState("");
  const grant = vhost || vhosts[0] || "";

  return (
    <form
      className="rise space-y-3 rounded-2xl border border-line bg-panel p-4"
      onSubmit={async (e) => {
        e.preventDefault();
        if (!name.trim() || !password) return;
        const ok = await run("create", async () => {
          await qapi.putUser(name.trim(), { password, tags });
          if (grant) await qapi.putPermissions(grant, name.trim(), { configure: ".*", write: ".*", read: ".*" });
          return true;
        });
        if (ok) onCreated(name.trim());
      }}
    >
      <div className="grid gap-2 sm:grid-cols-3">
        <Field label="Username">
          <input autoFocus value={name} onChange={(e) => setName(e.target.value)} spellCheck={false} autoComplete="off" className={`${input} w-full font-mono text-[12px]`} />
        </Field>
        <Field label="Password">
          <input type="password" value={password} onChange={(e) => setPassword(e.target.value)} autoComplete="new-password" className={`${input} w-full`} />
        </Field>
        <Field label="Full access to">
          <VhostSelect value={grant} vhosts={vhosts} onChange={setVhost} all={vhosts.length ? undefined : "no vhosts"} className="w-full" />
        </Field>
      </div>
      <Field label="Tags">
        <TagPicker tags={tags} onChange={setTags} />
      </Field>
      <ErrorNote error={error} onClose={() => setError(null)} />
      <div className="flex justify-end gap-2">
        <button type="button" onClick={onCancel} className={btn}>
          Cancel
        </button>
        <button type="submit" disabled={busy !== null || !name.trim() || !password} className={primary}>
          {busy ? <Pending>Creating…</Pending> : "Create user"}
        </button>
      </div>
    </form>
  );
}
