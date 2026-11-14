import { Check, Copy, KeyRound, Plus, Send } from "lucide-react";
import { useState } from "react";
import { Link } from "@tanstack/react-router";
import { ACTIONS, type Action, type NewToken } from "@/lib/secrets-api";
import { CodeLeaks } from "./code-leaks";
import { Identities } from "./identities";
import { ShareDialog, type ShareSource } from "./share-dialog";
import { useSecrets } from "./secrets-page";
import { ActorMark, ago, button, card, Chip, dangerButton, ErrorNote, errorText, field, ghost, label, LinkChip, primary, Section, Teach, until, useLoad } from "./ui";

const ACTION_HELP: Record<Action, string> = {
  read: "names, metadata, history, compare",
  reveal: "values: roll run, export, reveal",
  write: "set, import, rollback, promote, open requests",
  delete: "delete secrets, environments, leases",
  admin: "rename, reorder, protection, review requests",
};

/** Tokens for this project (roll-access's `rlt_…`, for CI and services: `ROLL_TOKEN=… roll run …`), workload identities (OIDC logins), the leases of dynamic credentials, and one-time shares. */
export function AccessView() {
  return (
    <div className="mx-auto max-w-4xl space-y-8 px-4 py-5">
      <Tokens />
      <Identities />
      <Leases />
      <Shares />
      <CodeLeaks />
    </div>
  );
}

function Tokens() {
  const { api, project, standard } = useSecrets();
  const tokens = useLoad(() => api.tokens(), [api]);
  const [creating, setCreating] = useState(false);
  const [made, setMade] = useState<{ name: string; token: string } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const active = (tokens.data ?? []).filter((t) => !t.expires_at || t.expires_at > Date.now());

  return (
    <Section
      title="Tokens"
      aside={
        <button className={button} onClick={() => setCreating((c) => !c)}>
          <Plus className="size-3.5" /> New token
        </button>
      }
    >
      {made && (
        <div className="fade-in mb-3 rounded-xl border border-accent/40 bg-panel p-3 shadow-card inset-shadow-highlight" role="status">
          <p className="text-[12.5px]">
            <b className="font-medium">{made.name}</b> <span className="text-muted">is ready. Copy it now: it isn't shown again.</span>
          </p>
          <div className="mt-2 flex items-center gap-2">
            <code className="min-w-0 flex-1 truncate rounded-md border border-line-2 bg-panel-2 px-2 py-1.5 font-mono text-[12px] select-all">{made.token}</code>
            <button
              className={button}
              onClick={() => {
                void navigator.clipboard?.writeText(made.token);
                setCopied(true);
                setTimeout(() => setCopied(false), 1200);
              }}
            >
              {copied ? <Check className="size-3.5 text-ok" /> : <Copy className="size-3.5" />} Copy
            </button>
            <button className={ghost} onClick={() => setMade(null)}>
              Done
            </button>
          </div>
          <p className="mt-2 font-mono text-[11px] text-subtle">ROLL_TOKEN=… roll run --env production -- &lt;cmd&gt;</p>
        </div>
      )}
      {creating && (
        <NewTokenForm
          envs={standard.map((e) => e.slug)}
          onCancel={() => setCreating(false)}
          onCreate={async (t) => {
            setError(null);
            try {
              const r = await api.createToken(t);
              setMade({ name: r.name, token: r.token });
              setCreating(false);
              tokens.reload();
            } catch (e) {
              setError(errorText(e));
            }
          }}
        />
      )}
      <ErrorNote error={error ?? tokens.error} onDismiss={error ? () => setError(null) : undefined} className="mb-3" />
      {tokens.data && tokens.data.length === 0 && !creating ? (
        <Teach title="No tokens yet" cli={[`ROLL_TOKEN=rlt_… roll run -p ${project.name} --env production -- <cmd>`]}>
          A token lets CI or a service read this project's secrets without your sign-in, limited to the environments, folders and actions you pick.
        </Teach>
      ) : (
        tokens.data &&
        tokens.data.length > 0 && (
          <div className={`${card} divide-y divide-line`}>
            {tokens.data.map((t) => {
              const expired = !!t.expires_at && t.expires_at <= Date.now();
              return (
                <div key={t.id} className={`flex flex-wrap items-center gap-x-3 gap-y-1 px-3 py-2 ${expired ? "opacity-60" : ""}`}>
                  <KeyRound className="size-3.5 text-muted" />
                  <span className="min-w-0 leading-tight">
                    <span className="block text-[12.5px] font-medium">{t.name}</span>
                    <Link to="/dashboard/access/tokens/$id" params={{ id: t.id }} className="block font-mono text-[10.5px] text-subtle hover:text-fg">
                      {t.id}
                    </Link>
                  </span>
                  <span className="flex flex-wrap gap-1 text-[10.5px]">
                    {t.actions.map((a) => (
                      <span key={a} className="rounded border border-line-2 px-1 text-muted">
                        {a}
                      </span>
                    ))}
                  </span>
                  <span className="text-[11px] text-subtle">
                    {t.environments ? t.environments.join(", ") : "all environments"}
                    {t.paths ? ` · ${t.paths.join(", ")}` : ""}
                    {t.allowed_ips ? ` · from ${t.allowed_ips.join(", ")}` : ""}
                    {t.keys ? ` · encryption keys ${t.keys}` : ""}
                  </span>
                  <span className="ml-auto text-right text-[11px] text-subtle">
                    {expired ? "expired" : t.expires_at ? `expires ${until(t.expires_at)}` : "no expiry"} · {t.last_used_at ? `used ${ago(t.last_used_at)}` : "never used"}
                  </span>
                  <Revoke onRevoke={() => api.revokeToken(t.id).then(tokens.reload, (e) => setError(errorText(e)))} />
                </div>
              );
            })}
          </div>
        )
      )}
      {active.length > 0 && <p className="mt-2 text-[11px] text-subtle">These are roll tokens with a rule on this project: open one for its activity, or to change what it reaches. Revoking takes effect at once.</p>}
    </Section>
  );
}

