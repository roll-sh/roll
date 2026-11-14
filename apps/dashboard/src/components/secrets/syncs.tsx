import { Check, Copy, Plus, RefreshCw, Send } from "lucide-react";
import { useState } from "react";
import { FOLDER_PATH, SECRET_KEY, type NewSync, type Sync, type SyncKind, type SyncPreview } from "@/lib/secrets-api";
import { useSecrets } from "./secrets-page";
import { ActorMark, ago, button, card, dangerButton, ErrorNote, errorText, field, ghost, label, primary, Section, Teach, useLoad } from "./ui";
import { OutsideDatabases } from "./outside-databases";
import { AwsRoles } from "./aws-roles";
import { Rotators } from "./rotators";

/** Syncs (docs/secrets.md, "Syncs"): one-way pushes of an environment's values to GitHub, a webhook or a deploy hook. */
export function SyncsView() {
  const { api } = useSecrets();
  const list = useLoad(() => api.listSyncs(), [api]);
  const [creating, setCreating] = useState(false);
  const [made, setMade] = useState<{ id: string; secret: string } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const syncs = list.data?.syncs ?? [];
  const kinds = list.data?.kinds ?? [];

  return (
    <div className="mx-auto max-w-4xl space-y-8 px-4 py-5">
      <Section
        title="Syncs"
        aside={
          kinds.length > 0 && (
            <button className={button} onClick={() => setCreating((c) => !c)}>
              <Plus className="size-3.5" /> New sync
            </button>
          )
        }
      >
        <p className="mb-2 text-[11.5px] text-muted">
          A sync pushes an environment&apos;s values to a platform outside roll whenever they change, one way. Values edited there are overwritten at the key&apos;s next change.
        </p>
        {made && <SigningSecret id={made.id} secret={made.secret} onDone={() => setMade(null)} />}
        {creating && (
          <NewSyncForm
            kinds={kinds}
            onCancel={() => setCreating(false)}
            onCreate={async (s) => {
              setError(null);
              try {
                const r = await api.createSync(s);
                if (r.signing_secret) setMade({ id: r.id, secret: r.signing_secret });
                setCreating(false);
                list.reload();
              } catch (e) {
                setError(errorText(e));
              }
            }}
          />
        )}
        <ErrorNote error={error ?? list.error} onDismiss={error ? () => setError(null) : undefined} className="mb-3" />
        {list.data && syncs.length === 0 && !creating ? (
          <Teach title="No syncs yet">
            {kinds.length === 0
              ? "This server has no sync kinds registered, so nothing can be synced from here."
              : `Push an environment's values to ${kinds.map((k) => k.label).join(", ")}, every time they change.`}
          </Teach>
        ) : (
          syncs.length > 0 && (
            <div className={`${card} divide-y divide-line`}>
              {syncs.map((s) => (
                <SyncRow key={s.id} sync={s} kind={kinds.find((k) => k.kind === s.kind)} onChanged={list.reload} onError={setError} />
              ))}
            </div>
          )
        )}
      </Section>
      <OutsideDatabases />
      <Rotators />
      <AwsRoles />
    </div>
  );
}

/** The kind picker's groups, in order; `[]`: every kind no group names. */
const GROUPS: [string, string[]][] = [
  ["CI", ["github", "gitlab", "circleci", "bitbucket"]],
  ["Hosting", ["vercel", "netlify", "render", "fly", "railway", "heroku", "digitalocean", "cloudflare"]],
  ["Clouds and clusters", ["aws-ssm", "aws-secrets-manager", "google-secret-manager", "azure-key-vault", "kubernetes", "terraform-cloud"]],
  ["Anything else", []],
];

