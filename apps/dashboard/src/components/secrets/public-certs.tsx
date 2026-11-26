import { Ban, Globe, KeyRound, Plus, RefreshCw, Trash2 } from "lucide-react";
import { useState } from "react";
import { Link } from "@/components/link";
import type { AwsRole, NewPublicCert, PublicCert, PublicCertChange, PublicCertDns, RevokeReason } from "@/lib/secrets-api";
import { secretsHref, useSecrets } from "./secrets-page";
import { ActorMark, Chip, button, card, dangerButton, ErrorNote, errorText, field, ghost, label, primary, Section, stamp, until, useLoad } from "./ui";

/** The public CAs roll orders from (docs/secrets.md, "Publicly trusted certificates"); `eab`: it asks for an External Account Binding. */
const CAS = [
  { url: "https://acme-v02.api.letsencrypt.org/directory", name: "Let's Encrypt", eab: false },
  { url: "https://acme-staging-v02.api.letsencrypt.org/directory", name: "Let's Encrypt (staging)", eab: false },
  { url: "https://acme.zerossl.com/v2/DV90", name: "ZeroSSL", eab: true },
  { url: "https://dv.acme-v02.api.pki.goog/directory", name: "Google Trust Services", eab: true },
  { url: "https://dv.acme-v02.test-api.pki.goog/directory", name: "Google Trust Services (test)", eab: true },
] as const;

const DNS: Record<PublicCertDns, string> = { cloudflare: "Cloudflare", route53: "Route 53" };
const NAME = /^[A-Za-z0-9_-]{1,64}$/;
const DOMAIN = /^(\*\.)?(?!-)[a-z0-9-]{1,63}(\.(?!-)[a-z0-9-]{1,63})+$/;
const REASONS: RevokeReason[] = ["unspecified", "keyCompromise", "superseded", "affiliationChanged", "cessationOfOperation"];

/** Whether the credential fields hold something to send: one of the project's AWS roles (Route 53), or a token or key in its shape. */
const credentialOk = (dns: PublicCertDns, role: string, credential: string) =>
  (dns === "route53" && role !== "") || (dns === "route53" ? /^[A-Za-z0-9]{16,128}:\S+$/.test(credential.trim()) : credential.trim() !== "");

/** What the credential fields send: the role, else the token or key (typed in once; nothing reads it back). */
const credentialBody = (dns: PublicCertDns, role: string, credential: string): Pick<PublicCertChange, "aws_role" | "dns_credential"> =>
  dns === "route53" && role !== "" ? { aws_role: role } : { dns_credential: credential.trim() };

const caName = (url: string) => CAS.find((c) => c.url === url)?.name ?? url;
const day = (ms: number) => new Date(ms).toLocaleDateString("en-US", { month: "short", day: "numeric", year: "numeric" });

/** Where its renewals stand: the next one due (none before its first certificate: the status says it's on its way), and the caps on asking (one an hour, three a week). */
function renewal(c: PublicCert, now = Date.now()) {
  const next = c.renews_at === 0 ? "renewing now" : c.renews_at ? `renews ${until(c.renews_at, now).replace("expired", "now")}` : c.status === "failing" ? "retried hourly" : "";
  const caps = c.renew_after ? `renewals held ${until(c.renew_after, now)}` : `${c.renewals_asked} of 3 renewals this week`;
  return { next, caps };
}

/**
 * Publicly trusted certificates (docs/secrets.md, "Publicly trusted certificates"): roll orders them
 * from Let's Encrypt, ZeroSSL or Google over ACME, proves the names with dns-01 through Cloudflare or
 * Route 53, renews them, and rotates the keys linked to them. The DNS credential goes in once: no
 * answer ever holds it, so it's never shown again; a certificate's own form replaces it (or moves
 * Route 53 between an access key and a project AWS role) over `PATCH`, which orders at once.
 */
