import { Plus, Trash2, Users } from "lucide-react";
import { useState } from "react";
import type { Directory, DirectoryFlavor } from "@/lib/secrets-api";
import { Segmented } from "../agents/ui/toggle-group";
import { GatewayPicker } from "./gateways";
import { useSecrets } from "./secrets-page";
import { ActorMark, ago, button, card, dangerButton, ErrorNote, errorText, field, ghost, primary, Section, textareaField, useLoad } from "./ui";

const DN = /^[^\s=,][^\n\r]*=[^\n\r]+$/;
const URL_RE = /^ldaps?:\/\/[^\s/?@]+\/?$/;

/**
 * Directories (docs/secrets.md, "Directories (LDAP and Active Directory)"): OpenLDAP and Active
 * Directory servers roll rotates passwords on itself. Two managed accounts take turns (one keeps
 * working through the grace period), one is replaced at once; a base for users gives each dynamic
 * lease a user of its own, in the groups.
 */
export function Directories() {
  const { api } = useSecrets();
  const list = useLoad(() => api.directories(), [api]);
  const [adding, setAdding] = useState(false);
  const [name, setName] = useState("");
  const [url, setUrl] = useState("");
  const [flavor, setFlavor] = useState<DirectoryFlavor>("openldap");
  const [bindDn, setBindDn] = useState("");
  const [bindPassword, setBindPassword] = useState("");
  const [accounts, setAccounts] = useState<[string, string]>(["", ""]);
  const [base, setBase] = useState("");
  const [groups, setGroups] = useState("");
  const [ca, setCa] = useState("");
  const [gateway, setGateway] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [deleting, setDeleting] = useState<string | null>(null);

  const managed = accounts.map((a) => a.trim()).filter(Boolean);
  const groupList = groups
    .split("\n")
    .map((g) => g.trim())
    .filter(Boolean);
  const ok =
    /^[A-Za-z0-9_-]{1,64}$/.test(name) &&
    URL_RE.test(url.trim()) &&
    DN.test(bindDn.trim()) &&
    bindPassword !== "" &&
    managed.every((a) => DN.test(a)) &&
    new Set(managed.map((a) => a.toLowerCase())).size === managed.length &&
    (base.trim() === "" ? managed.length > 0 && groupList.length === 0 : DN.test(base.trim())) &&
    groupList.every((g) => DN.test(g));

  const reset = () => {
    setName("");
    setUrl("");
    setBindDn("");
    setBindPassword("");
    setAccounts(["", ""]);
    setBase("");
    setGroups("");
    setCa("");
    setGateway("");
  };
  const add = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.addDirectory({
        name,
        url: url.trim(),
        flavor,
        bind_dn: bindDn.trim(),
        bind_password: bindPassword,
        managed_accounts: managed,
        user_base_dn: base.trim() || undefined,
        groups: groupList,
        ca: ca.trim() || undefined,
        gateway: gateway || undefined,
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
  const del = (d: Directory) =>
    api.deleteDirectory(d.id).then(
      () => (setDeleting(null), list.reload()),
      (e) => (setDeleting(null), setError(errorText(e))),
    );

  return (
    <Section
      title="Directories"
      aside={
        <button className={button} onClick={() => setAdding((a) => !a)}>
          <Plus className="size-3.5" /> Add a directory
        </button>
      }
    >
      <p className="mb-2 text-[11.5px] text-muted">
        OpenLDAP or Active Directory, always over TLS. Managed: two accounts take turns, so the one in use keeps working through the grace period (with one account, each rotation replaces its password at once). Dynamic: each lease gets a user of its own
        under a base, in your groups, deleted when the lease ends. Link <span className="font-mono">LDAP_BIND_DN</span> and <span className="font-mono">LDAP_PASSWORD</span> to it.
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
            <input autoFocus value={name} onChange={(e) => setName(e.target.value)} placeholder="corp" spellCheck={false} className={`${field} font-mono text-[12px]`} />
          </label>
          <label>
            <span className="mb-1 block text-[11px] font-medium text-subtle">URL (ldaps://, or ldap:// with StartTLS)</span>
            <input value={url} onChange={(e) => setUrl(e.target.value)} placeholder="ldaps://dc1.example.com:636" spellCheck={false} autoComplete="off" className={`${field} font-mono text-[12px]`} />
          </label>
          <div className="sm:col-start-2">
            <Segmented
              label="Directory"
              value={flavor}
              onChange={setFlavor}
              options={[
                { value: "openldap", label: <span className="px-1.5 text-[11.5px]">OpenLDAP</span> },
                { value: "active_directory", label: <span className="px-1.5 text-[11.5px]">Active Directory</span> },
              ]}
            />
          </div>
          <div className="sm:col-start-2 grid gap-2 sm:grid-cols-2">
            <label>
              <span className="mb-1 block text-[11px] font-medium text-subtle">Bind DN (may reset these passwords)</span>
              <input value={bindDn} onChange={(e) => setBindDn(e.target.value)} placeholder="cn=roll,ou=svc,dc=example,dc=com" spellCheck={false} className={`${field} font-mono text-[12px]`} />
            </label>
            <label>
              <span className="mb-1 block text-[11px] font-medium text-subtle">Bind password (kept sealed)</span>
              <input type="password" value={bindPassword} onChange={(e) => setBindPassword(e.target.value)} autoComplete="new-password" className={`${field} font-mono text-[12px]`} />
            </label>
            {accounts.map((a, i) => (
              <label key={i}>
                <span className="mb-1 block text-[11px] font-medium text-subtle">Managed account {i + 1}</span>
                <input
                  value={a}
                  onChange={(e) => setAccounts((as) => (i === 0 ? [e.target.value, as[1]] : [as[0], e.target.value]))}
                  placeholder={i === 0 ? "cn=app-blue,ou=svc,dc=example,dc=com" : "cn=app-green,… (optional)"}
                  spellCheck={false}
                  className={`${field} font-mono text-[12px]`}
                />
              </label>
            ))}
            <label>
              <span className="mb-1 block text-[11px] font-medium text-subtle">Base for dynamic users</span>
              <input value={base} onChange={(e) => setBase(e.target.value)} placeholder="ou=roll,dc=example,dc=com" spellCheck={false} className={`${field} font-mono text-[12px]`} />
            </label>
            <label>
              <span className="mb-1 block text-[11px] font-medium text-subtle">Groups they join (one DN a line)</span>
              <textarea value={groups} onChange={(e) => setGroups(e.target.value)} rows={2} placeholder="cn=readers,ou=groups,dc=example,dc=com" spellCheck={false} className={`${textareaField} font-mono text-[12px]`} />
            </label>
          </div>
          <div className="sm:col-start-2 max-w-sm">
            <GatewayPicker value={gateway} onChange={setGateway} />
          </div>
          <details className="sm:col-start-2">
            <summary className="cursor-pointer text-[11px] text-subtle select-none">Server CA certificate (a private CA, as most domain controllers have)</summary>
            <textarea value={ca} onChange={(e) => setCa(e.target.value)} rows={4} placeholder="-----BEGIN CERTIFICATE-----" spellCheck={false} className={`${textareaField} mt-1 font-mono text-[11px]`} />
          </details>
          <div className="sm:col-start-2 flex gap-2">
            <button type="submit" className={primary} disabled={!ok || busy}>
              {busy ? "Binding…" : "Add directory"}
            </button>
            <button type="button" className={ghost} onClick={() => setAdding(false)}>
              Cancel
            </button>
          </div>
        </form>
      )}
      {list.data && list.data.length === 0 && !adding ? (
        <p className="rounded-xl border border-dashed border-line-2 px-3 py-4 text-[12px] text-muted">No directories. Add one to have roll rotate an LDAP or Active Directory password, or make a user per lease.</p>
      ) : (
        list.data && (
          <div className={`${card} divide-y divide-line`}>
            {list.data.map((d) => (
              <div key={d.id} className="flex flex-wrap items-center gap-x-3 gap-y-1 px-3 py-2">
                <Users className="size-3.5 text-muted" />
                <span className="font-mono text-[12px]">{d.name}</span>
                <span className="min-w-0 truncate font-mono text-[11.5px] text-subtle">{d.url}</span>
                <span className="rounded border border-line-2 px-1 text-[10px] text-subtle">{d.flavor === "active_directory" ? "Active Directory" : "OpenLDAP"}</span>
                {d.gateway && <span className="font-mono text-[11px] text-muted">through {d.gateway}</span>}
                {d.managed_accounts.length > 0 && <span className="min-w-0 truncate font-mono text-[11px] text-muted">{d.managed_accounts.join(" ⇄ ")}</span>}
                {d.user_base_dn && <span className="min-w-0 truncate font-mono text-[11px] text-muted">dynamic in {d.user_base_dn}</span>}
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
                  <button className={ghost} onClick={() => setDeleting(d.id)} aria-label={`Delete the directory ${d.name}`}>
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
