"use client";

import { Eye, EyeOff, KeyRound, Plus, RefreshCw, Trash2 } from "lucide-react";
import Link from "next/link";
import { useEffect, useState } from "react";
import {
  agoMs,
  apiOidcError,
  DEV_API_CLIENTS,
  devApiSnippet,
  formatDate,
  jwtSecretError,
  maskSecret,
  storageApi,
  type ApiAuthPatch,
  type ApiOidcProvider,
  type Bucket,
  type DevApiClient,
  type IamPolicy,
  type StorageApiView,
} from "@/lib/storage-api";
import { errorMessage, isOperatorOnly, StorageError } from "@/lib/storage-session";
import { AppShell } from "../app-shell";
import { iconBtn } from "../tasks/ui";
import { btn, Confirm, Connecting, CopyButton, ErrorNote, input, Offline, Panel, SecretField, useCopy } from "../ui";
import { OperatorOnlyNote, StorageIdentity, useConsoleSession, useScope } from "./console";
import { Field, InlineConfirm, SaveBar, useAction } from "./shared";

const noop = async () => {};
const MAX_OIDC = 10;

/**
 * The developer API (a Supabase-Storage-compatible REST API) of the account the dashboard acts in:
 * its endpoint, anon and service keys, the auth providers whose user tokens it accepts, and rule
 * presets (bucket policies evaluated with the token's claims).
 */
export function StorageDevApi() {
  const session = useConsoleSession();
  const scope = useScope();
  const [view, setView] = useState<StorageApiView | null>(null);
  const [loadErr, setLoadErr] = useState<unknown>(null);

  useEffect(() => {
    let alive = true;
    setView(null);
    setLoadErr(null);
    storageApi.devApi().then(
      (v) => alive && setView(v),
      (e) => alive && setLoadErr(e),
    );
    return () => {
      alive = false;
    };
  }, [scope]);

  return (
    <AppShell
      crumb={
        <>
          <Link href="/dashboard/storage" className="text-muted hover:text-fg">
            storage
          </Link>{" "}
          <span className="text-subtle">/</span> API
        </>
      }
    >
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <h1 className="text-2xl font-semibold tracking-[-0.04em]">Developer API</h1>
          <p className="mt-1 max-w-2xl text-[13px] text-muted">
            A Supabase Storage compatible REST API for apps: browsers call it with the anon key and your users&apos; JWTs, servers
            with the service key. Rules decide what each user may do.
          </p>
        </div>
        <div className="flex flex-col items-start gap-1 sm:items-end">
          <StorageIdentity />
          {view && <p className="font-mono text-[11px] text-subtle">account {view.account_id}</p>}
        </div>
      </div>

      {loadErr ? (
        session && isOperatorOnly(loadErr) ? (
          <OperatorOnlyNote what="The developer API" />
        ) : loadErr instanceof StorageError ? (
          <ErrorNote error={errorMessage(loadErr)} />
        ) : (
          <Offline />
        )
      ) : !view ? (
        <Connecting />
      ) : (
        <div className="mt-6 space-y-4" key={scope}>
          <div className="grid gap-4 lg:grid-cols-5">
            <EndpointPanel view={view} className="lg:col-span-2" />
            <QuickstartPanel view={view} className="lg:col-span-3" />
          </div>
          <KeysPanel view={view} onView={setView} />
          <div className="grid items-start gap-4 lg:grid-cols-2">
            <JwtPanel key={`jwt-${view.key_generation}`} view={view} onView={setView} />
            <OidcPanel view={view} onView={setView} />
          </div>
          <RulesPanel view={view} />
        </div>
      )}
    </AppShell>
  );
}

// --- Endpoint and quickstart -------------------------------------------------------------------

