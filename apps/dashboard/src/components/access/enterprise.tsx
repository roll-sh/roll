import { Building2 } from "lucide-react";
import { useEffect, useState } from "react";
import {
  accessApi,
  catalogFor,
  idpFromForm,
  presetRules,
  publicJwk,
  type Catalog,
  type Enterprise,
  type EnterpriseApp,
  type EnterpriseIdp,
  type Rule,
} from "@/lib/access-api";
import { btn, Confirm, CopyButton, ErrorNote, input, Panel, primary, useCopy } from "../ui";
import { Builder } from "./builder";
import { relative } from "./shell";

/**
 * Enterprise sign-in (docs/permissions.md, "Enterprise identity providers"): the identity
 * providers roll trusts to vouch for its people, and the apps they may vouch for. An app trades a
 * provider's ID-JAG for a token acting as the person, with no consent screen. The operator's only:
 * hidden when the config doesn't answer.
 */
export function EnterprisePanel() {
  const [e, setE] = useState<Enterprise | null>(null);
  const [error, setError] = useState<string | null>(null);
  const load = async () => {
    try {
      setE(await accessApi.enterprise());
    } catch {
      setE(null);
    }
  };
  useEffect(() => {
    void load();
  }, []);
  if (!e) return null;
  const saveIdps = async (idps: EnterpriseIdp[]) => {
    setError(null);
    try {
      const r = await accessApi.setIdps(idps);
      setE({ ...e, idps: r.idps });
      return true;
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
      return false;
    }
  };
  return (
    <Panel title="Enterprise sign-in">
      <p className="border-b border-line px-5 py-2.5 text-[12px] text-muted">
        Your company&apos;s identity provider (Okta&apos;s Cross App Access, MCP&apos;s enterprise-managed authorization) decides which of its
        people may use an app with roll. The app trades the provider&apos;s assertion for an hour&apos;s token acting as the person: no
        consent screen, never more than their account reaches, and revoking their connection here keeps them out.
      </p>
      <Providers idps={e.idps} audience={e.audience} save={saveIdps} />
      <Apps apps={e.apps} tokenEndpoint={e.token_endpoint} refresh={load} hasIdp={e.idps.length > 0} />
      {error && (
        <div className="px-5 pb-4">
          <ErrorNote error={error} />
        </div>
      )}
    </Panel>
  );
}