function Revoke({ onRevoke }: { onRevoke: () => void }) {
  const [sure, setSure] = useState(false);
  return sure ? (
    <span className="flex gap-1">
      <button className={dangerButton} onClick={onRevoke}>
        Revoke
      </button>
      <button className={ghost} onClick={() => setSure(false)}>
        Keep
      </button>
    </span>
  ) : (
    <button className={ghost} onClick={() => setSure(true)}>
      Revoke
    </button>
  );
}

function NewTokenForm({ envs, onCreate, onCancel }: { envs: string[]; onCreate: (t: NewToken) => Promise<void>; onCancel: () => void }) {
  const [name, setName] = useState("");
  const [allEnvs, setAllEnvs] = useState(false);
  const [picked, setPicked] = useState<Set<string>>(new Set(envs.includes("production") ? ["production"] : envs.slice(0, 1)));
  const [paths, setPaths] = useState("");
  const [actions, setActions] = useState<Set<Action>>(new Set(["read", "reveal"]));
  const [ttl, setTtl] = useState("90");
  const [ips, setIps] = useState("");
  const [busy, setBusy] = useState(false);
  const pathList = paths.split(/[\s,]+/).filter(Boolean);
  const ipList = ips.split(/[\s,]+/).filter(Boolean);
  const ttlDays = ttl.trim() ? Number(ttl) : undefined;
  const ok = name.trim().length > 0 && actions.size > 0 && (allEnvs || picked.size > 0) && pathList.every((p) => p.startsWith("/")) && (ttlDays === undefined || (Number.isInteger(ttlDays) && ttlDays > 0));
  const toggle = <T,>(set: Set<T>, v: T) => {
    const n = new Set(set);
    if (n.has(v)) n.delete(v);
    else n.add(v);
    return n;
  };
  return (
    <form
      className="rise mb-3 space-y-3 rounded-xl border border-line-2 bg-panel p-3 shadow-card inset-shadow-highlight"
      onSubmit={async (e) => {
        e.preventDefault();
        if (!ok) return;
        setBusy(true);
        await onCreate({
          name: name.trim(),
          ...(allEnvs ? {} : { environments: [...picked] }),
          ...(pathList.length && { paths: pathList }),
          actions: ACTIONS.filter((a) => actions.has(a)),
          ...(ttlDays !== undefined && { ttl_days: ttlDays }),
          ...(ipList.length && { allowed_ips: ipList }),
        });
        setBusy(false);
      }}
      onKeyDown={(e) => e.key === "Escape" && onCancel()}
    >
      <div className="grid gap-3 sm:grid-cols-[minmax(0,2fr)_minmax(0,1fr)]">
        <label>
          <span className={label}>Name</span>
          <input autoFocus value={name} onChange={(e) => setName(e.target.value)} placeholder="github-actions deploy" className={field} />
        </label>
        <label>
          <span className={label}>Expires after (days, empty for never)</span>
          <input value={ttl} onChange={(e) => setTtl(e.target.value)} inputMode="numeric" className={`${field} font-mono`} />
        </label>
      </div>
      <div>
        <span className={label}>Environments</span>
        <div className="flex flex-wrap gap-1">
          <Chip on={allEnvs} onClick={() => setAllEnvs((a) => !a)}>
            All
          </Chip>
          {!allEnvs &&
            envs.map((e) => (
              <Chip key={e} on={picked.has(e)} onClick={() => setPicked((p) => toggle(p, e))}>
                <span className="font-mono">{e}</span>
              </Chip>
            ))}
        </div>
      </div>
      <div>
        <span className={label}>Actions</span>
        <div className="flex flex-wrap gap-1">
          {ACTIONS.map((a) => (
            <Chip key={a} on={actions.has(a)} onClick={() => setActions((s) => toggle(s, a))} title={ACTION_HELP[a]}>
              {a}
            </Chip>
          ))}
        </div>
      </div>
      <label className="block">
        <span className={label}>Folders (path prefixes; empty for all)</span>
        <input value={paths} onChange={(e) => setPaths(e.target.value)} placeholder="/api, /workers" className={`${field} font-mono`} />
      </label>
      <label className="block">
        <span className={label}>Allowed addresses (CIDRs; empty for any)</span>
        <input value={ips} onChange={(e) => setIps(e.target.value)} placeholder="203.0.113.0/24, 2001:db8::/32" className={`${field} font-mono`} />
      </label>
      <div className="flex justify-end gap-2">
        <button type="button" className={ghost} onClick={onCancel}>
          Cancel
        </button>
        <button type="submit" className={primary} disabled={!ok || busy}>
          {busy ? "Creating…" : "Create token"}
        </button>
      </div>
    </form>
  );
}