function EndpointPanel({ view, className }: { view: StorageApiView; className?: string }) {
  const { copied, copy } = useCopy();
  return (
    <Panel title="Endpoint" className={className}>
      <div className="space-y-4 px-5 py-4">
        {view.endpoint ? (
          <div className="space-y-1">
            <p className="text-[12px] text-subtle">REST endpoint</p>
            <div className="flex items-center gap-1 rounded-lg border border-line bg-bg px-2.5 py-2">
              <code className="min-w-0 flex-1 truncate font-mono text-[11px]">{view.endpoint}</code>
              <CopyButton id="endpoint" text={view.endpoint} copied={copied} onCopy={copy} />
            </div>
            <p className="text-[11px] text-subtle">The storage URL a Supabase client takes, e.g. for storage-js.</p>
          </div>
        ) : (
          <p className="text-[13px] text-muted">
            rolld isn&apos;t serving the developer API right now, so there&apos;s no endpoint. The keys, providers and rules below
            still apply once it is.
          </p>
        )}
        <div className="space-y-1">
          <p className="text-[12px] text-subtle">Project</p>
          <div className="flex items-center gap-1 rounded-lg border border-line bg-bg px-2.5 py-2">
            <code className="min-w-0 flex-1 truncate font-mono text-[11px]">{view.account_id}</code>
            <CopyButton id="account" text={view.account_id} copied={copied} onCopy={copy} />
          </div>
          <p className="text-[11px] text-subtle">Each storage account is one project, with its own keys and buckets.</p>
        </div>
      </div>
    </Panel>
  );
}

function QuickstartPanel({ view, className }: { view: StorageApiView; className?: string }) {
  const [client, setClient] = useState<DevApiClient>("@roll/storage");
  const { copied, copy } = useCopy();
  const code = devApiSnippet(client, view.endpoint ?? "<endpoint>", view.anon_key);
  return (
    <Panel title="Quickstart" className={className}>
      <div className="p-4">
        <div className="overflow-hidden rounded-lg border border-line bg-bg">
          <div className="flex items-center gap-1 overflow-x-auto border-b border-line px-2 py-1.5">
            {DEV_API_CLIENTS.map((c) => (
              <button
                key={c}
                type="button"
                onClick={() => setClient(c)}
                className={`shrink-0 rounded-md px-2 py-1 font-mono text-[11px] transition ${
                  c === client ? "bg-panel-2 text-fg" : "text-subtle hover:text-fg"
                }`}
              >
                {c}
              </button>
            ))}
            <span className="ml-auto">
              <CopyButton id="snippet" text={code} copied={copied} onCopy={copy} />
            </span>
          </div>
          <pre className="overflow-x-auto px-4 py-3 font-mono text-[12px] leading-relaxed text-muted">{code}</pre>
        </div>
      </div>
      <p className="mt-auto border-t border-line px-5 py-3 text-[12px] text-subtle">
        Signed-in users: pass their JWT as <span className="font-mono text-muted">Authorization: Bearer &lt;token&gt;</span>{" "}
        instead of the anon key; rules see its claims.
      </p>
    </Panel>
  );
}

// --- Keys --------------------------------------------------------------------------------------

