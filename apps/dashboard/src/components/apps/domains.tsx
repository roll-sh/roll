import { ExternalLink, Globe, Lock, Pencil, RefreshCw, ShieldAlert, Trash2 } from "lucide-react";
import { useState } from "react";
import { appsApi, type Domain, type Environment } from "@/lib/apps-api";
import { btn, CopyButton, input, panel, primary, useCopy } from "../ui";
import { SureButton, type Runner } from "./shared";

/** Where a domain's TXT challenge goes: a wildcard's on the name under it (as the server checks). */
const challengeName = (host: string) => `_roll-challenge.${host.replace(/^\*\./, "")}`;

const STATUS: Record<Domain["status"], { label: string; dot: string }> = {
  pending: { label: "Waiting for DNS", dot: "bg-accent pulse-dot" },
  issuing: { label: "Issuing certificate", dot: "bg-accent pulse-dot" },
  active: { label: "Active", dot: "bg-ok" },
  failed: { label: "Failed", dot: "bg-danger" },
};

/** The certificate next to a custom domain (nothing where certificates aren't issued). */
const TLS: Record<Exclude<Domain["tls"], "none">, { label: string; icon: typeof Lock; tone: string }> = {
  pending: { label: "Certificate pending", icon: Lock, tone: "text-subtle" },
  issued: { label: "Certificate issued", icon: Lock, tone: "text-muted" },
  failed: { label: "Certificate failed", icon: ShieldAlert, tone: "text-danger" },
};

function TlsBadge({ d }: { d: Domain }) {
  if (d.tls === "none") return null;
  const t = TLS[d.tls];
  const Icon = t.icon;
  return (
    <span className={`inline-flex items-center gap-1 text-[12px] ${t.tone}`} title={d.tls_error ?? undefined}>
      <Icon className="size-3" strokeWidth={1.5} />
      {t.label}
    </span>
  );
}

/** The generated hostname, then custom domains: their DNS record, Verify and Remove. */
/** A custom domain's own port (Railway's target port): an admin UI beside the app. Saved on
 *  Enter or when it loses focus; empty is the service's own port. */
function PortField({ port, busy, onSave }: { port?: number; busy: boolean; onSave: (p: number | null) => unknown }) {
  const [v, setV] = useState(port ? String(port) : "");
  const save = () => {
    const n = v.trim() ? Number(v) : null;
    if ((n ?? undefined) === port || (n !== null && !(Number.isInteger(n) && n >= 1 && n <= 65535))) return;
    void onSave(n);
  };
  return (
    <label className="flex flex-wrap items-center gap-2 text-[12px] text-muted">
      On the app's port
      <input
        value={v}
        inputMode="numeric"
        placeholder="its own"
        disabled={busy}
        onChange={(e) => setV(e.target.value.replace(/\D/g, ""))}
        onBlur={save}
        onKeyDown={(e) => e.key === "Enter" && save()}
        className="h-7 w-20 rounded-md border border-line bg-panel px-2 font-mono text-[12px] outline-none"
        aria-label="The app's port this domain goes to"
      />
      {port ? <span className="text-subtle">from the deploy after it was set</span> : null}
    </label>
  );
}

/** A zone's apex (`example.com`, `example.co.uk`), as the server tells a TXT-proved one: its
 *  `www.` is the pair Vercel and Render offer with it. */
const SECOND_LEVEL = ["co", "com", "org", "net", "ac", "gov", "edu"];
const isApex = (h: string) => {
  const labels = h.split(".");
  return labels.length === 2 || (labels.length === 3 && SECOND_LEVEL.includes(labels[1]!));
};
/** The other half of `example.com` / `www.example.com`, when `h` is one of them. */
export const wwwPair = (h: string): string | null => (h.startsWith("www.") && isApex(h.slice(4)) ? h.slice(4) : !h.startsWith("*.") && isApex(h) ? `www.${h}` : null);