function Leases() {
  const { api, resourceName } = useSecrets();
  const leases = useLoad(() => api.leases(), [api]);
  const [error, setError] = useState<string | null>(null);
  const [all, setAll] = useState(false);
  const now = Date.now();
  const rows = (leases.data ?? []).filter((l) => all || (!l.revoked_at && l.expires_at > now));
  return (
    <Section
      title="Leases"
      aside={
        <label className="flex items-center gap-1.5 text-[12px] text-muted">
          <input type="checkbox" checked={all} onChange={(e) => setAll(e.target.checked)} className="accent-[var(--accent)]" />
          Show ended
        </label>
      }
    >
      <p className="mb-2 text-[11.5px] text-muted">Credentials issued for dynamic links (one per resolve) and managed credentials retired by a rotation, until they're revoked.</p>
      <ErrorNote error={error ?? leases.error} onDismiss={error ? () => setError(null) : undefined} className="mb-2" />
      {leases.data && rows.length === 0 ? (
        <p className="rounded-xl border border-dashed border-line-2 px-3 py-4 text-[12px] text-muted">No live leases. A dynamic link issues one each time it's resolved, for example by roll run.</p>
      ) : (
        <div className={`${card} divide-y divide-line`}>
          {rows.map((l) => {
            const ended = !!l.revoked_at || l.expires_at <= now;
            return (
              <div key={l.id} className={`flex flex-wrap items-center gap-x-3 gap-y-1 px-3 py-2 ${ended ? "opacity-60" : ""}`}>
                <span className="font-mono text-[12px]">{l.key}</span>
                <span className="font-mono text-[11px] text-subtle">{l.environment}</span>
                {l.link && <LinkChip link={l.link} name={resourceName(l.link)} />}
                <span className="text-[11px] text-subtle">{l.kind === "retired" ? "retired by a rotation" : <ActorMark who={l.consumer} />}</span>
                <span className="ml-auto text-[11px] text-subtle">
                  issued {ago(l.issued_at)} · {l.revoked_at ? `revoked ${ago(l.revoked_at)}` : `ends ${until(l.expires_at)}`}
                </span>
                {!ended && <Revoke onRevoke={() => api.revokeLease(l.id).then(leases.reload, (e) => setError(errorText(e)))} />}
              </div>
            );
          })}
        </div>
      )}
    </Section>
  );
}