function KeysPanel({ view, onView }: { view: StorageApiView; onView: (v: StorageApiView) => void }) {
  const { copied, copy } = useCopy();
  const { busy, error, run } = useAction(noop);
  const [rotating, setRotating] = useState(false);
  return (
    <Panel
      title="API keys"
      aside={
        <span className="font-mono text-[12px] text-subtle" title={formatDate(view.keys_issued_at)}>
          generation {view.key_generation} · issued {agoMs(view.keys_issued_at)}
        </span>
      }
    >
      <div className="grid gap-x-4 gap-y-5 px-5 py-4 md:grid-cols-2">
        <div className="min-w-0 space-y-1">
          <p className="flex items-center gap-2 text-[12px] text-subtle">
            <span className="font-mono text-fg">anon</span>
            <span className="rounded-md border border-line px-1.5 py-px text-[10px] text-muted">public</span>
          </p>
          <SecretField id="anon" value={view.anon_key} masked={view.anon_key} copied={copied} onCopy={copy} />
          <p className="text-[11px] text-subtle">
            Safe in browsers and apps. Requests act as <span className="font-mono">anon</span>, or as the user whose JWT they send.
          </p>
        </div>
        <div className="min-w-0 space-y-1">
          <p className="flex items-center gap-2 text-[12px] text-subtle">
            <span className="font-mono text-fg">service_role</span>
            <span className="rounded-md border border-danger/40 px-1.5 py-px text-[10px] text-danger">secret</span>
          </p>
          <SecretField id="service" value={view.service_key} masked={maskSecret(view.service_key, 12)} copied={copied} onCopy={copy} />
          <p className="text-[11px] text-subtle">Full access to this account&apos;s storage. Servers only: never ship it to a browser.</p>
        </div>
      </div>
      <div className="space-y-3 border-t border-line px-5 py-4">
        <ErrorNote error={error} />
        {rotating ? (
          <Confirm
            word="rotate"
            action="Rotate keys"
            busy={busy === "rotate"}
            onCancel={() => setRotating(false)}
            onConfirm={async () => {
              const v = await run("rotate", storageApi.rotateApiKeys);
              if (v) {
                onView(v);
                setRotating(false);
              }
            }}
          >
            Both keys are replaced and the current ones stop working immediately: every app and server using them fails until it
            has the new keys. Signed URLs made with them stop working too.
          </Confirm>
        ) : (
          <div className="flex flex-wrap items-center justify-between gap-3">
            <p className="text-[12px] text-subtle">Rotate if a service key leaked. Users&apos; own JWTs aren&apos;t affected.</p>
            <button type="button" onClick={() => setRotating(true)} className={`${btn} hover:border-danger/50 hover:text-danger`}>
              <RefreshCw className="size-3.5" strokeWidth={1.5} /> Rotate keys
            </button>
          </div>
        )}
      </div>
    </Panel>
  );
}

// --- Auth providers ----------------------------------------------------------------------------

/** 48 random bytes as base64url (64 characters). */
function randomSecret() {
  const b = new Uint8Array(48);
  crypto.getRandomValues(b);
  return btoa(String.fromCharCode(...b))
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/, "");
}