/** What a sync's target is, in a few words: `owner/app · production`, or a URL's host. */
function targetSummary(s: Sync): string {
  const t = s.target;
  const str = (k: string) => (typeof t[k] === "string" ? (t[k] as string) : "");
  if (str("repo")) return str("environment") ? `${str("repo")} · ${str("environment")}` : str("repo");
  if (s.kind === "kubernetes") return `${str("namespace")}/${str("secret")}`;
  if (s.kind === "gitlab") return `${str("project")} · ${str("environment_scope") || "*"}`;
  if (s.kind === "render") return str("service");
  if (s.kind === "netlify") return `${str("site")} · ${str("context") || "all"}`;
  if (s.kind === "fly") return str("app");
  if (s.kind === "railway") return `${str("project")} · ${str("environment")}${str("service") ? ` · ${str("service")}` : ""}`;
  if (s.kind === "heroku") return str("app");
  if (s.kind === "digitalocean") return str("component") ? `${str("app")} · ${str("component")}` : str("app");
  if (s.kind === "circleci") return str("project");
  if (s.kind === "bitbucket") return `${str("workspace")}/${str("repo")}${str("environment") ? " · deployment" : ""}`;
  if (s.kind === "aws-ssm") return `${str("region")} · ${str("prefix")}`;
  if (s.kind === "aws-secrets-manager") return `${str("region")} · ${str("secret_name")}`;
  if (s.kind === "google-secret-manager") return `${str("project")}${str("prefix") ? ` · ${str("prefix")}…` : ""}`;
  if (s.kind === "azure-key-vault") return str("vault");
  if (s.kind === "terraform-cloud") return `${str("workspace_id")} · ${str("category") || "env"}`;
  if (str("url")) {
    try {
      return new URL(str("url")).host;
    } catch {
      return str("url");
    }
  }
  return Object.values(t)
    .filter((v) => typeof v === "string")
    .join(" · ");
}

const STATUS: Record<Sync["status"], { text: string; tone: string }> = {
  ok: { text: "synced", tone: "text-ok" },
  pending: { text: "pending", tone: "text-muted" },
  error: { text: "error", tone: "text-danger" },
};

function SyncRow({ sync: s, kind, onChanged, onError }: { sync: Sync; kind?: SyncKind; onChanged: () => void; onError: (e: string) => void }) {
  const { api } = useSecrets();
  const [running, setRunning] = useState(false);
  const [sure, setSure] = useState(false);
  const status = STATUS[s.status] ?? STATUS.pending;
  const run = async () => {
    setRunning(true);
    try {
      await api.runSync(s.id);
    } catch (e) {
      onError(errorText(e));
    } finally {
      setRunning(false);
      onChanged();
    }
  };
  return (
    <div className="px-3 py-2">
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
        <Send className="size-3.5 text-muted" />
        <span className="min-w-0 leading-tight">
          <span className="block truncate text-[12.5px] font-medium">{targetSummary(s)}</span>
          <span className="block text-[10.5px] text-subtle">{kind?.label ?? s.kind}</span>
        </span>
        <span className="font-mono text-[11px] text-subtle">
          {s.environment}
          {s.path === "/" ? "" : `:${s.path}`}
        </span>
        {(s.keys || s.prefix) && (
          <span className="text-[11px] text-subtle">
            {s.keys ? `${s.keys.length} ${s.keys.length === 1 ? "key" : "keys"}` : "all keys"}
            {s.prefix ? <span className="font-mono"> · {s.prefix}*</span> : null}
          </span>
        )}
        <span className="ml-auto flex items-center gap-2 text-right text-[11px] text-subtle">
          <span className={running ? "shimmer" : status.tone}>{running ? "pushing…" : status.text}</span>
          <span>{s.last_synced_at ? `${ago(s.last_synced_at)} · ${s.synced} at the target` : "never synced"}</span>
        </span>
        <button className={ghost} onClick={run} disabled={running} title="Push every key now">
          <RefreshCw className={`size-3.5 ${running ? "animate-spin motion-reduce:animate-none" : ""}`} /> Run now
        </button>
        {sure ? (
          <span className="flex gap-1">
            <button className={dangerButton} onClick={() => api.deleteSync(s.id).then(onChanged, (e) => onError(errorText(e)))}>
              Delete
            </button>
            <button className={ghost} onClick={() => setSure(false)}>
              Keep
            </button>
          </span>
        ) : (
          <button className={ghost} onClick={() => setSure(true)} title="Delete the sync; what it pushed stays at the target">
            Delete
          </button>
        )}
      </div>
      {s.status === "error" && s.error && <p className="mt-1 pl-6 text-[11.5px] break-words text-danger">{s.error}</p>}
      <p className="mt-0.5 pl-6 text-[10.5px] text-faint">
        <ActorMark who={s.created_by} /> · created {ago(s.created_at)}
      </p>
    </div>
  );
}

