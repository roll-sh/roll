import { Bot } from "lucide-react";
import { useEffect, useState } from "react";
import { accessApi, catalogFor, EXPIRIES, presetRules, publicJwk, statusOf, type Catalog, type MachineClient, type MadeMachineClient, type Rule } from "@/lib/access-api";
import { btn, CopyButton, ErrorNote, input, Panel, primary, useCopy } from "../ui";
import { Builder } from "./builder";
import { GrantRow } from "./overview";

/**
 * Machine clients (docs/permissions.md, "Machine clients"): automation with no person (CI, a
 * scheduled agent) that trades a JWT its key signed, or a secret, for an hour's token held to the
 * rules chosen here. The operator's: the panel is shown only when the list answers.
 */
export function MachineClients({ clients, refresh }: { clients: MachineClient[]; refresh: () => Promise<void> }) {
  const [making, setMaking] = useState(false);
  const [made, setMade] = useState<MadeMachineClient | null>(null);
  const live = clients.filter((c) => statusOf(c.grant) === "active");
  const done = async (m: MadeMachineClient) => {
    setMaking(false);
    setMade(m);
    await refresh();
  };
  return (
    <Panel
      title="Machine clients"
      aside={
        <span className="flex items-center gap-3">
          {live.length > 0 && <span className="font-mono text-[12px] text-subtle">{live.length} live</span>}
          {!making && (
            <button onClick={() => (setMade(null), setMaking(true))} className={btn}>
              New machine client
            </button>
          )}
        </span>
      }
    >
      <p className="border-b border-line px-5 py-2.5 text-[12px] text-muted">
        Automation with no person (CI, a scheduled agent) signs in with OAuth client credentials: it trades a JWT signed with its key,
        or a secret, for an hour&apos;s token that can do only what its rules allow. Revoking one ends its tokens at once.
      </p>
      {made && <Made made={made} />}
      {making && <NewClient onMade={done} onCancel={() => setMaking(false)} />}
      {live.length === 0 && !making && !made && (
        <div className="flex items-center gap-3 px-5 py-4 text-[12px] text-subtle">
          <Bot className="size-4" strokeWidth={1.5} />
          None yet. A key is better than a secret: roll keeps nothing secret for it.
        </div>
      )}
      <ul className="divide-y divide-line">
        {live.map((c) => (
          <li key={c.client_id}>
            <div className="px-5 pt-3 font-mono text-[12px] text-subtle">{c.client_id}</div>
            <ul>
              <GrantRow g={c.grant} refresh={refresh} kindLabel={c.signs_with_key ? "signs with a key" : "signs with a secret"} />
            </ul>
          </li>
        ))}
      </ul>
    </Panel>
  );
}

function NewClient({ onMade, onCancel }: { onMade: (m: MadeMachineClient) => Promise<void>; onCancel: () => void }) {
  const [catalog, setCatalog] = useState<Catalog | null>(null);
  const [name, setName] = useState("");
  const [rules, setRules] = useState<Rule[]>([]);
  const [expiry, setExpiry] = useState(1);
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
  const problems = [
    ...(name.trim() ? [] : ["Name the client"]),
    ...(rules.length ? [] : ["Allow at least one action"]),
    ...(parsed && "problem" in parsed ? [parsed.problem] : []),
  ];
  const create = async () => {
    setBusy(true);
    setError(null);
    try {
      const ms = EXPIRIES[expiry].ms;
      const public_jwk = parsed && "jwk" in parsed ? parsed.jwk : null;
      await onMade(await accessApi.createMachineClient({ name: name.trim(), rules, expires_at: ms === null ? null : Date.now() + ms, public_jwk }));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
  if (!catalog) return error ? <div className="p-5"><ErrorNote error={error} /></div> : null;
  return (
    <div className="rise space-y-4 border-b border-line p-5">
      <Builder catalog={catalog} rules={rules} onChange={setRules} />
      <div className="grid gap-4 sm:grid-cols-2">
        <label className="block space-y-1.5">
          <span className="block text-[12px] text-subtle">Name</span>
          <input autoFocus value={name} onChange={(e) => setName(e.target.value)} placeholder="nightly report, deploy from CI" maxLength={64} className={`${input} w-full`} />
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
      </div>
      <label className="block space-y-1.5">
        <span className="block text-[12px] text-subtle">
          Public key (optional: an Ed25519 or P-256 PEM, or a JWK). Without one, roll makes a secret and shows it once.
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
          {busy ? <span className="shimmer">Creating…</span> : parsed && "jwk" in parsed ? "Create with this key" : "Create with a secret"}
        </button>
        <button onClick={onCancel} className={btn}>
          Cancel
        </button>
        {problems.length > 0 && <span className="text-[12px] text-subtle">{problems[0]}</span>}
      </div>
    </div>
  );
}

function Made({ made }: { made: MadeMachineClient }) {
  const { copied, copy } = useCopy();
  const secret = made.client_secret;
  const curl = secret
    ? `curl -s ${made.token_endpoint} -u "${made.client_id}:$ROLL_CLIENT_SECRET" -d grant_type=client_credentials`
    : `curl -s ${made.token_endpoint} -d grant_type=client_credentials \\\n  -d client_assertion_type=urn:ietf:params:oauth:client-assertion-type:jwt-bearer \\\n  -d client_assertion="$JWT"`;
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
    <div className="rise space-y-3 border-b border-line p-5">
      <p className="text-[13px] font-medium">{made.grant.name} is ready</p>
      <div className="grid gap-3 sm:grid-cols-2">
        {field("Client id", "client-id", made.client_id)}
        {field("Token endpoint", "token-endpoint", made.token_endpoint)}
      </div>
      {secret && field("Client secret: copy it now, it isn't shown again", "client-secret", secret)}
      <p className="text-[12px] text-muted">
        {secret
          ? "Trade it for a token with client_secret_basic (or client_secret_post):"
          : "Sign a JWT with its private key (iss and sub the client id, aud the token endpoint, a new jti, at most 5 minutes to live), then trade it:"}
      </p>
      <pre className="overflow-x-auto rounded-lg border border-line bg-bg px-3 py-2 font-mono text-[12px]">{curl}</pre>
    </div>
  );
}