function JwtPanel({ view, onView }: { view: StorageApiView; onView: (v: StorageApiView) => void }) {
  const auth = view.auth;
  const [secret, setSecret] = useState("");
  const [show, setShow] = useState(false);
  const [issuer, setIssuer] = useState(auth.jwt_issuer ?? "");
  const [audience, setAudience] = useState(auth.jwt_audience ?? "");
  const [removing, setRemoving] = useState(false);
  const { busy, error, run } = useAction(noop);
  const { copied, copy } = useCopy();
  const secretErr = secret ? jwtSecretError(secret) : null;
  const issuerChanged = issuer.trim() !== (auth.jwt_issuer ?? "");
  const audienceChanged = audience.trim() !== (auth.jwt_audience ?? "");
  const dirty = !!secret || issuerChanged || audienceChanged;

  const save = async () => {
    if (secretErr) return;
    const patch: ApiAuthPatch = {};
    if (secret) patch.jwt_secret = secret;
    if (issuerChanged) patch.jwt_issuer = issuer.trim() || null;
    if (audienceChanged) patch.jwt_audience = audience.trim() || null;
    const v = await run("jwt", () => storageApi.setApiAuth(patch));
    if (v) {
      onView(v);
      setSecret("");
      setShow(false);
      setIssuer(v.auth.jwt_issuer ?? "");
      setAudience(v.auth.jwt_audience ?? "");
    }
  };

  return (
    <Panel
      title="JWT secret"
      aside={
        <span className="flex items-center gap-1.5 text-[12px] text-subtle">
          <span className={`inline-block size-1.5 rounded-full ${auth.jwt_secret_set ? "bg-accent" : "border border-line-2"}`} />
          HS256 · {auth.jwt_secret_set ? "set" : "not set"}
        </span>
      }
    >
      <form
        className="space-y-4 px-5 py-4"
        onSubmit={(e) => {
          e.preventDefault();
          save();
        }}
      >
        <p className="text-[13px] text-muted">
          Accept user tokens signed with a shared secret, like Supabase Auth&apos;s JWT secret or your own auth server&apos;s. The
          secret is write-only: it&apos;s never shown again.
        </p>
        <ErrorNote error={error} />
        <Field
          label={auth.jwt_secret_set ? "Replace secret" : "Secret"}
          hint={secretErr ?? (secret ? `${secret.length} characters` : auth.jwt_secret_set ? "empty: keep the current secret" : "32–512 characters")}
        >
          <span className="flex items-center gap-1 rounded-lg border border-line bg-bg pr-1 focus-within:border-line-2">
            <input
              type={show ? "text" : "password"}
              value={secret}
              onChange={(e) => setSecret(e.target.value)}
              autoComplete="new-password"
              spellCheck={false}
              placeholder={auth.jwt_secret_set ? "••••••••  (unchanged)" : "at least 32 characters"}
              aria-invalid={!!secretErr}
              className="min-w-0 flex-1 bg-transparent px-2.5 py-1.5 font-mono text-[12px] outline-none placeholder:text-subtle"
            />
            {secret && (
              <>
                <button type="button" onClick={() => setShow((s) => !s)} className={iconBtn} aria-label={show ? "Hide" : "Show"}>
                  {show ? <EyeOff className="size-3.5" /> : <Eye className="size-3.5" />}
                </button>
                <CopyButton id="jwt-secret" text={secret} copied={copied} onCopy={copy} />
              </>
            )}
          </span>
        </Field>
        <button
          type="button"
          onClick={() => {
            setSecret(randomSecret());
            setShow(true);
          }}
          className={btn}
        >
          <KeyRound className="size-3.5" strokeWidth={1.5} /> Generate a secret
        </button>
        {secret && show && (
          <p className="rise text-[12px] text-muted">Copy it to your auth server before saving: it can&apos;t be read back.</p>
        )}
        <div className="grid gap-x-3 gap-y-4 sm:grid-cols-2">
          <Field label="Issuer (optional)" hint="tokens' iss must equal it">
            <input
              value={issuer}
              onChange={(e) => setIssuer(e.target.value)}
              spellCheck={false}
              placeholder="any"
              className={`${input} w-full font-mono text-[12px]`}
            />
          </Field>
          <Field label="Audience (optional)" hint="tokens' aud must include it">
            <input
              value={audience}
              onChange={(e) => setAudience(e.target.value)}
              spellCheck={false}
              placeholder="any"
              className={`${input} w-full font-mono text-[12px]`}
            />
          </Field>
        </div>
        {auth.jwt_secret_set &&
          !dirty &&
          (removing ? (
            <div className="rise flex flex-wrap items-center justify-between gap-3">
              <p className="text-[12px] text-muted">Tokens signed with it stop working at once.</p>
              <InlineConfirm
                label="Remove secret"
                busy={busy === "remove"}
                onCancel={() => setRemoving(false)}
                onConfirm={async () => {
                  const v = await run("remove", () => storageApi.setApiAuth({ jwt_secret: null }));
                  if (v) onView(v);
                  setRemoving(false);
                }}
              />
            </div>
          ) : (
            <button type="button" onClick={() => setRemoving(true)} className={`${btn} hover:border-danger/50 hover:text-danger`}>
              <Trash2 className="size-3.5" strokeWidth={1.5} /> Remove secret
            </button>
          ))}
        <SaveBar
          dirty={dirty}
          busy={busy === "jwt"}
          label={secret ? (auth.jwt_secret_set ? "Replace secret" : "Set secret") : "Save"}
          onReset={() => {
            setSecret("");
            setShow(false);
            setIssuer(auth.jwt_issuer ?? "");
            setAudience(auth.jwt_audience ?? "");
          }}
        />
      </form>
    </Panel>
  );
}

interface OidcDraft {
  issuer: string;
  audiences: string;
  jwks_uri: string;
}