function SigningSecret({ id, secret, onDone }: { id: string; secret: string; onDone: () => void }) {
  const [copied, setCopied] = useState(false);
  return (
    <div className="fade-in mb-3 rounded-xl border border-accent/40 bg-panel p-3 shadow-card inset-shadow-highlight" role="status">
      <p className="text-[12.5px]">
        <b className="font-mono font-medium">{id}</b> <span className="text-muted">signs its requests with this secret. Copy it now: it isn&apos;t shown again.</span>
      </p>
      <div className="mt-2 flex items-center gap-2">
        <code className="min-w-0 flex-1 truncate rounded-md border border-line-2 bg-panel-2 px-2 py-1.5 font-mono text-[12px] select-all">{secret}</code>
        <button
          className={button}
          onClick={() => {
            void navigator.clipboard?.writeText(secret);
            setCopied(true);
            setTimeout(() => setCopied(false), 1200);
          }}
        >
          {copied ? <Check className="size-3.5 text-ok" /> : <Copy className="size-3.5" />} Copy
        </button>
        <button className={ghost} onClick={onDone}>
          Done
        </button>
      </div>
      <p className="mt-2 font-mono text-[11px] text-subtle">X-Roll-Signature-256: sha256=HMAC(secret, &quot;&lt;X-Roll-Timestamp&gt;.&lt;body&gt;&quot;)</p>
    </div>
  );
}

