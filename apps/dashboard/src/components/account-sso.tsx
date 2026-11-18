import { Building2, Copy } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { accessApi, tieForAgents, untieForAgents, type EnterpriseIdp } from "@/lib/access-api";
import { accountsApi, ssoDomains, type SsoConnection } from "@/lib/accounts-api";

const card = "rounded-xl border border-line-2 bg-sheet inset-shadow-highlight";
const btn = "flex h-7 items-center gap-1.5 rounded-md border border-line-2 bg-raised px-2.5 text-[12px] font-medium inset-shadow-highlight transition-colors hover:border-line-3 disabled:opacity-50";
const primary = "h-8 rounded-lg bg-fg px-3.5 text-[12.5px] font-medium text-bg transition-opacity hover:opacity-90 disabled:opacity-40";
const field = "h-8 rounded-lg border border-line-2 bg-canvas px-2.5 text-[12.5px] outline-none transition-colors focus:border-accent/60";
const errorText = (e: unknown) => (e instanceof Error ? e.message : String(e));

/**
 * Admins' single sign-on (docs/accounts.md, "SSO"): a company's OpenID Connect provider for its
 * email domains, people linked by their verified email, optionally provisioned, optionally the
 * only way in for those domains. The client secret is sealed by roll and never shown again.
 */