const toOidcDraft = (p: ApiOidcProvider): OidcDraft => ({ issuer: p.issuer, audiences: p.audiences.join(", "), jwks_uri: p.jwks_uri ?? "" });
const fromOidcDraft = (d: OidcDraft): ApiOidcProvider => {
  const jwks = d.jwks_uri.trim();
  return {
    issuer: d.issuer.trim(),
    audiences: d.audiences
      .split(",")
      .map((s) => s.trim())
      .filter(Boolean),
    ...(jwks ? { jwks_uri: jwks } : {}),
  };
};
/** For comparing: `jwks_uri` absent and `null` are the same. */
const oidcKey = (list: ApiOidcProvider[]) => JSON.stringify(list.map((p) => [p.issuer, p.audiences, p.jwks_uri ?? null]));

function OidcPanel({ view, onView }: { view: StorageApiView; onView: (v: StorageApiView) => void }) {
  const saved = view.auth.oidc;
  const [drafts, setDrafts] = useState<OidcDraft[]>(() => saved.map(toOidcDraft));
  const { busy, error, run } = useAction(noop);
  const providers = drafts.map(fromOidcDraft);
  const dirty = oidcKey(providers) !== oidcKey(saved);
  const problem = providers.map(apiOidcError).find(Boolean) ?? null;
  const set = (i: number, patch: Partial<OidcDraft>) => setDrafts((d) => d.map((x, j) => (j === i ? { ...x, ...patch } : x)));
  const text = (i: number, field: keyof OidcDraft, label: string, placeholder: string, hint?: string) => (
    <Field label={label} hint={hint}>
      <input
        value={drafts[i][field]}
        onChange={(e) => set(i, { [field]: e.target.value })}
        placeholder={placeholder}
        spellCheck={false}
        className={`${input} w-full font-mono text-[12px]`}
      />
    </Field>
  );
  return (
    <Panel title="OIDC issuers" aside={<span className="font-mono text-[12px] text-subtle">{saved.length}</span>}>
      <form
        className="space-y-4 px-5 py-4"
        onSubmit={async (e) => {
          e.preventDefault();
          if (problem) return;
          const v = await run("oidc", () => storageApi.setApiAuth({ oidc: providers }));
          if (v) {
            onView(v);
            setDrafts(v.auth.oidc.map(toOidcDraft));
          }
        }}
      >
        <p className="text-[13px] text-muted">
          Accept ID tokens from identity providers (Auth0, Clerk, Firebase, Cognito, Google…), checked against their published
          keys.
        </p>
        <ErrorNote error={error} />
        {drafts.length === 0 && <p className="text-[13px] text-subtle">No issuers.</p>}
        {drafts.map((_, i) => (
          <div key={i} className="rise space-y-3 rounded-lg border border-line bg-bg p-4">
            <div className="flex items-center justify-between">
              <span className="font-mono text-[11px] text-subtle">issuer {i + 1}</span>
              <button
                type="button"
                onClick={() => setDrafts((d) => d.filter((_, j) => j !== i))}
                className={`${iconBtn} hover:text-danger`}
                aria-label={`Remove issuer ${i + 1}`}
              >
                <Trash2 className="size-3.5" />
              </button>
            </div>
            {text(i, "issuer", "Issuer URL", "https://auth.example.com/", "exactly the tokens' iss")}
            <div className="grid gap-x-3 gap-y-3 sm:grid-cols-2">
              {text(i, "audiences", "Audiences", "client-id", "comma separated; tokens' aud")}
              {text(i, "jwks_uri", "JWKS URI (optional)", "from discovery", "default: the issuer's discovery document")}
            </div>
          </div>
        ))}
        <button type="button" onClick={() => setDrafts((d) => [...d, { issuer: "", audiences: "", jwks_uri: "" }])} disabled={drafts.length >= MAX_OIDC} className={btn}>
          <Plus className="size-3.5" /> Add issuer
        </button>
        {problem && <p className="text-[12px] text-danger">{problem}</p>}
        <SaveBar dirty={dirty} busy={busy === "oidc"} onReset={() => setDrafts(saved.map(toOidcDraft))} />
      </form>
    </Panel>
  );
}

