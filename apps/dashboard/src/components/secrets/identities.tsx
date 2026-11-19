import { Check, Copy, Fingerprint, Plus } from "lucide-react";
import { useState } from "react";
import { useInstallLine } from "@/lib/install-line";
import { ROLL_API } from "@/lib/roll-api";
import { AZURE, AZURE_AUDIENCE, ciSnippets, GITHUB, GITLAB, type CiSnippet } from "@/lib/secrets-ci";
import { ACTIONS, type Action, type Identity, type NewIdentity } from "@/lib/secrets-api";
import { GatewayPicker } from "./gateways";
import { useSecrets } from "./secrets-page";
import { ago, button, card, Chip, dangerButton, ErrorNote, errorText, field, ghost, label, primary, Section, textareaField, useLoad } from "./ui";

type Provider = "github" | "gitlab" | "azure" | "aws" | "other";
/** One role or user in one account; a `*` only ends a session name (the server checks it too). */
const AWS_ARN = /^arn:aws(-cn|-us-gov)?:(iam::\d{12}:(role|user)\/([\w+=,.@-]+\/)*[\w+=,.@-]+|sts::\d{12}:assumed-role\/[\w+=,.@-]+\/[\w+=,.@-]*\*?)$/;

/** What an identity takes, in a few words: `acme/web · refs/heads/main`. */
function identitySummary(i: Pick<Identity, "issuer" | "subject" | "claims">): string {
  const repo = i.issuer === GITHUB ? /^repo:([^:]+):/.exec(i.subject)?.[1] : i.issuer === GITLAB ? /^project_path:([^:]+):/.exec(i.subject)?.[1] : i.issuer.startsWith(AZURE) ? i.subject.replace(/^p:\/\//, "") : undefined;
  const claims = Object.entries(i.claims).map(([k, v]) => (k === "ref" ? v : `${k}=${v}`));
  return [repo ?? i.subject, ...claims].join(" · ");
}

/** One CI snippet with its copy button. */
function Snippet({ snippet }: { snippet: CiSnippet }) {
  const [copied, setCopied] = useState(false);
  return (
    <div className="mt-2">
      <span className="mb-1 block text-[11px] text-muted">{snippet.title}</span>
      <div className="flex items-start gap-2">
        <pre className="min-w-0 flex-1 overflow-x-auto rounded-md border border-line-2 bg-panel-2 px-2 py-1.5 font-mono text-[11.5px] leading-relaxed select-all">{snippet.text}</pre>
        <button
          className={button}
          onClick={() => {
            void navigator.clipboard?.writeText(snippet.text);
            setCopied(true);
            setTimeout(() => setCopied(false), 1200);
          }}
        >
          {copied ? <Check className="size-3.5 text-ok" /> : <Copy className="size-3.5" />} Copy
        </button>
      </div>
    </div>
  );
}

/** An AWS machine's environment to log in as `i` (CI's are `ciSnippets`). */
function awsSetup(i: Identity, env: string): CiSnippet[] {
  if (i.kind !== "aws") return [];
  // Signed requests name the server; only worth setting when the API's URL isn't that name.
  const named = i.server_id && i.server_id !== ROLL_API.replace(/\/+$/, "") ? [`ROLL_AWS_SERVER_ID=${i.server_id}`] : [];
  return [{ title: "The machine's environment", text: [`ROLL_API_URL=${ROLL_API}`, `ROLL_IDENTITY=${i.id}`, ...named, `roll run --env ${env} -- ./server`].join("\n") }];
}

/** The sentence under a new identity: where it goes. */
function readyText(i: Identity): string {
  if (i.gateway) return ` Inside the network: roll secrets gateways up --gateway ${i.gateway} --identity ${i.id} --oidc-token-file FILE --allow HOST:PORT`;
  if (i.issuer === GITHUB) return " In the workflow:";
  if (i.issuer === GITLAB) return " In the job:";
  if (i.issuer.startsWith(AZURE)) return " In the pipeline:";
  if (i.kind === "aws") return " On the machine (its instance profile, task role, IRSA or Pod Identity signs the login):";
  return ` Set ROLL_IDENTITY=${i.id} and ROLL_OIDC_TOKEN (or ROLL_OIDC_TOKEN_FILE) in the workload.`;
}

/**
 * Workload identities (docs/secrets.md, "Workload identities"): CI jobs and clusters log in with
 * their own OIDC token for a short-lived roll token, so no long-lived token sits in a CI system.
 */
export function Identities() {
  const { api, standard } = useSecrets();
  const identities = useLoad(() => api.identities(), [api]);
  const [creating, setCreating] = useState(false);
  const [made, setMade] = useState<Identity | null>(null);
  const [error, setError] = useState<string | null>(null);
  const install = useInstallLine();
  const env = made?.environments?.[0] ?? "production";
  const snippets = made ? [...ciSnippets(made, { api: ROLL_API, env, install }), ...awsSetup(made, env)] : [];
  const rows = identities.data ?? [];

  return (
    <Section
      title="Workload identities"
      aside={
        <button className={button} onClick={() => setCreating((c) => !c)}>
          <Plus className="size-3.5" /> New identity
        </button>
      }
    >
      <p className="mb-2 text-[11.5px] text-muted">CI jobs, clusters and machines on AWS log in with their own OIDC token (GitHub Actions, GitLab, Azure Pipelines, Kubernetes) or AWS role, and get a token for minutes: nothing to store or rotate.</p>
      {made && (
        <div className="fade-in mb-3 rounded-xl border border-accent/40 bg-panel p-3 shadow-card inset-shadow-highlight" role="status">
          <p className="text-[12.5px]">
            <b className="font-medium">{made.name}</b>{" "}
            <span className="text-muted">
              is ready.
              {readyText(made)}
            </span>
          </p>
          {!made.gateway &&
            snippets.map((snippet) => (
              <Snippet key={snippet.title} snippet={snippet} />
            ))}
          <div className="mt-2 flex justify-end">
            <button className={ghost} onClick={() => setMade(null)}>
              Done
            </button>
          </div>
        </div>
      )}
      {creating && (
        <NewIdentityForm
          envs={standard.map((e) => e.slug)}
          onCancel={() => setCreating(false)}
          onCreate={async (n) => {
            setError(null);
            try {
              setMade(await api.createIdentity(n));
              setCreating(false);
              identities.reload();
            } catch (e) {
              setError(errorText(e));
            }
          }}
        />
      )}
      <ErrorNote error={error ?? identities.error} onDismiss={error ? () => setError(null) : undefined} className="mb-2" />
      {identities.data && rows.length === 0 && !creating ? (
        <p className="rounded-xl border border-dashed border-line-2 px-3 py-4 text-[12px] text-muted">No identities yet. Add one for a repo's GitHub Actions to deploy without a stored token.</p>
      ) : (
        rows.length > 0 && (
          <div className={`${card} divide-y divide-line`}>
            {rows.map((i) => (
              <div key={i.id} className="flex flex-wrap items-center gap-x-3 gap-y-1 px-3 py-2">
                <Fingerprint className="size-3.5 text-muted" />
                <span className="min-w-0 leading-tight">
                  <span className="block text-[12.5px] font-medium">{i.name}</span>
                  <span className="block truncate font-mono text-[10.5px] text-subtle" title={`${i.issuer}\n${i.subject}`}>
                    {identitySummary(i)}
                  </span>
                </span>
                {i.gateway ? (
                  <span className="font-mono text-[11px] text-muted">logs in gateway {i.gateway}</span>
                ) : (
                  <span className="flex flex-wrap gap-1 text-[10.5px]">
                    {i.actions.map((a) => (
                      <span key={a} className="rounded border border-line-2 px-1 text-muted">
                        {a}
                      </span>
                    ))}
                  </span>
                )}
                <span className="text-[11px] text-subtle">
                  {i.gateway ? "" : `${i.environments ? i.environments.join(", ") : "all environments"}${i.paths ? ` · ${i.paths.join(", ")}` : ""} · `}
                  {i.ttl_minutes} min
                  {i.allowed_ips ? ` · from ${i.allowed_ips.join(", ")}` : ""}
                  {i.pinned_keys ? ` · ${i.pinned_keys} pinned ${i.pinned_keys === 1 ? "key" : "keys"}` : ""}
                </span>
                <span className="ml-auto truncate text-right font-mono text-[10.5px] text-subtle" title={i.last_subject ?? undefined}>
                  {i.last_used_at ? `logged in ${ago(i.last_used_at)}` : "never used"} · {i.id}
                </span>
                <Delete onDelete={() => api.deleteIdentity(i.id).then(identities.reload, (e) => setError(errorText(e)))} />
              </div>
            ))}
          </div>
        )
      )}
    </Section>
  );
}

function Delete({ onDelete }: { onDelete: () => void }) {
  const [sure, setSure] = useState(false);
  return sure ? (
    <span className="flex gap-1">
      <button className={dangerButton} onClick={onDelete} title="Its live tokens stop working at once">
        Delete
      </button>
      <button className={ghost} onClick={() => setSure(false)}>
        Keep
      </button>
    </span>
  ) : (
    <button className={ghost} onClick={() => setSure(true)}>
      Delete
    </button>
  );
}

const REPO = /^[\w.-]+\/[\w.-]+$/;

/** A JWKS with at least one key, as text (the server checks which it can use). */
function jwksHasKeys(text: string): boolean {
  try {
    const keys = (JSON.parse(text) as { keys?: unknown }).keys;
    return Array.isArray(keys) && keys.length > 0;
  } catch {
    return false;
  }
}
const GITLAB_PATH = /^[\w.-]+(\/[\w.-]+)+$/;
/** An Azure DevOps organization's id (a GUID), and one pipeline: `organization/project/pipeline`. */
const GUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const PIPELINE = /^[^/*\s][^/*]*\/[^/*]+\/[^/*]+$/;

function NewIdentityForm({ envs, onCreate, onCancel }: { envs: string[]; onCreate: (n: NewIdentity) => Promise<void>; onCancel: () => void }) {
  const [provider, setProvider] = useState<Provider>("github");
  const [name, setName] = useState("");
  const [repo, setRepo] = useState("");
  const [branch, setBranch] = useState("main");
  const [issuer, setIssuer] = useState("");
  const [subject, setSubject] = useState("");
  // This server's URL: a token minted for one roll server isn't good at another.
  const [audience, setAudience] = useState(ROLL_API);
  const [picked, setPicked] = useState<Set<string>>(new Set(envs.includes("production") ? ["production"] : envs.slice(0, 1)));
  const [actions, setActions] = useState<Set<Action>>(new Set(["read", "reveal"]));
  const [ttl, setTtl] = useState("15");
  const [ips, setIps] = useState("");
  const [jwks, setJwks] = useState("");
  const [arn, setArn] = useState("");
  // A gateway it logs in instead of reading secrets: its logins get that gateway's own rule, so
  // environments and actions don't apply (the server refuses them).
  const [gateway, setGateway] = useState("");
  const [busy, setBusy] = useState(false);
  const ipList = ips.split(/[\s,]+/).filter(Boolean);
  const minutes = Number(ttl);
  const repoOk = provider === "github" ? REPO.test(repo.trim()) : provider === "gitlab" ? GITLAB_PATH.test(repo.trim()) : provider === "azure" ? GUID.test(issuer.trim()) && PIPELINE.test(repo.trim()) : true;
  // Azure's tokens always carry Azure's audience; the others this server's.
  const pick = (p: Provider) => {
    setProvider(p);
    if (p === "azure") setAudience(AZURE_AUDIENCE);
    else if (audience === AZURE_AUDIENCE) setAudience(ROLL_API);
  };
  // Pinned keys: the issuer is only compared with iss (a cluster's own needn't be reachable).
  const keysOk = jwks.trim() === "" || jwksHasKeys(jwks);
  const otherOk = provider !== "other" || ((jwks.trim() ? issuer.trim() !== "" : issuer.trim().startsWith("https://")) && keysOk && subject.trim() !== "" && subject.trim() !== "*");
  const arnOk = provider !== "aws" || AWS_ARN.test(arn.trim());
  const ok = name.trim() !== "" && repoOk && otherOk && arnOk && (provider === "aws" || audience.trim() !== "") && (gateway !== "" || (picked.size > 0 && actions.size > 0)) && Number.isInteger(minutes) && minutes >= 5 && minutes <= 60;
  const toggle = <T,>(set: Set<T>, v: T) => {
    const n = new Set(set);
    if (n.has(v)) n.delete(v);
    else n.add(v);
    return n;
  };
  const shape = (): { kind: "aws"; arn: string } | { issuer: string; audience: string; subject: string; claims: Record<string, string>; jwks?: string } => {
    if (provider === "aws") return { kind: "aws", arn: arn.trim() };
    const ref: Record<string, string> = branch.trim() ? { ref: `refs/heads/${branch.trim()}` } : {};
    const aud = audience.trim();
    if (provider === "github") return { issuer: GITHUB, audience: aud, subject: `repo:${repo.trim()}:*`, claims: ref };
    if (provider === "gitlab") return { issuer: GITLAB, audience: aud, subject: `project_path:${repo.trim()}:*`, claims: branch.trim() ? { ref: branch.trim() } : {} };
    if (provider === "azure") return { issuer: `${AZURE}${issuer.trim().toLowerCase()}`, audience: aud, subject: `p://${repo.trim()}`, claims: {} };
    return { issuer: issuer.trim().replace(/\/+$/, ""), audience: aud, subject: subject.trim(), claims: {}, ...(jwks.trim() && { jwks: jwks.trim() }) };
  };
  return (
    <form
      className="rise mb-3 space-y-3 rounded-xl border border-line-2 bg-panel p-3 shadow-card inset-shadow-highlight"
      onSubmit={async (e) => {
        e.preventDefault();
        if (!ok) return;
        setBusy(true);
        const scope: Pick<NewIdentity, "environments" | "actions" | "gateway"> = gateway ? { actions: [], gateway } : { environments: [...picked], actions: ACTIONS.filter((a) => actions.has(a)) };
        await onCreate({ name: name.trim(), ...shape(), ...scope, ttl_minutes: minutes, ...(ipList.length && { allowed_ips: ipList }) });
        setBusy(false);
      }}
      onKeyDown={(e) => e.key === "Escape" && onCancel()}
    >
      <div>
        <span className={label}>Who logs in</span>
        <div className="flex flex-wrap gap-1">
          <Chip on={provider === "github"} onClick={() => pick("github")}>
            GitHub Actions
          </Chip>
          <Chip on={provider === "gitlab"} onClick={() => pick("gitlab")}>
            GitLab CI
          </Chip>
          <Chip on={provider === "azure"} onClick={() => pick("azure")}>
            Azure Pipelines
          </Chip>
          <Chip on={provider === "aws"} onClick={() => pick("aws")}>
            AWS
          </Chip>
          <Chip on={provider === "other"} onClick={() => pick("other")}>
            Other OIDC (Kubernetes…)
          </Chip>
        </div>
      </div>
      <div className="grid gap-3 sm:grid-cols-2">
        <label>
          <span className={label}>Name</span>
          <input autoFocus value={name} onChange={(e) => setName(e.target.value)} placeholder="deploy" className={field} />
        </label>
        {provider === "aws" ? (
          <label>
            <span className={label}>Role or user ARN</span>
            <input value={arn} onChange={(e) => setArn(e.target.value)} placeholder="arn:aws:iam::123456789012:role/web" className={`${field} font-mono`} aria-invalid={arn.trim() !== "" && !arnOk} />
          </label>
        ) : provider === "azure" ? (
          <>
            <label>
              <span className={label}>Organization id ($(System.CollectionId))</span>
              <input value={issuer} onChange={(e) => setIssuer(e.target.value)} placeholder="00000000-0000-0000-0000-000000000000" className={`${field} font-mono`} aria-invalid={issuer.trim() !== "" && !GUID.test(issuer.trim())} />
            </label>
            <label>
              <span className={label}>Pipeline (organization/project/pipeline)</span>
              <input value={repo} onChange={(e) => setRepo(e.target.value)} placeholder="acme/shop/deploy" className={`${field} font-mono`} aria-invalid={repo.trim() !== "" && !PIPELINE.test(repo.trim())} />
            </label>
          </>
        ) : provider === "other" ? (
          <label>
            <span className={label}>Issuer (the token's iss)</span>
            <input value={issuer} onChange={(e) => setIssuer(e.target.value)} placeholder="https://oidc.eks.eu-west-1.amazonaws.com/id/…" className={`${field} font-mono`} />
          </label>
        ) : (
          <label>
            <span className={label}>{provider === "github" ? "Repo (owner/name)" : "Project (group/name)"}</span>
            <input value={repo} onChange={(e) => setRepo(e.target.value)} placeholder={provider === "github" ? "acme/web" : "acme/platform/web"} className={`${field} font-mono`} aria-invalid={repo.trim() !== "" && !repoOk} />
          </label>
        )}
        {provider === "azure" || provider === "aws" ? null : provider === "other" ? (
          <label>
            <span className={label}>Subject (sub; * for any run)</span>
            <input value={subject} onChange={(e) => setSubject(e.target.value)} placeholder="system:serviceaccount:prod:deployer" className={`${field} font-mono`} />
          </label>
        ) : (
          <label>
            <span className={label}>Branch (empty for any)</span>
            <input value={branch} onChange={(e) => setBranch(e.target.value)} placeholder="main" className={`${field} font-mono`} />
          </label>
        )}
        {provider !== "aws" && (
          <label>
            <span className={label}>Audience (aud)</span>
            <input value={audience} onChange={(e) => setAudience(e.target.value)} className={`${field} font-mono`} />
          </label>
        )}
        {provider === "aws" && (
          <p className="text-[11.5px] leading-snug text-muted sm:col-span-2">
            Any session of the role logs in (an EC2 instance profile, an ECS task role, IRSA, Pod Identity). For some sessions only, use <span className="font-mono">arn:aws:sts::…:assumed-role/web/i-*</span>. The machine signs a GetCallerIdentity with its role, roll checks it with STS: nothing to store.
          </p>
        )}
        {provider === "other" && (
          <label className="sm:col-span-2">
            <span className={label}>Issuer&apos;s keys, for a cluster roll can&apos;t reach (optional; kubectl get --raw /openid/v1/jwks)</span>
            <textarea
              value={jwks}
              onChange={(e) => setJwks(e.target.value)}
              rows={3}
              spellCheck={false}
              placeholder='{"keys":[{"kty":"RSA","kid":"…","n":"…","e":"AQAB"}]}'
              aria-invalid={!keysOk}
              className={`${textareaField} resize-y font-mono text-[11.5px]`}
            />
          </label>
        )}
      </div>
      <div className="max-w-sm">
        <GatewayPicker value={gateway} onChange={setGateway} title="A login gets" none="secrets (the environments and actions below)" />
      </div>
      {!gateway && (
        <div>
          <span className={label}>Environments</span>
          <div className="flex flex-wrap gap-1">
            {envs.map((e) => (
              <Chip key={e} on={picked.has(e)} onClick={() => setPicked((p) => toggle(p, e))}>
                <span className="font-mono">{e}</span>
              </Chip>
            ))}
          </div>
        </div>
      )}
      <div className="flex flex-wrap items-end gap-3">
        {!gateway && (
          <div>
            <span className={label}>Actions</span>
            <div className="flex flex-wrap gap-1">
              {ACTIONS.map((a) => (
                <Chip key={a} on={actions.has(a)} onClick={() => setActions((s) => toggle(s, a))}>
                  {a}
                </Chip>
              ))}
            </div>
          </div>
        )}
        <label className="w-28">
          <span className={label}>Minutes (5–60)</span>
          <input value={ttl} onChange={(e) => setTtl(e.target.value)} inputMode="numeric" className={`${field} font-mono`} />
        </label>
      </div>
      <label className="block">
        <span className={label}>Allowed addresses (CIDRs; empty for any)</span>
        <input value={ips} onChange={(e) => setIps(e.target.value)} placeholder="the runners' egress ranges, e.g. 203.0.113.0/24" className={`${field} font-mono`} />
      </label>
      <div className="flex justify-end gap-2">
        <button type="button" className={ghost} onClick={onCancel}>
          Cancel
        </button>
        <button type="submit" className={primary} disabled={!ok || busy}>
          {busy ? "Creating…" : "Create identity"}
        </button>
      </div>
    </form>
  );
}
