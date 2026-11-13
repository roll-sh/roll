import { ExternalLink, Globe, Lock, RefreshCw, ShieldAlert, Trash2 } from "lucide-react";
import { useState } from "react";
import { appsApi, type Domain, type Environment } from "@/lib/apps-api";
import { btn, CopyButton, input, panel, primary, useCopy } from "../ui";
import { SureButton, type Runner } from "./shared";

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
export function DomainsTab({ service, env, runner }: { service: string; env: Environment; runner: Runner }) {
  const [host, setHost] = useState("");
  const { copied, copy } = useCopy();
  const roll = env.domains.filter((d) => d.kind === "roll");
  const custom = env.domains.filter((d) => d.kind === "custom");
  // Where a domain may redirect: the environment's hostnames that serve it.
  const targets = env.domains.filter((d) => d.status === "active");
  const valid = /^(?=.{1,253}$)([a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z]{2,63}$/.test(host.trim());

  const add = async () => {
    if (await runner.run("domain", () => appsApi.addDomain(service, env.name, host.trim()))) setHost("");
  };

  return (
    <div className="space-y-6">
      <section className={panel}>
        <div className="flex min-h-14 items-center border-b border-line px-5 py-3">
          <h2 className="text-[15px] font-medium">roll domain</h2>
        </div>
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
                {d.error && <p className="text-[12px] text-danger">{d.error}</p>}
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
                          <Record type="TXT" name={`_roll-challenge.${d.hostname}`} value={d.token} copied={copied} copy={copy} />
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
          <input value={host} onChange={(e) => setHost(e.target.value.toLowerCase())} placeholder="www.example.com" className={`${input} min-w-0 flex-1 font-mono text-[12px]`} spellCheck={false} />
          <button type="submit" className={primary} disabled={!valid || runner.busy !== null}>
            Add domain
          </button>
        </form>
      </section>
    </div>
  );
}

function Record({ type, name, value, copied, copy }: { type: string; name: string; value: string; copied: string | null; copy: (k: string, t: string) => void }) {
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
