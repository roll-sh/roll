import { Check, Copy, Fingerprint, Plus } from "lucide-react";
import { useState } from "react";
import { ROLL_API } from "@/lib/roll-api";
import { ACTIONS, type Action, type Identity, type NewIdentity } from "@/lib/secrets-api";
import { useSecrets } from "./secrets-page";
import { ago, button, card, Chip, dangerButton, ErrorNote, errorText, field, ghost, label, primary, Section, useLoad } from "./ui";

const GITHUB = "https://token.actions.githubusercontent.com";
const GITLAB = "https://gitlab.com";
type Provider = "github" | "gitlab" | "other";

/** What an identity takes, in a few words: `acme/web · refs/heads/main`. */
function identitySummary(i: Pick<Identity, "issuer" | "subject" | "claims">): string {
  const repo = i.issuer === GITHUB ? /^repo:([^:]+):/.exec(i.subject)?.[1] : i.issuer === GITLAB ? /^project_path:([^:]+):/.exec(i.subject)?.[1] : undefined;
  const claims = Object.entries(i.claims).map(([k, v]) => (k === "ref" ? v : `${k}=${v}`));
  return [repo ?? i.subject, ...claims].join(" · ");
}

/** The workflow lines a GitHub Actions job needs to log in as `id`. */
function workflow(id: string, env: string): string {
  return ["permissions: { id-token: write, contents: read }", `env: { ROLL_API_URL: ${ROLL_API}, ROLL_IDENTITY: ${id} }`, "steps:", `  - run: roll run --env ${env} -- ./deploy.sh`].join("\n");
}

/**
 * Workload identities (docs/secrets.md, "Workload identities"): CI jobs and clusters log in with
 * their own OIDC token for a short-lived token, so no `rlst_` token sits in a CI system.
 */
export function Identities() {
  const { api, standard } = useSecrets();
  const identities = useLoad(() => api.identities(), [api]);
  const [creating, setCreating] = useState(false);
  const [made, setMade] = useState<Identity | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
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
      <p className="mb-2 text-[11.5px] text-muted">CI jobs and clusters log in with their own OIDC token (GitHub Actions, GitLab, Kubernetes) and get a token for minutes: nothing to store or rotate.</p>
      {made && (
        <div className="fade-in mb-3 rounded-xl border border-accent/40 bg-panel p-3 shadow-card inset-shadow-highlight" role="status">
          <p className="text-[12.5px]">
            <b className="font-medium">{made.name}</b> <span className="text-muted">is ready.{made.issuer === GITHUB ? " In the workflow:" : ` Set ROLL_IDENTITY=${made.id} and ROLL_OIDC_TOKEN (or ROLL_OIDC_TOKEN_FILE) in the workload.`}</span>
          </p>
          {made.issuer === GITHUB && (
            <div className="mt-2 flex items-start gap-2">
              <pre className="min-w-0 flex-1 overflow-x-auto rounded-md border border-line-2 bg-panel-2 px-2 py-1.5 font-mono text-[11.5px] leading-relaxed select-all">{workflow(made.id, made.environments?.[0] ?? "production")}</pre>
              <button
                className={button}
                onClick={() => {
                  void navigator.clipboard?.writeText(workflow(made.id, made.environments?.[0] ?? "production"));
                  setCopied(true);
                  setTimeout(() => setCopied(false), 1200);
                }}
              >
                {copied ? <Check className="size-3.5 text-ok" /> : <Copy className="size-3.5" />} Copy
              </button>
            </div>
          )}
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
                <span className="flex flex-wrap gap-1 text-[10.5px]">
                  {i.actions.map((a) => (
                    <span key={a} className="rounded border border-line-2 px-1 text-muted">
                      {a}
                    </span>
                  ))}
                </span>
                <span className="text-[11px] text-subtle">
                  {i.environments ? i.environments.join(", ") : "all environments"}
                  {i.paths ? ` · ${i.paths.join(", ")}` : ""} · {i.ttl_minutes} min
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
  const [busy, setBusy] = useState(false);
  const ipList = ips.split(/[\s,]+/).filter(Boolean);
  const minutes = Number(ttl);
  const repoOk = provider === "github" ? REPO.test(repo.trim()) : provider === "gitlab" ? GITLAB_PATH.test(repo.trim()) : true;
  // Pinned keys: the issuer is only compared with iss (a cluster's own needn't be reachable).
  const keysOk = jwks.trim() === "" || jwksHasKeys(jwks);
  const otherOk = provider !== "other" || ((jwks.trim() ? issuer.trim() !== "" : issuer.trim().startsWith("https://")) && keysOk && subject.trim() !== "" && subject.trim() !== "*");
  const ok = name.trim() !== "" && repoOk && otherOk && audience.trim() !== "" && picked.size > 0 && actions.size > 0 && Number.isInteger(minutes) && minutes >= 5 && minutes <= 60;
  const toggle = <T,>(set: Set<T>, v: T) => {
    const n = new Set(set);
    if (n.has(v)) n.delete(v);
    else n.add(v);
    return n;
  };
  const shape = (): Pick<NewIdentity, "issuer" | "subject" | "claims" | "jwks"> => {
    const ref: Record<string, string> = branch.trim() ? { ref: `refs/heads/${branch.trim()}` } : {};
    if (provider === "github") return { issuer: GITHUB, subject: `repo:${repo.trim()}:*`, claims: ref };
    if (provider === "gitlab") return { issuer: GITLAB, subject: `project_path:${repo.trim()}:*`, claims: branch.trim() ? { ref: branch.trim() } : {} };
    return { issuer: issuer.trim().replace(/\/+$/, ""), subject: subject.trim(), claims: {}, ...(jwks.trim() && { jwks: jwks.trim() }) };
  };
  return (
    <form
      className="rise mb-3 space-y-3 rounded-xl border border-line-2 bg-panel p-3 shadow-card inset-shadow-highlight"
      onSubmit={async (e) => {
        e.preventDefault();
        if (!ok) return;
        setBusy(true);
        await onCreate({ name: name.trim(), audience: audience.trim(), ...shape(), environments: [...picked], actions: ACTIONS.filter((a) => actions.has(a)), ttl_minutes: minutes, ...(ipList.length && { allowed_ips: ipList }) });
        setBusy(false);
      }}
      onKeyDown={(e) => e.key === "Escape" && onCancel()}
    >
      <div>
        <span className={label}>Who logs in</span>
        <div className="flex flex-wrap gap-1">
          <Chip on={provider === "github"} onClick={() => setProvider("github")}>
            GitHub Actions
          </Chip>
          <Chip on={provider === "gitlab"} onClick={() => setProvider("gitlab")}>
            GitLab CI
          </Chip>
          <Chip on={provider === "other"} onClick={() => setProvider("other")}>
            Other OIDC (Kubernetes…)
          </Chip>
        </div>
      </div>
      <div className="grid gap-3 sm:grid-cols-2">
        <label>
          <span className={label}>Name</span>
          <input autoFocus value={name} onChange={(e) => setName(e.target.value)} placeholder="deploy" className={field} />
        </label>
        {provider === "other" ? (
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
        {provider === "other" ? (
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
        <label>
          <span className={label}>Audience (aud)</span>
          <input value={audience} onChange={(e) => setAudience(e.target.value)} className={`${field} font-mono`} />
        </label>
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
              className={`${field} h-auto resize-y py-1.5 font-mono text-[11.5px]`}
            />
          </label>
        )}
      </div>
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
      <div className="flex flex-wrap items-end gap-3">
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
