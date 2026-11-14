import { Database, Plus, Trash2 } from "lucide-react";
import { useState } from "react";
import type { OutsideDatabase } from "@/lib/secrets-api";
import { useSecrets } from "./secrets-page";
import { ActorMark, ago, button, card, dangerButton, ErrorNote, errorText, field, ghost, primary, Section, useLoad } from "./ui";

const ROLE = /^[a-z_][a-z0-9_]{0,62}$/;

/**
 * Databases outside roll (docs/secrets.md, "Databases outside roll"): Postgres and MySQL servers roll
 * rotates logins on itself. Two roles take turns (managed links), so the one in use keeps working
 * through the grace period while the other gets a new password; a group role gives each dynamic
 * lease a login of its own.
 */
export function OutsideDatabases() {
  const { api } = useSecrets();
  const list = useLoad(() => api.outsideDatabases(), [api]);
  const [adding, setAdding] = useState(false);
  const [name, setName] = useState("");
  const [url, setUrl] = useState("");
  const [roles, setRoles] = useState<[string, string]>(["", ""]);
  const [group, setGroup] = useState("");
  const [ca, setCa] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [deleting, setDeleting] = useState<string | null>(null);
  const pair = roles.every((r) => ROLE.test(r)) && roles[0] !== roles[1];
  const noPair = roles.every((r) => r === "");
  const groupOk = group === "" ? !noPair : ROLE.test(group) && !roles.includes(group);
  const ok = /^[A-Za-z0-9_-]{1,64}$/.test(name) && /^(postgres(ql)?|mysql|sqlserver|clickhouse|rabbitmq):\/\/\S+@\S+/.test(url.trim()) && (pair || noPair) && groupOk;

  const add = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.addOutsideDatabase({ name, url: url.trim(), roles: noPair ? [] : roles, group: group || undefined, ca: ca.trim() || undefined });
      setAdding(false);
      setName("");
      setUrl("");
      setRoles(["", ""]);
      setGroup("");
      setCa("");
      list.reload();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  const del = (d: OutsideDatabase) =>
    api.deleteOutsideDatabase(d.id).then(
      () => (setDeleting(null), list.reload()),
      (e) => (setDeleting(null), setError(errorText(e))),
    );

  return (
    <Section
      title="Databases outside roll"
      aside={
        <button className={button} onClick={() => setAdding((a) => !a)}>
          <Plus className="size-3.5" /> Add a database
        </button>
      }
    >
      <p className="mb-2 text-[11.5px] text-muted">
        Postgres or MySQL that roll doesn&apos;t run (RDS, Supabase, PlanetScale, your own server). Managed: two login roles take turns, roll sets the free one&apos;s password on each rotation, and the
        one in use keeps working through the grace period. Dynamic: each lease gets a login of its own in a group role, valid until the lease ends. Link <span className="font-mono">DB_USER</span> and{" "}
        <span className="font-mono">DB_PASSWORD</span> to it.
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
            <input autoFocus value={name} onChange={(e) => setName(e.target.value)} placeholder="billing" spellCheck={false} className={`${field} font-mono text-[12px]`} />
          </label>
          <label>
            <span className="mb-1 block text-[11px] font-medium text-subtle">Admin URL (kept sealed; a user that may alter these roles)</span>
            <input value={url} onChange={(e) => setUrl(e.target.value)} placeholder="postgres://admin:…@db.example.com:5432/app, mysql://…, sqlserver://…, clickhouse://… or rabbitmq://…" spellCheck={false} autoComplete="off" className={`${field} font-mono text-[12px]`} />
          </label>
          <div className="sm:col-start-2 flex flex-wrap gap-2">
            {roles.map((r, i) => (
              <label key={i} className="w-44">
                <span className="mb-1 block text-[11px] font-medium text-subtle">Managed login {i + 1}</span>
                <input value={r} onChange={(e) => setRoles((rs) => (i === 0 ? [e.target.value, rs[1]] : [rs[0], e.target.value]))} placeholder={i === 0 ? "app_blue" : "app_green"} spellCheck={false} className={`${field} font-mono text-[12px]`} />
              </label>
            ))}
            <label className="w-44">
              <span className="mb-1 block text-[11px] font-medium text-subtle">Group for dynamic logins</span>
              <input value={group} onChange={(e) => setGroup(e.target.value)} placeholder="app_rw" spellCheck={false} className={`${field} font-mono text-[12px]`} />
            </label>
          </div>
          <details className="sm:col-start-2">
            <summary className="cursor-pointer text-[11px] text-subtle select-none">Server CA certificate (Amazon RDS and other private CAs)</summary>
            <textarea value={ca} onChange={(e) => setCa(e.target.value)} rows={4} placeholder="-----BEGIN CERTIFICATE-----" spellCheck={false} className={`${field} mt-1 font-mono text-[11px]`} />
          </details>
          <div className="sm:col-start-2 flex gap-2">
            <button type="submit" className={primary} disabled={!ok || busy}>
              {busy ? "Connecting…" : "Add database"}
            </button>
            <button type="button" className={ghost} onClick={() => setAdding(false)}>
              Cancel
            </button>
          </div>
        </form>
      )}
      {list.data && list.data.length === 0 && !adding ? (
        <p className="rounded-xl border border-dashed border-line-2 px-3 py-4 text-[12px] text-muted">No databases outside roll. Add one to have roll rotate its login without a script of yours.</p>
      ) : (
        list.data && (
          <div className={`${card} divide-y divide-line`}>
            {list.data.map((d) => (
              <div key={d.id} className="flex flex-wrap items-center gap-x-3 gap-y-1 px-3 py-2">
                <Database className="size-3.5 text-muted" />
                <span className="font-mono text-[12px]">{d.name}</span>
                <span className="font-mono text-[11.5px] text-subtle">{d.host}</span>
                {d.roles.length > 0 && <span className="font-mono text-[11px] text-muted">{d.roles.join(" ⇄ ")}</span>}
                {d.group && <span className="font-mono text-[11px] text-muted">dynamic in {d.group}</span>}
                <span className="ml-auto text-[11px] text-subtle">
                  added {ago(d.created_at)} · <ActorMark who={d.created_by} />
                </span>
                {deleting === d.id ? (
                  <span className="flex gap-1">
                    <button className={dangerButton} onClick={() => del(d)}>
                      Delete
                    </button>
                    <button className={ghost} onClick={() => setDeleting(null)}>
                      Keep
                    </button>
                  </span>
                ) : (
                  <button className={ghost} onClick={() => setDeleting(d.id)} aria-label={`Delete the database ${d.name}`}>
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