// --- Rules -------------------------------------------------------------------------------------

const pretty = (p: unknown) => JSON.stringify(p, null, 2);
const code = "font-mono text-[12px] text-fg";

function RulesPanel({ view }: { view: StorageApiView }) {
  const [buckets, setBuckets] = useState<Bucket[] | null>(null);
  const [bucketsErr, setBucketsErr] = useState<string | null>(null);
  const [bucket, setBucket] = useState("");
  const [preset, setPreset] = useState(view.presets[0]?.id ?? "");
  const [preview, setPreview] = useState<{ key: string; policy: IamPolicy | null; error: string | null } | null>(null);
  const [confirming, setConfirming] = useState(false);
  const [applied, setApplied] = useState<string | null>(null);
  const { busy, error, run } = useAction(noop);
  const { copied, copy } = useCopy();

  useEffect(() => {
    let alive = true;
    storageApi.buckets().then(
      (b) => {
        if (!alive) return;
        setBuckets(b);
        setBucket((cur) => cur || b[0]?.name || "");
      },
      (e) => alive && setBucketsErr(errorMessage(e)),
    );
    return () => {
      alive = false;
    };
  }, []);

  const key = `${bucket}\n${preset}`;
  useEffect(() => {
    if (!bucket || !preset) return;
    let alive = true;
    storageApi.rulePreview(bucket, preset).then(
      (r) => alive && setPreview({ key, policy: r.policy, error: null }),
      (e) => alive && setPreview({ key, policy: null, error: errorMessage(e) }),
    );
    return () => {
      alive = false;
    };
  }, [bucket, preset, key]);

  const current = preview?.key === key ? preview : null;
  const selected = buckets?.find((b) => b.name === bucket) ?? null;
  const presetInfo = view.presets.find((p) => p.id === preset);

  return (
    <Panel title="Rules" aside={<span className="text-[12px] text-subtle">bucket policies with JWT claims</span>}>
      <div className="space-y-4 px-5 py-4">
        <div className="space-y-2 text-[13px] text-muted">
          <p>
            Rules are bucket policies, evaluated for every developer-API request with the caller&apos;s JWT claims. The service key
            isn&apos;t bound by them (only an explicit <span className={code}>Deny</span> still applies); the anon key and
            users&apos; tokens get only what a bucket&apos;s policy grants.
          </p>
          <ul className="space-y-1.5 border-l border-line pl-3">
            <li>
              Principals: <span className={code}>{'{"App": "authenticated"}'}</span> is any signed-in user,{" "}
              <span className={code}>{'{"App": "anon"}'}</span> the anon key alone, <span className={code}>&quot;*&quot;</span>{" "}
              everyone, public URLs included.
            </li>
            <li>
              Claims: <span className={code}>{"${jwt:sub}"}</span>, <span className={code}>{"${jwt:role}"}</span>,{" "}
              <span className={code}>{"${jwt:email}"}</span> or any <span className={code}>{"${jwt:<claim>}"}</span> (nested:{" "}
              <span className={code}>jwt:app_metadata.plan</span>) in resources and conditions:{" "}
              <span className={code}>{"arn:aws:s3:::avatars/${jwt:sub}/*"}</span> is a user&apos;s own folder.
            </li>
            <li>
              Ownership: <span className={code}>roll:ObjectOwner</span> is the <span className={code}>sub</span> that uploaded an
              object, for owner-only rules.
            </li>
          </ul>
          <p>Start from a preset, then fine-tune the policy in the bucket&apos;s settings.</p>
        </div>
        <ErrorNote error={bucketsErr ?? error} />
        {buckets === null ? (
          !bucketsErr && (
            <p className="text-[13px] text-subtle">
              <span className="shimmer">Loading buckets…</span>
            </p>
          )
        ) : buckets.length === 0 ? (
          <p className="text-[13px] text-muted">
            No buckets yet.{" "}
            <Link href="/dashboard/storage" className="text-accent hover:underline">
              Create one
            </Link>{" "}
            to give it rules.
          </p>
        ) : (
          <>
            <div className="grid gap-x-3 gap-y-4 sm:grid-cols-2">
              <Field
                label="Bucket"
                hint={selected ? (selected.policy ? "has a policy: applying replaces it" : "no policy yet") : undefined}
              >
                <select
                  value={bucket}
                  onChange={(e) => {
                    setBucket(e.target.value);
                    setConfirming(false);
                    setApplied(null);
                  }}
                  className={`${input} w-full font-mono text-[12px]`}
                >
                  {buckets.map((b) => (
                    <option key={b.id} value={b.name}>
                      {b.name}
                    </option>
                  ))}
                </select>
              </Field>
              <Field label="Preset">
                <select
                  value={preset}
                  onChange={(e) => {
                    setPreset(e.target.value);
                    setConfirming(false);
                    setApplied(null);
                  }}
                  className={`${input} w-full`}
                >
                  {view.presets.map((p) => (
                    <option key={p.id} value={p.id}>
                      {p.name}
                    </option>
                  ))}
                </select>
              </Field>
            </div>
            {presetInfo && <p className="text-[12px] text-muted">{presetInfo.description}</p>}
            <div className="overflow-hidden rounded-lg border border-line bg-bg">
              <div className="flex items-center gap-2 border-b border-line px-3 py-1.5">
                <span className="font-mono text-[11px] text-subtle">{applied === bucket ? "preview · applied" : "preview · nothing applied yet"}</span>
                {current?.policy && (
                  <span className="ml-auto">
                    <CopyButton id="policy" text={pretty(current.policy)} copied={copied} onCopy={copy} />
                  </span>
                )}
              </div>
              {current?.error ? (
                <p className="px-4 py-3 text-[12px] text-danger">{current.error}</p>
              ) : current?.policy ? (
                <pre className="max-h-96 overflow-auto px-4 py-3 font-mono text-[12px] leading-relaxed text-muted">{pretty(current.policy)}</pre>
              ) : (
                <p className="px-4 py-3 text-[12px] text-subtle">
                  <span className="shimmer">Generating…</span>
                </p>
              )}
            </div>
            {applied && (
              <p className="rise text-[13px] text-ok">
                Applied to <span className="font-mono">{applied}</span>.{" "}
                <Link href={`/dashboard/storage/${encodeURIComponent(applied)}#settings`} className="text-accent hover:underline">
                  View its policy
                </Link>
              </p>
            )}
            {confirming ? (
              <div className="rise space-y-3 rounded-lg border border-danger/40 bg-danger/5 p-4">
                <p className="text-[13px] text-muted">
                  This replaces <span className="font-mono text-fg">{bucket}</span>&apos;s bucket policy
                  {selected?.policy ? ", and the statements it has now are lost" : ""}. S3 requests are governed by it too.
                </p>
                <InlineConfirm
                  label="Replace policy"
                  busy={busy === "apply"}
                  onCancel={() => setConfirming(false)}
                  onConfirm={async () => {
                    const b = await run("apply", () => storageApi.applyRule(bucket, preset));
                    if (b) {
                      setBuckets((list) => list?.map((x) => (x.id === b.id ? b : x)) ?? null);
                      setApplied(b.name);
                    }
                    setConfirming(false);
                  }}
                />
              </div>
            ) : (
              <div className="flex flex-wrap items-center justify-between gap-3 border-t border-line pt-4">
                <span className="text-[12px] text-subtle">Applying replaces the bucket&apos;s whole policy.</span>
                <button
                  type="button"
                  onClick={() => {
                    setConfirming(true);
                    setApplied(null);
                  }}
                  disabled={!current?.policy || busy !== null}
                  className={btn}
                >
                  Apply to {bucket}
                </button>
              </div>
            )}
          </>
        )}
      </div>
    </Panel>
  );
}