export function PublicCerts() {
  const { api, project } = useSecrets();
  const list = useLoad(() => api.publicCerts(), [api]);
  const roles = useLoad(() => api.awsRoles().catch(() => []), [api]);
  const [adding, setAdding] = useState(false);
  const [name, setName] = useState("");
  const [names, setNames] = useState("");
  const [ca, setCa] = useState<string>(CAS[0].url);
  const [dns, setDns] = useState<PublicCertDns>("cloudflare");
  const [role, setRole] = useState("");
  const [credential, setCredential] = useState("");
  const [eabKid, setEabKid] = useState("");
  const [eabKey, setEabKey] = useState("");
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [confirm, setConfirm] = useState<{ id: string; what: "revoke" | "delete" } | null>(null);
  const [reason, setReason] = useState<RevokeReason>("unspecified");
  const [replacing, setReplacing] = useState<{ id: string; role: string; credential: string } | null>(null);
  const rolesHref = `${secretsHref(project.id, { view: "credentials" })}#aws-roles`;

  const domains = names.split(/[\s,]+/).map((d) => d.trim().toLowerCase().replace(/\.$/, "")).filter(Boolean);
  const eab = CAS.find((c) => c.url === ca)?.eab ?? false;
  const ok = NAME.test(name) && domains.length > 0 && domains.length <= 100 && domains.every((d) => DOMAIN.test(d)) && credentialOk(dns, role, credential) && (!eab || (eabKid.trim() !== "" && eabKey.trim() !== ""));

  // What was typed in goes with the form: nothing reads it back.
  const reset = () => {
    setAdding(false);
    setName("");
    setNames("");
    setCredential("");
    setEabKid("");
    setEabKey("");
    setRole("");
  };
  const add = async () => {
    setBusy("add");
    setError(null);
    const body: NewPublicCert = { name, directory_url: ca, domains, dns_provider: dns, ...credentialBody(dns, role, credential) };
    if (eab) Object.assign(body, { eab_kid: eabKid.trim(), eab_hmac_key: eabKey.trim() });
    try {
      await api.addPublicCert(body);
      reset();
      list.reload();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(null);
    }
  };
  // A new credential is checked with the provider, kept sealed, and orders at once; the form (and
  // what was typed in it) goes once it's taken.
  const replace = async (c: PublicCert) => {
    if (!replacing) return;
    setBusy(`${c.id}:replace`);
    setError(null);
    try {
      await api.updatePublicCert(c.id, credentialBody(c.dns_provider, replacing.role, replacing.credential));
      setReplacing(null);
      list.reload();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(null);
    }
  };
  const act = (c: PublicCert, what: string, call: () => Promise<unknown>) => {
    setBusy(`${c.id}:${what}`);
    setError(null);
    call().then(
      () => (setBusy(null), setConfirm(null), list.reload()),
      (e) => (setBusy(null), setConfirm(null), setError(errorText(e)), list.reload()),
    );
  };

  return (
    <Section
      title="Publicly trusted certificates"
      aside={
        <button className={button} onClick={() => (adding ? reset() : setAdding(true))}>
          <Plus className="size-3.5" /> Add a public certificate
        </button>
      }
    >
      <p className="mb-2 text-[11.5px] text-muted">
        Certificates browsers trust, for endpoints other people reach: roll orders them from Let&apos;s Encrypt, ZeroSSL or Google, proves the names through your DNS provider (a TXT record per name),
        renews them, and rotates every key linked to one (<span className="font-mono">TLS_CERT</span>, <span className="font-mono">TLS_KEY</span>, <span className="font-mono">TLS_FULLCHAIN</span>). Each
        renewal spends the CA&apos;s weekly limit for the names, so asking is capped at one an hour and three a week.
      </p>
      <ErrorNote error={error ?? list.error} onDismiss={error ? () => setError(null) : undefined} className="mb-2" />
      {adding && (
        <form
          className="rise mb-3 grid gap-3 rounded-xl border border-line-2 bg-panel p-3 sm:grid-cols-2"
          onSubmit={(e) => {
            e.preventDefault();
            if (ok && !busy) void add();
          }}
        >
          <label>
            <span className={label}>Name</span>
            <input autoFocus value={name} onChange={(e) => setName(e.target.value)} placeholder="web" spellCheck={false} className={`${field} font-mono text-[12px]`} />
          </label>
          <label>
            <span className={label}>CA</span>
            <select value={ca} onChange={(e) => setCa(e.target.value)} className={`${field} text-[12px]`}>
              {CAS.map((c) => (
                <option key={c.url} value={c.url}>
                  {c.name}
                </option>
              ))}
            </select>
          </label>
          <label className="sm:col-span-2">
            <span className={label}>Names (comma-separated; a wildcard like *.example.com too)</span>
            <input value={names} onChange={(e) => setNames(e.target.value)} placeholder="example.com, *.example.com" spellCheck={false} className={`${field} font-mono text-[12px]`} />
          </label>
          <div className="sm:col-span-2">
            <span className={label}>DNS provider</span>
            <span className="flex gap-1.5">
              {(Object.keys(DNS) as PublicCertDns[]).map((d) => (
                <Chip key={d} on={dns === d} onClick={() => (setDns(d), setCredential(""), setRole(d === "route53" ? (roles.data?.[0]?.name ?? "") : ""))}>
                  {DNS[d]}
                </Chip>
              ))}
            </span>
          </div>
          <DnsFields dns={dns} roles={roles.data ?? []} rolesHref={rolesHref} role={role} setRole={setRole} credential={credential} setCredential={setCredential} />
          {eab && (
            <>
              <label>
                <span className={label}>EAB key id</span>
                <input value={eabKid} onChange={(e) => setEabKid(e.target.value)} spellCheck={false} className={`${field} font-mono text-[12px]`} />
              </label>
              <label>
                <span className={label}>EAB HMAC key</span>
                <input type="password" autoComplete="off" value={eabKey} onChange={(e) => setEabKey(e.target.value)} spellCheck={false} className={`${field} font-mono text-[12px]`} />
              </label>
            </>
          )}
          <p className="text-[11px] text-subtle sm:col-span-2">
            <DnsHint dns={dns} role={role} />
          </p>
          <span className="flex gap-2 sm:col-span-2">
            <button type="submit" className={primary} disabled={!ok || busy !== null}>
              {busy === "add" ? "Checking…" : "Add certificate"}
            </button>
            <button type="button" className={ghost} onClick={reset}>
              Cancel
            </button>
          </span>
        </form>
      )}
      {list.data && list.data.length === 0 && !adding ? (
        <p className="rounded-xl border border-dashed border-line-2 px-3 py-4 text-[12px] text-muted">No public certificates. Add one to have roll order, renew and rotate a certificate browsers trust.</p>
      ) : (
        list.data && (
          <div className={`${card} divide-y divide-line`}>
            {list.data.map((c) => {
              const r = renewal(c);
              const pending = c.revocations?.filter((v) => v.state === "pending") ?? [];
              return (
                <div key={c.id} className="px-3 py-2">
                  <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
                    <Globe className="size-3.5 text-muted" />
                    <span className="font-mono text-[12px]">{c.name}</span>
                    <Status status={c.status} />
                    <span className="min-w-0 truncate font-mono text-[11.5px] text-subtle">{c.domains.join(", ")}</span>
                    <span className="ml-auto flex items-center gap-1">
                      <button
                        className={ghost}
                        disabled={busy !== null || c.renew_after !== undefined}
                        onClick={() => act(c, "renew", () => api.renewPublicCert(c.id))}
                        aria-label={`Renew ${c.name}`}
                        title={c.renew_after ? `Renewals are held until ${stamp(c.renew_after)}: each spends the CA's weekly limit for these names` : "Order a new certificate now; linked keys rotate once it's issued"}
                      >
                        <RefreshCw className={`size-3.5 ${busy === `${c.id}:renew` ? "animate-spin motion-reduce:animate-none" : ""}`} />
                      </button>
                      <button
                        className={ghost}
                        disabled={busy !== null}
                        onClick={() => (setConfirm(null), setReplacing(replacing?.id === c.id ? null : { id: c.id, role: c.dns_provider === "route53" ? (c.aws_role ?? roles.data?.[0]?.name ?? "") : "", credential: "" }))}
                        aria-label={`Replace ${c.name}'s credential`}
                        title={c.dns_provider === "route53" ? "Replace the DNS credential, or move between an access key and an AWS role" : "Replace the DNS credential"}
                      >
                        <KeyRound className="size-3.5" />
                      </button>
                      <button className={ghost} disabled={busy !== null || !c.serial} onClick={() => (setReplacing(null), setConfirm({ id: c.id, what: "revoke" }))} aria-label={`Revoke ${c.name}`} title="Revoke at the CA">
                        <Ban className="size-3.5" />
                      </button>
                      <button className={ghost} disabled={busy !== null} onClick={() => (setReplacing(null), setConfirm({ id: c.id, what: "delete" }))} aria-label={`Remove ${c.name}`} title="Remove (refused while a key links to it)">
                        <Trash2 className="size-3.5" />
                      </button>
                    </span>
                  </div>
                  <div className="mt-0.5 flex flex-wrap gap-x-3 pl-[22px] text-[11px] text-subtle">
                    <span>{caName(c.directory_url)}</span>
                    <span>{c.aws_role ? `Route 53 as AWS role ${c.aws_role}` : DNS[c.dns_provider]}</span>
                    {c.not_after && <span className="font-mono">until {day(c.not_after)}</span>}
                    {r.next && <span>{r.next}</span>}
                    <span className={c.renew_after ? "text-fg" : ""}>{r.caps}</span>
                    <span className="ml-auto">
                      <ActorMark who={c.created_by} />
                    </span>
                  </div>
                  {c.last_error && <p className="mt-1 pl-[22px] text-[11px] text-danger">{c.last_error}</p>}
                  {pending.map((v) => (
                    <p key={v.serial} className="mt-1 pl-[22px] text-[11px] text-danger">
                      Not revoked yet: <span className="font-mono">{v.serial}</span>
                      {v.last_error ? ` (${v.last_error})` : ""}; roll keeps asking.
                    </p>
                  ))}
                  {replacing?.id === c.id && (
                    <form
                      className="rise mt-2 ml-[22px] grid gap-3 rounded-xl border border-line-2 bg-panel p-3 sm:grid-cols-2"
                      onSubmit={(e) => {
                        e.preventDefault();
                        if (credentialOk(c.dns_provider, replacing.role, replacing.credential) && !busy) void replace(c);
                      }}
                    >
                      <DnsFields
                        dns={c.dns_provider}
                        roles={roles.data ?? []}
                        rolesHref={rolesHref}
                        role={replacing.role}
                        setRole={(role) => setReplacing({ ...replacing, role })}
                        credential={replacing.credential}
                        setCredential={(credential) => setReplacing({ ...replacing, credential })}
                        autoFocus
                      />
                      <p className="text-[11px] text-subtle sm:col-span-2">
                        <DnsHint dns={c.dns_provider} role={replacing.role} /> roll orders a new certificate at once with it.
                      </p>
                      <span className="flex gap-2 sm:col-span-2">
                        <button type="submit" className={primary} disabled={!credentialOk(c.dns_provider, replacing.role, replacing.credential) || busy !== null}>
                          {busy === `${c.id}:replace` ? "Checking…" : "Replace credential"}
                        </button>
                        <button type="button" className={ghost} onClick={() => setReplacing(null)}>
                          Cancel
                        </button>
                      </span>
                    </form>
                  )}
                  {confirm?.id === c.id && (
                    <div className="rise mt-2 flex flex-wrap items-center gap-2 pl-[22px] text-[11.5px]">
                      {confirm.what === "revoke" ? (
                        <>
                          <span className="text-muted">Revoke the certificate in use at the CA? Its keys get a new one within minutes.</span>
                          <select value={reason} onChange={(e) => setReason(e.target.value as RevokeReason)} aria-label="Reason" className={`${field} h-7 w-auto text-[11.5px]`}>
                            {REASONS.map((x) => (
                              <option key={x}>{x}</option>
                            ))}
                          </select>
                          <button className={dangerButton} disabled={busy !== null} onClick={() => act(c, "revoke", () => api.revokePublicCert(c.id, { serial: "current", reason }))}>
                            Revoke
                          </button>
                          {c.previous_serial && (
                            <button className={button} disabled={busy !== null} onClick={() => act(c, "revoke", () => api.revokePublicCert(c.id, { serial: "previous", reason }))}>
                              Revoke the previous one
                            </button>
                          )}
                        </>
                      ) : (
                        <>
                          <span className="text-muted">Remove {c.name}? Its certificates stay valid until they end.</span>
                          <button className={dangerButton} disabled={busy !== null} onClick={() => act(c, "delete", () => api.deletePublicCert(c.id))}>
                            Remove
                          </button>
                        </>
                      )}
                      <button className={ghost} onClick={() => setConfirm(null)}>
                        Keep
                      </button>
                    </div>
                  )}
                </div>
              );
            })}
          </div>
        )
      )}
    </Section>
  );
}