function NewSyncForm({ kinds, onCreate, onCancel }: { kinds: SyncKind[]; onCreate: (s: NewSync) => Promise<void>; onCancel: () => void }) {
  const { standard, previews, api } = useSecrets();
  const envs = [...standard, ...previews];
  const [kindName, setKindName] = useState(kinds[0]?.kind ?? "");
  const [target, setTarget] = useState<Record<string, string>>({});
  const [env, setEnv] = useState(standard.find((e) => e.slug === "production")?.slug ?? envs[0]?.slug ?? "");
  const [path, setPath] = useState("/");
  const [keys, setKeys] = useState("");
  const [prefix, setPrefix] = useState("");
  const [busy, setBusy] = useState(false);
  const [preview, setPreview] = useState<SyncPreview | string | null>(null);
  const kind = kinds.find((k) => k.kind === kindName);
  const keyList = keys.split(/[\s,]+/).filter(Boolean);
  const badKeys = keyList.filter((k) => !SECRET_KEY.test(k));
  const prefixOk = prefix === "" || /^[A-Za-z0-9_]{1,64}$/.test(prefix);
  const ok = !!kind && !!env && FOLDER_PATH.test(path) && badKeys.length === 0 && prefixOk && kind.fields.every((f) => !f.required || (target[f.name] ?? "").trim() !== "");

  const draft = (): NewSync => {
    const t: Record<string, string> = {};
    for (const f of kind?.fields ?? []) {
      const v = (target[f.name] ?? "").trim();
      if (v) t[f.name] = v;
    }
    return { environment: env, path, kind: kindName, target: t, ...(keyList.length && { keys: keyList }), ...(prefix && { prefix }) };
  };
  return (
    <form
      className="rise mb-3 space-y-3 rounded-xl border border-line-2 bg-panel p-3 shadow-card inset-shadow-highlight"
      onSubmit={async (e) => {
        e.preventDefault();
        if (!ok || !kind) return;
        setBusy(true);
        await onCreate(draft());
        setBusy(false);
      }}
      onKeyDown={(e) => e.key === "Escape" && onCancel()}
    >
      <div className="grid gap-3 sm:grid-cols-3">
        <label>
          <span className={label}>Kind</span>
          <select
            value={kindName}
            onChange={(e) => {
              setKindName(e.target.value);
              setTarget({});
            }}
            className={field}
          >
            {GROUPS.map(([group, members]) => {
              const these = kinds.filter((k) => (members.length ? members.includes(k.kind) : !GROUPS.some(([, m]) => m.includes(k.kind))));
              return (
                these.length > 0 && (
                  <optgroup key={group} label={group}>
                    {these.map((k) => (
                      <option key={k.kind} value={k.kind}>
                        {k.label}
                      </option>
                    ))}
                  </optgroup>
                )
              );
            })}
          </select>
        </label>
        <label>
          <span className={label}>Environment</span>
          <select value={env} onChange={(e) => setEnv(e.target.value)} className={field}>
            {envs.map((e) => (
              <option key={e.slug} value={e.slug}>
                {e.name} ({e.slug})
              </option>
            ))}
          </select>
        </label>
        <label>
          <span className={label}>Folder</span>
          <input value={path} onChange={(e) => setPath(e.target.value)} className={`${field} font-mono`} aria-invalid={!FOLDER_PATH.test(path)} />
        </label>
      </div>
      {kind && kind.fields.length > 0 && (
        <div className="grid gap-3 sm:grid-cols-2">
          {kind.fields.map((f, i) => (
            <label key={f.name}>
              <span className={label}>
                {f.label}
                {!f.required && " (optional)"}
              </span>
              <input autoFocus={i === 0} value={target[f.name] ?? ""} onChange={(e) => setTarget((t) => ({ ...t, [f.name]: e.target.value }))} placeholder={f.placeholder} className={`${field} font-mono`} />
            </label>
          ))}
        </div>
      )}
      {kind && !kind.values && <p className="text-[11.5px] text-muted">This kind sends no values: only that the environment changed, and which keys.</p>}
      <div className="grid gap-3 sm:grid-cols-[minmax(0,2fr)_minmax(0,1fr)]">
        <label>
          <span className={label}>Keys (empty for all)</span>
          <input value={keys} onChange={(e) => setKeys(e.target.value)} placeholder="DATABASE_URL, STRIPE_KEY" className={`${field} font-mono`} aria-invalid={badKeys.length > 0} />
        </label>
        <label>
          <span className={label}>Prefix at the target (optional)</span>
          <input value={prefix} onChange={(e) => setPrefix(e.target.value)} placeholder="APP_" className={`${field} font-mono`} aria-invalid={!prefixOk} />
        </label>
      </div>
      {badKeys.length > 0 && <p className="text-[11.5px] text-danger">Not a key: {badKeys.join(", ")}</p>}
      {preview && (
        <div className="rounded-lg border border-line-2 bg-panel-2 px-3 py-2 text-[12px]" role="status">
          {typeof preview === "string" ? (
            <p className="text-danger">{preview}</p>
          ) : (
            <>
              <p className="text-muted">
                It would push {preview.names.length} {preview.names.length === 1 ? "name" : "names"}
                {preview.credentials.length > 0 && <> (never its credential {preview.credentials.join(", ")})</>}:
              </p>
              <p className="mt-1 font-mono text-[11.5px] text-fg">{preview.names.join(", ") || "nothing"}</p>
              {preview.missing_credentials.length > 0 && <p className="mt-1 text-danger">Not set here: {preview.missing_credentials.join(", ")}, so the first push would fail.</p>}
              {preview.unknown_keys.length > 0 && <p className="mt-1 text-danger">No such key here: {preview.unknown_keys.join(", ")}.</p>}
            </>
          )}
        </div>
      )}
      <div className="flex justify-end gap-2">
        <button type="button" className={ghost} disabled={!ok} onClick={() => api.previewSync(draft()).then(setPreview, (e) => setPreview(errorText(e)))} title="What it would push (names only), before it exists">
          Preview
        </button>
        <button type="button" className={ghost} onClick={onCancel}>
          Cancel
        </button>
        <button type="submit" className={primary} disabled={!ok || busy}>
          {busy ? "Creating…" : "Create sync"}
        </button>
      </div>
    </form>
  );
}