function Providers({ idps, audience, save }: { idps: EnterpriseIdp[]; audience: string; save: (idps: EnterpriseIdp[]) => Promise<boolean> }) {
  const [adding, setAdding] = useState(false);
  const { copied, copy } = useCopy();
  return (
    <section className="border-b border-line">
      <div className="flex items-center gap-3 px-5 pt-4 text-[12px]">
        <span className="font-medium text-fg">Identity providers</span>
        <span className="flex min-w-0 items-center gap-1 text-subtle">
          audience <code className="truncate font-mono">{audience}</code>
          <CopyButton id="enterprise-audience" text={audience} copied={copied} onCopy={copy} />
        </span>
        <span className="flex-1" />
        {!adding && (
          <button onClick={() => setAdding(true)} className={btn}>
            Trust a provider
          </button>
        )}
      </div>
      {adding && (
        <NewIdp
          onCancel={() => setAdding(false)}
          onAdd={async (idp) => {
            const ok = await save([...idps.filter((i) => i.issuer !== idp.issuer), idp]);
            if (ok) setAdding(false);
            return ok;
          }}
        />
      )}
      {idps.length === 0 && !adding ? (
        <div className="flex items-center gap-3 px-5 py-4 text-[12px] text-subtle">
          <Building2 className="size-4" strokeWidth={1.5} />
          None trusted: no provider can sign anyone in.
        </div>
      ) : (
        <ul className="divide-y divide-line pt-2">
          {idps.map((i) => (
            <li key={i.issuer} className="flex flex-wrap items-center gap-x-3 gap-y-1 px-5 py-2.5 text-[12px]">
              <span className="font-medium">{i.name ?? i.issuer.replace(/^https:\/\//, "")}</span>
              <code className="font-mono text-subtle">{i.issuer}</code>
              <span className="text-muted">
                {(i.matching ?? "email") === "email" ? `people at ${(i.domains ?? []).join(", ")}` : "names roll accounts (usr_…)"}
              </span>
              {i.admins && <span className="text-accent">signs in admins</span>}
              {i.jwks && <span className="text-subtle">keys pinned</span>}
              <span className="flex-1" />
              <button onClick={() => void save(idps.filter((x) => x.issuer !== i.issuer))} className="text-muted hover:text-danger">
                Stop trusting
              </button>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}

function NewIdp({ onAdd, onCancel }: { onAdd: (idp: EnterpriseIdp) => Promise<boolean>; onCancel: () => void }) {
  const [f, setF] = useState({ issuer: "", name: "", matching: "email" as "email" | "aud_sub", domains: "", jwks: "", admins: false });
  const [busy, setBusy] = useState(false);
  const parsed = idpFromForm(f);
  const problem = "problem" in parsed ? parsed.problem : null;
  const set = (k: keyof typeof f) => (ev: { target: { value: string } }) => setF({ ...f, [k]: ev.target.value });
  const add = async () => {
    if (!("idp" in parsed)) return;
    setBusy(true);
    await onAdd(parsed.idp);
    setBusy(false);
  };
  return (
    <div className="rise space-y-4 p-5">
      <div className="grid gap-4 sm:grid-cols-2">
        <label className="block space-y-1.5">
          <span className="block text-[12px] text-subtle">Issuer</span>
          <input autoFocus value={f.issuer} onChange={set("issuer")} placeholder="https://acme.okta.com" spellCheck={false} className={`${input} w-full font-mono text-[12px]`} />
        </label>
        <label className="block space-y-1.5">
          <span className="block text-[12px] text-subtle">Name</span>
          <input value={f.name} onChange={set("name")} placeholder="Okta" maxLength={64} className={`${input} w-full`} />
        </label>
        <label className="block space-y-1.5">
          <span className="block text-[12px] text-subtle">How a person is found</span>
          <select value={f.matching} onChange={set("matching")} className={`${input} w-full`}>
            <option value="email">By their verified email</option>
            <option value="aud_sub">The provider names their roll account</option>
          </select>
        </label>
        <label className="block space-y-1.5">
          <span className="block text-[12px] text-subtle">Email domains it vouches for</span>
          <input value={f.domains} onChange={set("domains")} placeholder="acme.com, acme.co.uk" spellCheck={false} className={`${input} w-full font-mono text-[12px]`} />
        </label>
      </div>
      <label className="block space-y-1.5">
        <span className="block text-[12px] text-subtle">Pinned keys (optional: its JWKS, for a provider roll can&apos;t reach; otherwise read from its metadata)</span>
        <textarea value={f.jwks} onChange={set("jwks")} rows={2} spellCheck={false} placeholder='{"keys": [...]}' className={`${input} w-full resize-y font-mono text-[12px]`} />
      </label>
      <label className="flex items-start gap-2 text-[12px] text-muted">
        <input type="checkbox" checked={f.admins} onChange={(ev) => setF({ ...f, admins: ev.target.checked })} className="mt-0.5" />
        <span>
          It may sign in roll&apos;s admins too. They act as the operator, and the provider&apos;s sign-in replaces roll&apos;s own second factor
          for them.
        </span>
      </label>
      <div className="flex items-center gap-3">
        <button onClick={add} disabled={busy || !!problem} className={primary}>
          {busy ? <span className="shimmer">Saving…</span> : "Trust it"}
        </button>
        <button onClick={onCancel} className={btn}>
          Cancel
        </button>
        {problem && <span className="text-[12px] text-subtle">{problem}</span>}
      </div>
    </div>
  );
}

function Apps({ apps, tokenEndpoint, refresh, hasIdp }: { apps: EnterpriseApp[]; tokenEndpoint: string; refresh: () => Promise<void>; hasIdp: boolean }) {
  const [making, setMaking] = useState(false);
  const [made, setMade] = useState<(EnterpriseApp & { client_secret?: string }) | null>(null);
  const [removing, setRemoving] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const remove = async (id: string) => {
    setBusy(true);
    setError(null);
    try {
      const r = await accessApi.removeEnterpriseApp(id);
      setNote(r.revoked ? `Removed; ${r.revoked} ${r.revoked === 1 ? "connection" : "connections"} revoked.` : "Removed.");
      setRemoving(null);
      if (made?.client_id === id) setMade(null);
      await refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <section>
      <div className="flex items-center gap-3 px-5 pt-4 text-[12px]">
        <span className="font-medium text-fg">Apps</span>
        {note && <span className="text-subtle">{note}</span>}
        <span className="flex-1" />
        {!making && (
          <button onClick={() => (setMade(null), setNote(null), setMaking(true))} className={btn}>
            Register an app
          </button>
        )}
      </div>
      {made && <Made made={made} tokenEndpoint={tokenEndpoint} />}
      {making && (
        <NewApp
          onCancel={() => setMaking(false)}
          onMade={async (m) => {
            setMaking(false);
            setMade(m);
            await refresh();
          }}
        />
      )}
      {apps.length === 0 && !making && !made ? (
        <p className="px-5 py-4 text-[12px] text-subtle">
          None registered. {hasIdp ? "Register the app your provider vouches for: its client id goes in the provider's assertions." : "Trust a provider first, then register the apps it vouches for."}
        </p>
      ) : (
        <ul className="divide-y divide-line pt-2">
          {apps.map((a) => (
            <li key={a.client_id} className="px-5 py-2.5 text-[12px]">
              <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
                <span className="font-medium">{a.name}</span>
                <code className="font-mono text-subtle">{a.client_id}</code>
                <span className="text-muted">{a.signs_with_key ? "signs with a key" : "signs with a secret"}</span>
                <span className="text-subtle">{rulesSummary(a.rules)}</span>
                <span className="text-subtle">added {relative(a.created_at)}</span>
                <span className="flex-1" />
                {removing !== a.client_id && (
                  <button onClick={() => setRemoving(a.client_id)} className="text-muted hover:text-danger">
                    Remove
                  </button>
                )}
              </div>
              {removing === a.client_id && (
                <div className="pt-3">
                  <Confirm word={a.name} action="Remove the app" busy={busy} onConfirm={() => void remove(a.client_id)} onCancel={() => setRemoving(null)}>
                    Every person&apos;s connection to {a.name} is revoked with it, and their tokens stop at once.
                  </Confirm>
                </div>
              )}
            </li>
          ))}
        </ul>
      )}
      {error && (
        <div className="px-5 pb-4">
          <ErrorNote error={error} />
        </div>
      )}
    </section>
  );
}

/** "read and write in code, read in secrets": the actions an app's rules allow, by product. */
function rulesSummary(rules: Rule[]): string {
  const by = new Map<string, Set<string>>();
  for (const r of rules) {
    const s = by.get(r.product) ?? new Set<string>();
    for (const [act, eff] of Object.entries(r.actions ?? {})) if (eff) s.add(act);
    by.set(r.product, s);
  }
  return [...by].map(([p, acts]) => `${[...acts].join(", ") || "nothing"} in ${p === "*" ? "every product" : p}`).join("; ");
}

function NewApp({ onMade, onCancel }: { onMade: (m: EnterpriseApp & { client_secret?: string }) => Promise<void>; onCancel: () => void }) {
  const [catalog, setCatalog] = useState<Catalog | null>(null);
  const [name, setName] = useState("");
  const [rules, setRules] = useState<Rule[]>([]);
  const [key, setKey] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    accessApi
      .catalog()
      .then(async (c) => {
        c = catalogFor(c, await accessApi.me().catch(() => null));
        setCatalog(c);
        const start = c.presets.find((p) => p.id === "read-only") ?? c.presets[0];
        if (start) setRules(presetRules(start));
      })
      .catch((e) => setError(e instanceof Error ? e.message : String(e)));
  }, []);
  const parsed = key.trim() ? publicJwk(key) : null;
  const problems = [...(name.trim() ? [] : ["Name the app"]), ...(rules.length ? [] : ["Allow at least one action"]), ...(parsed && "problem" in parsed ? [parsed.problem] : [])];
  const create = async () => {
    setBusy(true);
    setError(null);
    try {
      const public_jwk = parsed && "jwk" in parsed ? parsed.jwk : null;
      await onMade(await accessApi.addEnterpriseApp({ name: name.trim(), rules, ...(public_jwk ? { public_jwk } : {}) }));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
  if (!catalog) return error ? <div className="p-5"><ErrorNote error={error} /></div> : null;
  return (
    <div className="rise space-y-4 p-5">
      <p className="text-[12px] text-muted">The most it may do for a person: never more than that person&apos;s own account reaches.</p>
      <Builder catalog={catalog} rules={rules} onChange={setRules} />
      <label className="block space-y-1.5">
        <span className="block text-[12px] text-subtle">Name</span>
        <input autoFocus value={name} onChange={(e) => setName(e.target.value)} placeholder="Claude, the support desk" maxLength={64} className={`${input} w-full sm:w-1/2`} />
      </label>
      <label className="block space-y-1.5">
        <span className="block text-[12px] text-subtle">
          Public key (optional: the app signs private_key_jwt with it; an Ed25519 or P-256 PEM, or a JWK). Without one, roll makes a secret and
          shows it once.
        </span>
        <textarea
          value={key}
          onChange={(e) => setKey(e.target.value)}
          rows={3}
          spellCheck={false}
          placeholder={"-----BEGIN PUBLIC KEY-----\n…\n-----END PUBLIC KEY-----"}
          className={`${input} w-full resize-y font-mono text-[12px]`}
        />
      </label>
      <ErrorNote error={error} />
      <div className="flex items-center gap-3">
        <button onClick={create} disabled={busy || problems.length > 0} className={primary}>
          {busy ? <span className="shimmer">Registering…</span> : parsed && "jwk" in parsed ? "Register with this key" : "Register with a secret"}
        </button>
        <button onClick={onCancel} className={btn}>
          Cancel
        </button>
        {problems.length > 0 && <span className="text-[12px] text-subtle">{problems[0]}</span>}
      </div>
    </div>
  );
}

function Made({ made, tokenEndpoint }: { made: EnterpriseApp & { client_secret?: string }; tokenEndpoint: string }) {
  const { copied, copy } = useCopy();
  const field = (label: string, id: string, text: string) => (
    <div className="space-y-1">
      <span className="block text-[12px] text-subtle">{label}</span>
      <div className="flex items-center gap-1 rounded-lg border border-line bg-bg px-3 py-2">
        <code className="min-w-0 flex-1 truncate font-mono text-[12px]">{text}</code>
        <CopyButton id={id} text={text} copied={copied} onCopy={copy} />
      </div>
    </div>
  );
  return (
    <div className="rise space-y-3 p-5">
      <p className="text-[13px] font-medium">{made.name} is registered</p>
      <div className="grid gap-3 sm:grid-cols-2">
        {field("Client id (the provider puts it in the assertion's client_id)", "enterprise-client-id", made.client_id)}
        {field("Token endpoint", "enterprise-token-endpoint", tokenEndpoint)}
      </div>
      {made.client_secret && field("Client secret: copy it now, it isn't shown again", "enterprise-client-secret", made.client_secret)}
      <p className="text-[12px] text-muted">
        The app sends the provider&apos;s ID-JAG as <code className="font-mono">assertion</code> with{" "}
        <code className="font-mono">grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer</code>, authenticating as itself
        {made.client_secret ? " with this secret." : " with a JWT its key signed."}
      </p>
    </div>
  );
}