/**
 * The DNS credential's fields: Cloudflare's API token, or for Route 53 one of the project's AWS
 * roles (listed first: no key is kept) or an access key. A password field that's never filled from
 * an answer: the credential is typed in once.
 */
function DnsFields(p: { dns: PublicCertDns; roles: AwsRole[]; rolesHref: string; role: string; setRole: (r: string) => void; credential: string; setCredential: (c: string) => void; autoFocus?: boolean }) {
  const asRole = p.dns === "route53" && p.role !== "";
  return (
    <>
      {p.dns === "route53" && (
        <label>
          <span className={label}>Sign in to AWS as</span>
          <select autoFocus={p.autoFocus && p.roles.length > 0} value={p.role} onChange={(e) => (p.setRole(e.target.value), p.setCredential(""))} className={`${field} text-[12px]`}>
            {p.roles.map((r) => (
              <option key={r.id} value={r.name}>
                AWS role {r.name} (no key kept)
              </option>
            ))}
            <option value="">An access key</option>
          </select>
          {p.roles.length === 0 && (
            <span className="mt-1 block text-[11px] text-subtle">
              No AWS role yet:{" "}
              <Link href={p.rolesHref} className="text-accent hover:underline">
                add one
              </Link>{" "}
              and no key is kept.
            </span>
          )}
        </label>
      )}
      {!asRole && (
        <label className={p.dns === "route53" ? "" : "sm:col-span-2"}>
          <span className={label}>{p.dns === "route53" ? "Access key (ACCESS_KEY_ID:SECRET_ACCESS_KEY)" : "API token (Zone DNS edit on the names' zones)"}</span>
          <input
            type="password"
            autoComplete="off"
            autoFocus={p.autoFocus && p.dns !== "route53"}
            value={p.credential}
            onChange={(e) => p.setCredential(e.target.value)}
            spellCheck={false}
            className={`${field} font-mono text-[12px]`}
          />
        </label>
      )}
    </>
  );
}

function DnsHint({ dns, role }: { dns: PublicCertDns; role: string }) {
  return dns === "route53" && role !== ""
    ? "roll writes the records as the role, a short session each time, named roll-public-cert-… and limited to _acme-challenge TXT records: give it route53:ChangeResourceRecordSets and ListResourceRecordSets on the zone, ListHostedZonesByName and GetChange."
    : "Checked with the provider first, then kept sealed: roll never shows it again, and the certificate's key button replaces it.";
}

function Status({ status }: { status: PublicCert["status"] }) {
  const tone = status === "issued" ? "text-ok" : status === "failing" ? "text-danger" : "text-accent";
  const text = status === "pending" ? "ordering" : status;
  return (
    <span className={`inline-flex items-center gap-1 text-[11px] ${tone}`}>
      <span className={`size-1.5 rounded-full bg-current ${status === "pending" ? "animate-pulse motion-reduce:animate-none" : ""}`} />
      {text}
    </span>
  );
}