export function SsoSection() {
  const [list, setList] = useState<SsoConnection[] | null>(null);
  const [adding, setAdding] = useState(false);
  const [removing, setRemoving] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const load = useCallback(() => accountsApi.ssoConnections().then(setList, (e) => setError(errorText(e))), []);
  useEffect(() => void load(), [load]);
  // Enterprise sign-in's providers (agents): a connection can be one too, set up once.
  const [idps, setIdps] = useState<EnterpriseIdp[] | null>(null);
  useEffect(() => {
    accessApi.enterprise().then((e) => setIdps(e.idps), () => setIdps(null));
  }, []);
  const saveIdps = (next: EnterpriseIdp[]) => accessApi.setIdps(next).then((r) => setIdps(r.idps), (e) => setError(errorText(e)));
  const change = (id: string, c: { enforce?: boolean; provision?: boolean; session_hours?: number }) => accountsApi.updateSso(id, c).then(load, (e) => setError(errorText(e)));
  return (
    <section id="sso" className="rise scroll-mt-16">
      <div className="flex items-end gap-3">
        <div className="min-w-0 flex-1">
          <h2 className="text-[15px] font-semibold tracking-[-0.03em]">Single sign-on</h2>
          <p className="mt-0.5 text-[12.5px] text-muted">
            Your company&apos;s OpenID Connect provider (Okta, Entra ID, Google Workspace) signs its people in, chosen by their email&apos;s domain.
          </p>
        </div>
        {!adding && (
          <button onClick={() => setAdding(true)} className={btn}>
            Connect a provider
          </button>
        )}
      </div>
      <div className="mt-3 space-y-3">
        {adding && (
          <NewConnection
            onCancel={() => setAdding(false)}
            onMade={() => {
              setAdding(false);
              void load();
            }}
          />
        )}
        {error && <p className="text-[12px] text-danger">{error}</p>}
        {list && list.length === 0 && !adding && (
          <p className={`${card} flex items-center gap-3 px-4 py-3 text-[12.5px] text-subtle`}>
            <Building2 className="size-4" strokeWidth={1.5} />
            None: people sign in by email code, GitHub or Google.
          </p>
        )}
        {list?.map((c) => (
          <div key={c.id} className={`${card} px-4 py-3 text-[12.5px]`}>
            <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
              <span className="font-medium">{c.name}</span>
              <span className="font-mono text-[11.5px] text-subtle">{c.issuer}</span>
              <span className="text-muted">
                {c.domains.map((d, i) => (
                  <span key={d}>
                    {i > 0 && ", "}
                    {d}
                    {!c.verified.includes(d) && <span className="ml-1 text-[11px] text-subtle">(not verified)</span>}
                  </span>
                ))}
              </span>
              <span className="flex-1" />
              {removing === c.id ? (
                <span className="flex items-center gap-2">
                  <span className="text-muted">Its people&apos;s links go; their accounts stay.</span>
                  <button onClick={() => accountsApi.removeSso(c.id).then(() => (setRemoving(null), load()), (e) => setError(errorText(e)))} className={`${btn} text-danger`}>
                    Remove
                  </button>
                  <button onClick={() => setRemoving(null)} className={btn}>
                    Cancel
                  </button>
                </span>
              ) : (
                <button onClick={() => setRemoving(c.id)} className="text-[12px] text-muted hover:text-danger">
                  Remove
                </button>
              )}
            </div>
            {c.domains.some((d) => !c.verified.includes(d)) && (
              <div className="mt-2 rounded-lg border border-line-2 bg-canvas px-3 py-2 text-[12px] text-muted">
                <p>
                  A domain counts once its DNS proves it&apos;s yours: add a TXT record at <span className="font-mono text-fg">{c.dns.name}</span> (for each domain) with the
                  value below, then verify. Until then nobody signs in through this provider there.
                </p>
                <div className="mt-1.5 flex items-center gap-2">
                  <span className="min-w-0 flex-1 truncate font-mono text-fg">{c.dns.value}</span>
                  <button onClick={() => navigator.clipboard.writeText(c.dns.value)} className={btn} aria-label="Copy the TXT value">
                    <Copy className="size-3.5" />
                  </button>
                  <button
                    onClick={() =>
                      accountsApi.verifySso(c.id).then(
                        (n) => {
                          setError(n.verified.length === c.verified.length ? `No new domain had the record yet (DNS can take a while).` : null);
                          void load();
                        },
                        (e) => setError(errorText(e)),
                      )
                    }
                    className={btn}
                  >
                    Verify
                  </button>
                </div>
              </div>
            )}
            <Scim connection={c} onChange={load} onError={setError} />
            {idps && (
              <div className="mt-2 flex flex-wrap items-center gap-2 text-[12px] text-muted">
                {idps.some((i) => i.sso === c.id) ? (
                  <>
                    Agents sign in through it too (enterprise sign-in, for its verified domains).
                    <button onClick={() => void saveIdps(untieForAgents(idps, c.id))} className={btn}>
                      Stop
                    </button>
                  </>
                ) : (
                  <>
                    Let it sign in your people&apos;s agents too (enterprise sign-in), for the domains it proved.
                    <button disabled={c.verified.length === 0} onClick={() => void saveIdps(tieForAgents(idps, c))} className={btn}>
                      Use for agents too
                    </button>
                  </>
                )}
              </div>
            )}
            <div className="mt-2 flex flex-wrap items-center gap-x-5 gap-y-1.5 text-[12px] text-muted">
              <label className="flex items-center gap-1.5">
                <input type="checkbox" checked={c.enforce} onChange={(e) => void change(c.id, { enforce: e.target.checked })} />
                Required for these domains (their other sessions end)
              </label>
              <label className="flex items-center gap-1.5">
                <input type="checkbox" checked={c.provision} onChange={(e) => void change(c.id, { provision: e.target.checked })} />
                Make accounts for new people (never admins)
              </label>
              <label className="flex items-center gap-1.5">
                Sessions last
                <select value={c.session_hours ?? 0} onChange={(e) => void change(c.id, { session_hours: Number(e.target.value) })} className="rounded-md border border-line-2 bg-canvas px-1.5 py-0.5 text-[12px]">
                  {[0, 1, 8, 12, 24, 72, 168].map((h) => (
                    <option key={h} value={h}>
                      {h === 0 ? "30 days" : h < 24 ? `${h} hours` : h === 24 ? "1 day" : `${h / 24} days`}
                    </option>
                  ))}
                </select>
                then sign in through it again
              </label>
              <span className="flex min-w-0 items-center gap-1.5">
                redirect URI <span className="truncate font-mono text-[11.5px] text-fg">{c.redirect_uri}</span>
                <button onClick={() => navigator.clipboard.writeText(c.redirect_uri)} className={btn} aria-label="Copy the redirect URI">
                  <Copy className="size-3.5" />
                </button>
              </span>
            </div>
          </div>
        ))}
      </div>
    </section>
  );
}

