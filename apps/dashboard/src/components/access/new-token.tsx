import { HorizonMark } from "@roll/brand";
import { useEffect, useState } from "react";
import { accessApi, describeRule, EXPIRIES, ipProblems, presetRules, type Catalog, type Grant, type Rule } from "@/lib/access-api";
import { Link } from "../link";
import { btn, Connecting, CopyButton, ErrorNote, input, Panel, primary, useCopy } from "../ui";
import { Builder } from "./builder";
import { ConnectPanel } from "./overview";
import { AccessShell } from "./shell";

/** `/dashboard/access/new`: the permission builder, then the token, shown once. */
export function NewToken() {
  const [catalog, setCatalog] = useState<Catalog | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [name, setName] = useState("");
  const [rules, setRules] = useState<Rule[]>([]);
  const [expiry, setExpiry] = useState(1);
  const [ips, setIps] = useState("");
  const [busy, setBusy] = useState(false);
  const [made, setMade] = useState<{ grant: Grant; token: string } | null>(null);

  useEffect(() => {
    accessApi
      .catalog()
      .then((c) => {
        setCatalog(c);
        const start = c.presets.find((p) => p.id === "read-only") ?? c.presets[0];
        if (start) setRules(presetRules(start));
      })
      .catch((e) => setError(e instanceof Error ? e.message : String(e)));
  }, []);

  const ipList = ips.split(/[\s,]+/).filter(Boolean);
  const problems = [...(name.trim() ? [] : ["Name the token"]), ...(rules.length ? [] : ["Allow at least one action"]), ...ipProblems(ipList)];

  const create = async () => {
    setBusy(true);
    setError(null);
    try {
      const ms = EXPIRIES[expiry].ms;
      setMade(await accessApi.create({ name: name.trim(), rules, expires_at: ms === null ? null : Date.now() + ms, ip_allow: ipList }));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  if (made) return <Made made={made} />;
  return (
    <AccessShell tab="new">
      <div className="space-y-6">
        <div>
          <h1 className="text-xl font-semibold tracking-[-0.04em]">New token</h1>
          <p className="mt-1 max-w-2xl text-[13px] text-muted">
            A token can do exactly what you choose here, on the API, from the CLI and over MCP. Give it the least it needs: you can
            always make another.
          </p>
        </div>
        {!catalog ? (
          error ? <ErrorNote error={error} /> : <Connecting />
        ) : (
          <>
            <Panel title="What it may do">
              <div className="p-5">
                <Builder catalog={catalog} rules={rules} onChange={(r) => setRules(r)} />
              </div>
            </Panel>
            <Panel title="Name, lifetime and network">
              <div className="grid gap-4 p-5 sm:grid-cols-3">
                <label className="block space-y-1.5">
                  <span className="block text-[12px] text-subtle">Name</span>
                  <input autoFocus value={name} onChange={(e) => setName(e.target.value)} placeholder="ci, cursor on my laptop" maxLength={64} className={`${input} w-full`} />
                </label>
                <label className="block space-y-1.5">
                  <span className="block text-[12px] text-subtle">Expires after</span>
                  <select value={expiry} onChange={(e) => setExpiry(Number(e.target.value))} className={`${input} w-full`}>
                    {EXPIRIES.map((x, i) => (
                      <option key={x.label} value={i}>
                        {x.label}
                      </option>
                    ))}
                  </select>
                </label>
                <label className="block space-y-1.5">
                  <span className="block text-[12px] text-subtle">Only from (optional)</span>
                  <input value={ips} onChange={(e) => setIps(e.target.value)} placeholder="10.0.0.0/8, 203.0.113.4" className={`${input} w-full font-mono text-[12px]`} />
                </label>
              </div>
            </Panel>
            <div className="space-y-2">
              {rules.map((r, i) => (
                <p key={i} className="text-[12px] text-muted">
                  {describeRule(r)}
                </p>
              ))}
            </div>
            <ErrorNote error={error} />
            <div className="flex items-center gap-3">
              <button onClick={create} disabled={busy || problems.length > 0} className={primary}>
                {busy ? <span className="shimmer">Creating…</span> : "Create token"}
              </button>
              <Link href="/dashboard/access" className={btn}>
                Cancel
              </Link>
              {problems.length > 0 && <span className="text-[12px] text-subtle">{problems[0]}</span>}
            </div>
          </>
        )}
      </div>
    </AccessShell>
  );
}

function Made({ made }: { made: { grant: Grant; token: string } }) {
  const { copied, copy } = useCopy();
  return (
    <AccessShell tab="new">
      <div className="space-y-6">
        <section className="rise flex items-start gap-4 rounded-xl border border-line bg-panel p-5">
          <HorizonMark state="success" size={28} />
          <div className="min-w-0 flex-1 space-y-3">
            <div>
              <h1 className="text-[15px] font-medium">{made.grant.name} is ready</h1>
              <p className="mt-1 text-[13px] text-muted">Copy it now: it isn&apos;t shown again. roll keeps only its hash.</p>
            </div>
            <div className="flex items-center gap-1 rounded-lg border border-line bg-bg px-3 py-2">
              <code className="min-w-0 flex-1 truncate font-mono text-[12px]">{made.token}</code>
              <CopyButton id="token" text={made.token} copied={copied} onCopy={copy} />
            </div>
            <p className="text-[12px] text-subtle">
              Use it as <code className="font-mono">ROLL_TOKEN</code> for the CLI, or <code className="font-mono">Authorization: Bearer</code> for
              the API and MCP.
            </p>
          </div>
        </section>
        <ConnectPanel token={made.token} />
        <Link href="/dashboard/access" className={btn}>
          Done
        </Link>
      </div>
    </AccessShell>
  );
}