/** One-time shares: links that open a value a few times (docs/secrets.md, "Sharing a value once"). The list never has a value or a link. */
function Shares() {
  const { api, project } = useSecrets();
  const shares = useLoad(() => api.shares(), [api]);
  const [error, setError] = useState<string | null>(null);
  const [sharing, setSharing] = useState<ShareSource | null>(null);
  const [all, setAll] = useState(false);
  const now = Date.now();
  const live = (x: { open: boolean; expires_at: number }) => x.open && x.expires_at > now;
  const rows = (shares.data ?? []).filter((x) => all || live(x));
  return (
    <Section
      title="Shares"
      aside={
        <span className="flex items-center gap-3">
          <label className="flex items-center gap-1.5 text-[12px] text-muted">
            <input type="checkbox" checked={all} onChange={(e) => setAll(e.target.checked)} className="accent-[var(--accent)]" />
            Show closed
          </label>
          <button className={button} onClick={() => setSharing({ kind: "value" })}>
            <Send className="size-3.5" /> Share a value…
          </button>
        </span>
      }
    >
      <p className="mb-2 text-[11.5px] text-muted">Links that open a value a few times, for someone outside roll. roll keeps only the ciphertext; the key is in the link, which is shown once.</p>
      <ErrorNote error={error ?? shares.error} onDismiss={error ? () => setError(null) : undefined} className="mb-2" />
      {shares.data && rows.length === 0 ? (
        <p className="rounded-xl border border-dashed border-line-2 px-3 py-4 text-[12px] text-muted">
          No open shares. Share once from a secret&apos;s menu, share a value here, or from the CLI: <span className="font-mono text-soft">roll secrets share -p {project.name} -e staging KEY</span>
        </p>
      ) : (
        <div className={`${card} divide-y divide-line`}>
          {rows.map((x) => {
            const on = live(x);
            const state = on ? `${x.views_left} ${x.views_left === 1 ? "open" : "opens"} left · expires ${until(x.expires_at)}` : x.open ? "expired" : x.opened_at ? `closed · last opened ${ago(x.opened_at)}` : "revoked";
            return (
              <div key={x.id} className={`flex flex-wrap items-center gap-x-3 gap-y-1 px-3 py-2 ${on ? "" : "opacity-60"}`}>
                <span className={`min-w-0 truncate text-[12.5px] ${x.note ? "" : "text-subtle"}`} title={x.note}>
                  {x.note || "No note"}
                </span>
                <ActorMark who={x.created_by} />
                <span className="ml-auto text-[11px] text-subtle">
                  {ago(x.created_at)} · {state}
                </span>
                {on && <Revoke onRevoke={() => api.revokeShare(x.id).then(shares.reload, (e) => setError(errorText(e)))} />}
              </div>
            );
          })}
        </div>
      )}
      <ShareDialog source={sharing} onClose={() => setSharing(null)} onShared={shares.reload} />
    </Section>
  );
}