export function DomainsTab({ service, env, runner }: { service: string; env: Environment; runner: Runner }) {
  const [host, setHost] = useState("");
  // Its www pair, added beside it and redirecting to it (Vercel's and Render's).
  const [pairToo, setPairToo] = useState(true);
  // Renaming the generated name: what's typed, while the field is open.
  const [rename, setRename] = useState<string | null>(null);
  const [renamed, setRenamed] = useState(false);
  // A name another environment holds, offered to claim (its add answered so).
  const [claiming, setClaiming] = useState<string | null>(null);
  const { copied, copy } = useCopy();
  const generated = env.url ? new URL(env.url).hostname : null;
  const [label, suffix] = generated ? [generated.split(".")[0]!, generated.slice(generated.indexOf("."))] : ["", ""];
  const nameOk = rename !== null && /^(?!.*--)[a-z0-9][a-z0-9-]{1,54}[a-z0-9]$/.test(rename);
  const saveName = async () => {
    if (rename === null || rename === label) return setRename(null);
    if (await runner.run("hostname", () => appsApi.putEnvironment(service, env.name, { hostname: rename }))) {
      setRename(null);
      setRenamed(true);
    }
  };
  const roll = env.domains.filter((d) => d.kind === "roll");
  const custom = env.domains.filter((d) => d.kind === "custom");
  // Where a domain may redirect: the environment's hostnames that serve it.
  const targets = env.domains.filter((d) => d.status === "active");
  const valid = /^(?=.{1,253}$)([a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z]{2,63}$/.test(host.trim());

  const h = host.trim();
  const pair = valid ? wwwPair(h) : null;
  const pairNew = pair && !env.domains.some((d) => d.hostname === pair) ? pair : null;
  const add = async () => {
    setClaiming(null);
    const done = await runner.run("domain", async () => {
      await appsApi.addDomain(service, env.name, h).catch((e: unknown) => {
        if (e instanceof Error && e.message.includes("/claim")) setClaiming(h);
        throw e;
      });
      if (pairNew && pairToo) {
        await appsApi.addDomain(service, env.name, pairNew);
        await appsApi.redirectDomain(service, env.name, pairNew, h);
      }
      return true;
    });
    if (done) setHost("");
  };

  return (
    <div className="space-y-6">
      <section className={panel}>
        <div className="flex min-h-14 items-center justify-between border-b border-line px-5 py-3">
          <h2 className="text-[15px] font-medium">roll domain</h2>
          {generated && rename === null && (
            <button type="button" className={btn} onClick={() => setRename(label)} title="Change its name on the roll domain">
              <Pencil className="size-3.5" /> Change
            </button>
          )}
        </div>
        {rename !== null && (
          <form
            className="rise flex flex-wrap items-center gap-2 border-b border-line px-5 py-3"
            onSubmit={(e) => {
              e.preventDefault();
              void saveName();
            }}
          >
            <div className="flex min-w-0 flex-1 items-center">
              <input autoFocus value={rename} onChange={(e) => setRename(e.target.value.toLowerCase())} aria-label="Name on the roll domain" spellCheck={false} className={`${input} min-w-0 flex-1 !rounded-r-none font-mono text-[12.5px]`} />
              <span className="flex h-8 items-center rounded-r-md border border-l-0 border-line-2 px-2 font-mono text-[12.5px] text-subtle">{suffix}</span>
            </div>
            <button type="submit" className={primary} disabled={!nameOk || runner.busy !== null}>
              {runner.busy === "hostname" ? <span className="shimmer">Saving…</span> : "Save"}
            </button>
            <button type="button" className={btn} onClick={() => setRename(null)}>
              Cancel
            </button>
            <p className="w-full text-[12px] text-subtle">
              {nameOk || rename === "" ? `${label}${suffix} stops answering at once; anyone can take it after.` : "3–56 of a–z, 0–9 and -, not starting or ending with -, without --."}
            </p>
          </form>
        )}
        {renamed && <p className="border-b border-line px-5 py-3 text-[12.5px] text-muted">Renamed. Redeploy to give the app its new <code className="font-mono text-fg">ROLL_PUBLIC_URL</code>.</p>}
        {roll.length === 0 && !env.url && <p className="px-5 py-4 text-[13px] text-muted">The environment gets its URL with its first deploy.</p>}
        <ul className="divide-y divide-line">
          {(roll.length ? roll.map((d) => `https://${d.hostname}`) : env.url ? [env.url] : []).map((url) => (
            <li key={url} className="flex items-center gap-3 px-5 py-3">
              <Globe className="size-3.5 text-subtle" />
              <a href={url} target="_blank" rel="noreferrer" className="min-w-0 flex-1 truncate font-mono text-[12.5px] text-accent hover:underline">
                {url.replace(/^https?:\/\//, "")}
              </a>
              <CopyButton id={url} text={url} copied={copied} onCopy={copy} />
            </li>
          ))}
        </ul>
      </section>

      <section className={panel}>
        <div className="flex min-h-14 items-center justify-between border-b border-line px-5 py-3">
          <h2 className="text-[15px] font-medium">Custom domains</h2>
        </div>
        <ul className="divide-y divide-line">
          {custom.length === 0 && <li className="px-5 py-4 text-[13px] text-muted">None yet. Add one, then point it at this service.</li>}
          {custom.map((d) => {
            const s = STATUS[d.status];
            return (
              <li key={d.hostname} className="space-y-3 px-5 py-4">
                <div className="flex flex-wrap items-center gap-3">
                  <a href={`https://${d.hostname}`} target="_blank" rel="noreferrer" className="inline-flex items-center gap-1 font-mono text-[13px] hover:underline">
                    {d.hostname} <ExternalLink className="size-3 text-subtle" />
                  </a>
                  <span className="inline-flex items-center gap-1.5 text-[12px] text-muted">
                    <i className={`size-1.5 rounded-full ${s.dot}`} />
                    {s.label}
                  </span>
                  <TlsBadge d={d} />
                  <span className="ml-auto flex gap-2">
                    {d.status !== "active" && (
                      <button className={btn} disabled={runner.busy !== null} onClick={() => runner.run(`verify:${d.hostname}`, () => appsApi.verifyDomain(service, env.name, d.hostname))}>
                        <RefreshCw className={`size-3.5 ${runner.busy === `verify:${d.hostname}` ? "animate-spin motion-reduce:animate-none" : ""}`} /> Verify
                      </button>
                    )}
                    <SureButton
                      className={btn}
                      disabled={runner.busy !== null}
                      sure="Sure? It stops answering"
                      onSure={() => runner.run(`remove:${d.hostname}`, () => appsApi.removeDomain(service, env.name, d.hostname))}
                    >
                      <Trash2 className="size-3.5" /> Remove
                    </SureButton>
                  </span>
                </div>
                {d.status === "active" && (
                  <label className="flex flex-wrap items-center gap-2 text-[12px] text-muted">
                    {d.redirect_to ? "Redirects (308) to" : "Serves the app, or redirects to"}
                    <select
                      value={d.redirect_to ?? ""}
                      disabled={runner.busy !== null}
                      onChange={(e) => runner.run(`redirect:${d.hostname}`, () => appsApi.redirectDomain(service, env.name, d.hostname, e.target.value || null))}
                      className="h-7 rounded-md border border-line bg-panel px-2 font-mono text-[12px] outline-none"
                      aria-label={`Where ${d.hostname} goes`}
                    >
                      <option value="">serve the app</option>
                      {targets
                        .filter((t) => t.hostname !== d.hostname && !t.redirect_to)
                        .map((t) => (
                          <option key={t.hostname} value={t.hostname}>
                            {t.hostname}
                          </option>
                        ))}
                    </select>
                  </label>
                )}
                {d.kind === "custom" && d.status === "active" && !d.redirect_to && env.egress?.mode !== "roll" && (
                  <PortField key={`${d.hostname}:${d.port ?? ""}`} port={d.port} busy={runner.busy !== null} onSave={(p) => runner.run(`port:${d.hostname}`, () => appsApi.domainPort(service, env.name, d.hostname, p))} />
                )}
                {d.error && <p className="text-[12px] text-danger">{d.error}</p>}
                {d.kind === "custom" && d.status !== "active" && (
                  <p className="text-[12px] text-subtle">
                    {(() => {
                      // Unverified custom domains are removed 7 days after they're added (the name is someone's to claim).
                      const days = Math.ceil((d.created_at + 7 * 86_400_000 - Date.now()) / 86_400_000);
                      return days > 1 ? `Removed in ${days} days unless its DNS checks out.` : "Removed within a day unless its DNS checks out.";
                    })()}
                  </p>
                )}
                {d.status === "active" && d.tls !== "issued" && d.tls_error && (
                  <p className={`text-[12px] ${d.tls === "failed" ? "text-danger" : "text-subtle"}`}>{d.tls_error}</p>
                )}
                {d.status !== "active" && (
                  <div className="overflow-x-auto rounded-lg border border-line bg-bg">
                    <table className="w-full font-mono text-[11.5px]">
                      <thead className="text-left text-subtle">
                        <tr>
                          {["type", "name", "value", ""].map((h) => (
                            <th key={h} className="border-b border-line px-3 py-1.5 font-normal">
                              {h}
                            </th>
                          ))}
                        </tr>
                      </thead>
                      <tbody>
                        {d.record !== "txt" && <Record type="CNAME" name={d.hostname} value={d.target} copied={copied} copy={copy} />}
                        {d.record !== "cname" && d.token && (
                          <Record type="TXT" name={challengeName(d.hostname)} value={d.token} copied={copied} copy={copy} />
                        )}
                      </tbody>
                    </table>
                  </div>
                )}
                {d.status !== "active" && (
                  <p className="text-[12px] text-subtle">
                    {d.record === "both"
                      ? "Add both records with your DNS provider, then Verify: the CNAME routes the domain to roll, the TXT record proves it's yours. The certificate is issued once they resolve."
                      : "Add the CNAME with your DNS provider, then Verify. At a zone's apex, where a CNAME isn't allowed, use your provider's ALIAS or flattening and add the TXT record. The certificate is issued once it resolves."}
                  </p>
                )}
                {d.kind === "custom" && d.status !== "active" && !d.hostname.startsWith("*.") && <OnCloudflare service={service} env={env.name} host={d.hostname} runner={runner} />}
              </li>
            );
          })}
        </ul>
        <form
          className="flex flex-wrap items-center gap-2 border-t border-line px-5 py-3"
          onSubmit={(e) => {
            e.preventDefault();
            if (valid) void add();
          }}
        >
          <input value={host} onChange={(e) => setHost(e.target.value.toLowerCase())} placeholder="www.example.com or *.example.com" className={`${input} min-w-0 flex-1 font-mono text-[12px]`} spellCheck={false} />
          <button type="submit" className={primary} disabled={!valid || runner.busy !== null}>
            Add domain
          </button>
          {pairNew && (
            <label className="flex w-full items-center gap-2 text-[12px] text-muted">
              <input type="checkbox" checked={pairToo} onChange={(e) => setPairToo(e.target.checked)} />
              Also add <span className="font-mono text-fg">{pairNew}</span>, redirecting to {h}
            </label>
          )}
        </form>
        {claiming && <ClaimDomain key={claiming} service={service} env={env.name} host={claiming} runner={runner} copied={copied} copy={copy} onDone={() => (setClaiming(null), setHost(""))} />}
      </section>
    </div>
  );
}

/** A name another environment holds, taken by proving it (Vercel's "linked to another team"):
 *  the TXT record to add, then a check that moves the domain here. */
function ClaimDomain({ service, env, host, runner, copied, copy, onDone }: { service: string; env: string; host: string; runner: Runner; copied: string | null; copy: (k: string, t: string) => void; onDone: () => void }) {
  const [txt, setTxt] = useState<{ name: string; value: string } | null>(null);
  const ask = () =>
    runner.run(`claim:${host}`, async () => {
      const r = await appsApi.claimDomain(service, env, host);
      if (r.claimed) onDone();
      else setTxt(r.txt);
    });
  return (
    <div className="space-y-2 border-t border-line px-5 py-3 text-[12.5px]">
      <p className="text-muted">
        {host} is another environment's. If its DNS is yours, prove it with a TXT record and take it: it stops answering there.
      </p>
      {txt && (
        <table className="w-full overflow-hidden rounded-lg border border-line font-mono text-[12px]">
          <tbody>
            <Record type="TXT" name={txt.name} value={txt.value} copied={copied} copy={copy} />
          </tbody>
        </table>
      )}
      <button type="button" className={btn} disabled={runner.busy !== null} onClick={() => void ask()}>
        {txt ? "Check it and take the domain" : "Claim it"}
      </button>
    </div>
  );
}

/** The records written at the customer's own Cloudflare with their token (Vercel's own DNS):
 *  a `roll://` reference to it, read as the person for this one call, never kept. */
function OnCloudflare({ service, env, host, runner }: { service: string; env: string; host: string; runner: Runner }) {
  const [open, setOpen] = useState(false);
  const [ref, setRef] = useState("");
  // Its name's A and AAAA records (the old host's) deleted for roll's CNAME.
  const [replace, setReplace] = useState(false);
  if (!open)
    return (
      <button type="button" className="text-[12px] text-accent hover:underline" onClick={() => setOpen(true)}>
        Its DNS is on Cloudflare? Write the records there
      </button>
    );
  return (
    <form
      className="flex flex-wrap items-center gap-2"
      onSubmit={(e) => {
        e.preventDefault();
        void runner.run(`cloudflare:${host}`, () => appsApi.configureDomain(service, env, host, ref.trim(), replace));
      }}
    >
      <input value={ref} onChange={(e) => setRef(e.target.value)} placeholder="roll://production/CLOUDFLARE_TOKEN" aria-label="Cloudflare token's roll:// reference" spellCheck={false} className={`${input} min-w-64 flex-1 font-mono text-[12px]`} />
      <button type="submit" className={btn} disabled={runner.busy !== null || !ref.trim().startsWith("roll://")}>
        {runner.busy === `cloudflare:${host}` ? <span className="shimmer">Writing…</span> : "Write and verify"}
      </button>
      <label className="flex items-center gap-1.5 text-[12px] text-muted">
        <input type="checkbox" checked={replace} onChange={(e) => setReplace(e.target.checked)} />
        Replace its A and AAAA records
      </label>
      <span className="w-full text-[12px] text-subtle">A roll secret holding a Cloudflare API token with Zone:DNS:Edit on this domain's zone, read as you for this once and never kept. A name still pointing at another host keeps its records unless you replace them.</span>
    </form>
  );
}

export function Record({ type, name, value, copied, copy }: { type: string; name: string; value: string; copied: string | null; copy: (k: string, t: string) => void }) {
  return (
    <tr className="border-b border-line last:border-0">
      <td className="px-3 py-1.5 text-muted">{type}</td>
      <td className="px-3 py-1.5">{name}</td>
      <td className="px-3 py-1.5">
        {value}
      </td>
      <td className="w-8 px-2">
        <CopyButton id={`${type}:${name}`} text={value} copied={copied} onCopy={copy} />
      </td>
    </tr>
  );
}