/** SCIM: the provider creates and deactivates its people; its token is shown once. */
function Scim({ connection: c, onChange, onError }: { connection: SsoConnection; onChange: () => void; onError: (e: string | null) => void }) {
  const [made, setMade] = useState<{ token: string; base_url: string } | null>(null);
  const mint = () =>
    accountsApi.scimToken(c.id).then(
      (m) => {
        setMade(m);
        onChange();
      },
      (e) => onError(errorText(e)),
    );
  return (
    <div className="mt-2 text-[12px] text-muted">
      {made ? (
        <div className="space-y-1.5 rounded-lg border border-accent/40 bg-accent/[0.05] px-3 py-2">
          <p>In the provider&apos;s SCIM settings (copy the token now: it isn&apos;t shown again):</p>
          {[
            ["Base URL", made.base_url],
            ["Bearer token", made.token],
          ].map(([label, value]) => (
            <div key={label} className="flex items-center gap-2">
              <span className="w-24 shrink-0 text-subtle">{label}</span>
              <span className="min-w-0 flex-1 truncate font-mono text-fg">{value}</span>
              <button onClick={() => navigator.clipboard.writeText(value)} className={btn} aria-label={`Copy the ${label}`}>
                <Copy className="size-3.5" />
              </button>
            </div>
          ))}
          <button onClick={() => setMade(null)} className={btn}>
            Done
          </button>
        </div>
      ) : (
        <span className="flex flex-wrap items-center gap-2">
          {c.scim_leaked && (
            <span className="w-full text-danger">
              Its SCIM token was {c.scim_leaked}: roll revoked it. The provider stops provisioning people until you make a new token and give it there.
            </span>
          )}
          {c.scim ? "The provider manages its people over SCIM (it deactivates whoever leaves)." : "Let the provider create and deactivate its people (SCIM)."}
          <button onClick={mint} className={c.scim_leaked ? `${btn} font-medium text-danger` : btn}>
            {c.scim || c.scim_leaked ? "New token" : "Turn on SCIM"}
          </button>
          {c.scim && (
            <button onClick={() => accountsApi.scimOff(c.id).then(onChange, (e) => onError(errorText(e)))} className={btn}>
              Turn off
            </button>
          )}
        </span>
      )}
    </div>
  );
}

function NewConnection({ onMade, onCancel }: { onMade: () => void; onCancel: () => void }) {
  const [f, setF] = useState({ name: "", issuer: "", client_id: "", client_secret: "", domains: "", enforce: false, provision: false });
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const parsed = ssoDomains(f.domains);
  const problem =
    !/^https:\/\/\S+$/.test(f.issuer.trim()) ? "The issuer is the provider's https URL" : !f.client_id.trim() || !f.client_secret.trim() ? "The client id and secret roll is registered with there" : "problem" in parsed ? parsed.problem : null;
  const set = (k: keyof typeof f) => (e: { target: { value: string } }) => setF({ ...f, [k]: e.target.value });
  const save = async () => {
    if (!("domains" in parsed)) return;
    setBusy(true);
    setError(null);
    try {
      await accountsApi.addSso({ name: f.name.trim() || undefined, issuer: f.issuer.trim(), client_id: f.client_id.trim(), client_secret: f.client_secret, domains: parsed.domains, enforce: f.enforce, provision: f.provision });
      onMade();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className={`${card} space-y-3 p-4`}>
      <div className="grid gap-3 sm:grid-cols-2">
        <input value={f.issuer} onChange={set("issuer")} placeholder="Issuer: https://acme.okta.com" spellCheck={false} className={`${field} font-mono`} />
        <input value={f.name} onChange={set("name")} placeholder="Name (Acme Okta)" className={field} />
        <input value={f.client_id} onChange={set("client_id")} placeholder="Client id" spellCheck={false} className={`${field} font-mono`} />
        <input type="password" value={f.client_secret} onChange={set("client_secret")} placeholder="Client secret" autoComplete="off" className={`${field} font-mono`} />
        <input value={f.domains} onChange={set("domains")} placeholder="Email domains: acme.com, acme.co.uk" spellCheck={false} className={`${field} font-mono sm:col-span-2`} />
      </div>
      <div className="flex flex-wrap gap-x-5 gap-y-1.5 text-[12px] text-muted">
        <label className="flex items-center gap-1.5">
          <input type="checkbox" checked={f.enforce} onChange={(e) => setF({ ...f, enforce: e.target.checked })} />
          Required for these domains (an admin token still works if the provider is down)
        </label>
        <label className="flex items-center gap-1.5">
          <input type="checkbox" checked={f.provision} onChange={(e) => setF({ ...f, provision: e.target.checked })} />
          Make accounts for new people
        </label>
      </div>
      <p className="text-[12px] text-subtle">
        roll reads the provider&apos;s discovery document now. Once saved, register the redirect URI it shows at the provider, and prove each domain with the TXT
        record it shows. Admins link their own accounts from Sign-in methods.
      </p>
      {error && <p className="text-[12px] text-danger">{error}</p>}
      <div className="flex items-center gap-2">
        <button onClick={save} disabled={busy || !!problem} className={primary}>
          {busy ? "Connecting…" : "Connect"}
        </button>
        <button onClick={onCancel} className={btn}>
          Cancel
        </button>
        {problem && <span className="text-[12px] text-subtle">{problem}</span>}
      </div>
    </div>
  );
}
