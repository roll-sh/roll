import { HorizonMark } from "@roll/brand";
import { ExternalLink, LucideProvider } from "lucide-react";
import { type ReactNode, useEffect, useState } from "react";
import {
  accessApi,
  broadRule,
  catalogFor,
  denyRedirect,
  describeRule,
  EXPIRIES,
  lifetimeCap,
  parseAuthorize,
  presetFor,
  parseRate,
  presetRules,
  redirectRegistered,
  stepUpId,
  type Allowed,
  type AuthorizeRequest,
  type Catalog,
  type Me,
  type Rule,
  type StepUp,
  waitsForAdmin,
} from "@/lib/access-api";
import { accountsApi } from "@/lib/accounts-api";
import { requestSignIn, subscribeToken } from "@/lib/code-token";
import { SignIn } from "../code/sign-in";
import { btn, Connecting, ErrorNote, input, primary } from "../ui";
import { Builder } from "./builder";

/**
 * `/oauth/authorize`: roll's OAuth consent screen for MCP clients (docs/mcp.md, "Authorization").
 * The signed-in operator sees which client asks and where it'll be sent back, chooses what it may
 * do (starting from the preset its `scope` asks for), and allows or denies. The client gets exactly
 * these rules; nothing it asked for widens them. A redirect is only ever followed when the client
 * registered it.
 */
export function Consent({ search }: { search: Record<string, unknown> }) {
  const req = parseAuthorize(search);
  return (
    <LucideProvider strokeWidth={1.5}>
      <div className="flex min-h-svh flex-col bg-bg">
        <header className="flex h-12 items-center justify-between border-b border-line px-4">
          <HorizonMark size={18} />
          <SignIn />
        </header>
        <main className="mx-auto w-full max-w-3xl flex-1 px-4 py-10">
          {"error" in req ? <Problem text={req.error} /> : req.scope?.split(" ").includes(EMA_SCOPE) ? <ExpressSetup req={req} /> : <Ask req={req} />}
        </main>
      </div>
    </LucideProvider>
  );
}

/** Claude's express admin setup (Claude's connector docs): its scope, its two clients, its callback. */
const EMA_SCOPE = "ema_issuer:manage";
const CLAUDE_CLIENTS = ["https://claude.ai/oauth/mcp-oauth-client-metadata", "https://claude.ai/oauth/mcp-oauth-client-metadata/confidential_client"];
const CLAUDE_CALLBACK = "https://claude.ai/api/mcp/auth_callback";

/**
 * Claude asks to set up auto-connect: an admin of roll names the email domains Claude's issuer
 * vouches for (its organization's members are found by their account's email, only in these) and
 * the most Claude may do for a person. Claude then sends its issuer, and roll trusts it.
 */
function ExpressSetup({ req }: { req: AuthorizeRequest }) {
  const loaded = useSignedInLoad(() => Promise.resolve(null), []);
  const [domains, setDomains] = useState("");
  const [rules, setRules] = useState<Rule[] | null>(null);
  // The domains a DNS record proved for roll's SSO connections, to start from (the admin changes them).
  useEffect(() => {
    accountsApi.ssoConnections().then(
      (cs) => setDomains((d) => d || [...new Set(cs.flatMap((c) => c.verified))].join(", ")),
      () => {},
    );
  }, []);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    if (loaded.state === "ready" && rules === null) {
      const start = loaded.catalog.presets.find((p) => p.id === "read-only") ?? loaded.catalog.presets[0];
      setRules(start ? presetRules(start) : []);
    }
  }, [loaded, rules]);
  if (loaded.state !== "ready") return <LoadState loaded={loaded} what="set up Claude's auto-connect" />;
  if (!CLAUDE_CLIENTS.includes(req.client_id) || req.redirect_uri !== CLAUDE_CALLBACK) return <Problem text="Only Claude asks for this, returning to claude.ai." />;
  if (loaded.me.kind !== "operator")
    return (
      <div className="space-y-4">
        <Problem title="An admin of roll approves this" text="Claude's auto-connect makes roll trust your Claude organization for everyone in the domains you name." next="Ask an admin of roll to click Connect in Claude's auto-connect setup." />
        <button
          onClick={() => window.location.assign(denyRedirect(req.redirect_uri, req.state, loaded.catalog.api_url, "Only roll's admins approve auto-connect"))}
          className={btn}
        >
          Back to Claude
        </button>
      </div>
    );
  const list = domains
    .split(/[\s,]+/)
    .map((d) => d.trim().replace(/^@/, "").toLowerCase())
    .filter(Boolean);
  const problems = [...(list.length ? [] : ["Name at least one email domain"]), ...(rules?.length ? [] : ["Allow at least one action"])];
  const allow = async () => {
    setBusy(true);
    setError(null);
    try {
      const { redirect } = await accessApi.authorize({
        rules: rules ?? [],
        ema_domains: list,
        client_id: req.client_id,
        redirect_uri: req.redirect_uri,
        code_challenge: req.code_challenge,
        code_challenge_method: req.code_challenge_method,
        state: req.state,
        resource: req.resource,
      });
      window.location.assign(redirect);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setBusy(false);
    }
  };
  return (
    <section className="rise space-y-5">
      <div className="flex items-start gap-4">
        <HorizonMark state={busy ? "thinking" : "idle"} size={36} interactive />
        <div className="min-w-0">
          <h1 className="text-xl font-semibold tracking-[-0.04em]">Claude wants to set up auto-connect</h1>
          <p className="mt-1 text-[13px] text-muted">
            Approving lets Claude register your Claude organization&apos;s issuer with roll. From then on, its members connect to roll without a
            consent screen, each as their own roll account (found by their Claude account&apos;s email), and only in the domains below. You can
            remove it any time under MCP &amp; access, Enterprise sign-in.
          </p>
        </div>
      </div>
      <label className="block space-y-1.5">
        <span className="block text-[12px] text-subtle">Email domains it vouches for (your company&apos;s, never a public one)</span>
        <input autoFocus value={domains} onChange={(e) => setDomains(e.target.value)} placeholder="acme.com" className={`${input} w-full sm:w-1/2`} />
      </label>
      <div className="space-y-1.5">
        <p className="text-[12px] text-subtle">The most Claude may do for a person (never more than their own account reaches)</p>
        {rules && <Builder catalog={loaded.catalog} rules={rules} onChange={setRules} />}
      </div>
      <ErrorNote error={error} />
      <div className="flex items-center gap-3">
        <button onClick={allow} disabled={busy || problems.length > 0} className={primary}>
          {busy ? <span className="shimmer">Setting up…</span> : "Approve"}
        </button>
        <button onClick={() => window.location.assign(denyRedirect(req.redirect_uri, req.state, loaded.catalog.api_url))} className={btn}>
          Deny
        </button>
        {problems.length > 0 && <span className="text-[12px] text-subtle">{problems[0]}</span>}
      </div>
    </section>
  );
}

/**
 * What the client's identity is worth: a client ID metadata document (its id an https URL) is
 * vouched for by whoever controls that address; a client registered here chose its own name.
 */
export function clientIdentity(id: string): string {
  if (id.startsWith("https://")) {
    return `Identified by its own address, ${new URL(id).host}: whoever controls that address published this client. Check it's the app you meant.`;
  }
  return "Any app can register under any name: check the address it returns to below, and that you started this from that app.";
}

/** Why a page can't go on: a connection request's problem by default, or the caller's own words. */
export function Problem({ text, title = "This connection request can't be used", next = "Start connecting again from your MCP client." }: { text: string; title?: string; next?: string }) {
  return (
    <section className="rise rounded-xl border border-line bg-panel p-6">
      <HorizonMark state="error" size={28} />
      <h1 className="mt-4 text-[15px] font-medium">{title}</h1>
      <p className="mt-1 text-[13px] text-muted">
        {text.charAt(0).toUpperCase() + text.slice(1)}
        {next ? ` ${next}` : ""}
      </p>
    </section>
  );
}

function Ask({ req }: { req: AuthorizeRequest }) {
  const stepUp = stepUpId(req.scope);
  // A step-up roll recorded for one of this person's connections (else: none, and a plain consent).
  const loaded = useSignedInLoad(
    () => Promise.all([accessApi.client(req.client_id), stepUp ? accessApi.stepUp(stepUp).catch(() => null) : Promise.resolve(null)]),
    [req.client_id, stepUp],
  );
  const [fresh, setFresh] = useState(false);
  if (loaded.state !== "ready") return <LoadState loaded={loaded} what="connect this client" />;
  const {
    me,
    catalog,
    value: [client, proposal],
  } = loaded;
  if (!redirectRegistered(client.redirect_uris, req.redirect_uri)) return <Problem text="The redirect isn't one this client registered." />;
  if (client.not_allowed) return <Problem title={`${client.name} can't connect`} text={`${client.not_allowed}.`} next="" />;
  const back = new URL(req.redirect_uri);
  if (proposal && proposal.connection.client_id === client.id && !fresh) {
    return (
      <StepUpConfirm
        clientName={client.name}
        proposal={proposal}
        onAdd={async () => {
          const { redirect } = await accessApi.authorize({
            rules: [],
            step_up: proposal.id,
            client_id: req.client_id,
            redirect_uri: req.redirect_uri,
            code_challenge: req.code_challenge,
            code_challenge_method: req.code_challenge_method,
            state: req.state,
            resource: req.resource,
          });
          window.location.assign(redirect);
        }}
        onDeny={() => window.location.assign(denyRedirect(req.redirect_uri, req.state, catalog.api_url))}
        onFresh={() => setFresh(true)}
      />
    );
  }
  return (
    <AllowForm
      me={me}
      catalog={catalog}
      clientName={client.name}
      scope={req.scope}
      onAllow={async (a) => {
        const { redirect } = await accessApi.authorize({
          ...a,
          client_id: req.client_id,
          redirect_uri: req.redirect_uri,
          code_challenge: req.code_challenge,
          code_challenge_method: req.code_challenge_method,
          state: req.state,
          resource: req.resource,
        });
        window.location.assign(redirect);
      }}
      onDeny={() => window.location.assign(denyRedirect(req.redirect_uri, req.state, catalog.api_url))}
      head={(busy) => (
        <section className="flex items-start gap-4">
          <HorizonMark state={busy ? "thinking" : "idle"} size={36} interactive />
          <div className="min-w-0">
            <h1 className="text-xl font-semibold tracking-[-0.04em]">{client.name} wants to use roll</h1>
            <p className="mt-1 text-[12px] text-subtle">{clientIdentity(client.id)}</p>
            <p className="mt-1 text-[13px] text-muted">
              It will act on your behalf through roll&apos;s MCP server, and can do only what you allow below. You can narrow it now and
              revoke it any time in MCP &amp; access.
            </p>
            <p className="mt-2 flex flex-wrap items-center gap-1 font-mono text-[11px] text-subtle">
              <ExternalLink className="size-3" /> returns to {back.protocol}//{back.host}
              {client.uri ? ` · ${client.uri}` : ""} · client {client.id}
            </p>
          </div>
        </section>
      )}
    />
  );
}

/**
 * Step-up's consent: a refused call of one of the person's connections asks for one rule. They see
 * the rule (said plainly when it reaches far) and which connection it joins, and add it, deny it, or
 * set up a new connection instead. What's added is roll's record of the refusal, never the client's.
 */
export function StepUpConfirm({
  clientName,
  proposal,
  onAdd,
  onDeny,
  onFresh,
}: {
  clientName: string;
  proposal: StepUp;
  onAdd: () => Promise<void>;
  onDeny: () => void;
  onFresh: () => void;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const add = async () => {
    setBusy(true);
    setError(null);
    try {
      await onAdd();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setBusy(false);
    }
  };
  return (
    <section className="rise space-y-4 rounded-xl border border-line bg-panel p-6">
      <HorizonMark state={busy ? "thinking" : "idle"} size={28} />
      <h1 className="text-xl font-semibold tracking-[-0.04em]">
        {clientName} asks for one more permission on &ldquo;{proposal.connection.name}&rdquo;
      </h1>
      <p className="text-[13px] text-muted">A call it made was refused by this connection&apos;s rules. Adding this lets it make that call:</p>
      <p className="rounded-lg border border-line bg-bg px-3 py-2 font-mono text-[12px]">{describeRule(proposal.rule)}</p>
      {broadRule(proposal.rule) && (
        <p className="text-[12px] text-danger">This reaches far: it applies to every resource of the product, or lets it reveal or administer. Add it only if you meant to.</p>
      )}
      {proposal.waits_for_admin && <p className="text-[12px] text-muted">Your organization&apos;s policy has an admin approve this: the connection waits until they do.</p>}
      {proposal.ends && <p className="text-[12px] text-muted">With it, this connection ends on {new Date(proposal.ends.until).toISOString().slice(0, 10)}: roll secrets&apos; policy.</p>}
      <p className="text-[12px] text-subtle">Its other rules and settings stay as they are, and every session of this connection gets it.</p>
      <ErrorNote error={error} />
      <div className="flex flex-wrap items-center gap-2">
        <button onClick={add} disabled={busy} className={primary}>
          {busy ? <span className="shimmer">Adding…</span> : "Add it"}
        </button>
        <button onClick={onDeny} disabled={busy} className={btn}>
          Deny
        </button>
        <button onClick={onFresh} disabled={busy} className="text-[12px] text-subtle underline-offset-2 hover:underline">
          Set up a new connection instead
        </button>
      </div>
    </section>
  );
}

export type Loaded<T> = { state: "loading" } | { state: "signed-out" } | { state: "error"; error: string } | { state: "ready"; me: Me; catalog: Catalog; value: T };

/**
 * What a consent page needs, loaded as the signed-in person: who they are, the catalogue they
 * may build from, and the page's own thing (the client, the device). Signed out, it asks them
 * to sign in and loads again once they have.
 */
export function useSignedInLoad<T>(load: () => Promise<T>, deps: unknown[]): Loaded<T> {
  const [loaded, setLoaded] = useState<Loaded<T>>({ state: "loading" });
  useEffect(() => {
    const run = async () => {
      try {
        const [me, catalog, value] = await Promise.all([accessApi.me(), accessApi.catalog(), load()]);
        setLoaded({ state: "ready", me, catalog: catalogFor(catalog, me), value });
      } catch (e) {
        const msg = e instanceof Error ? e.message : String(e);
        if (/token|401|unauthori/i.test(msg)) {
          setLoaded({ state: "signed-out" });
          requestSignIn();
        } else setLoaded({ state: "error", error: msg });
      }
    };
    void run();
    return subscribeToken(() => void run());
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);
  return loaded;
}

/** A consent page that isn't ready: signing in, loading, or what went wrong. */
export function LoadState({ loaded, what }: { loaded: Exclude<Loaded<unknown>, { state: "ready" }>; what: string }) {
  if (loaded.state === "signed-out") {
    return (
      <section className="rise rounded-xl border border-line bg-panel p-6">
        <HorizonMark state="sleep" size={28} />
        <h1 className="mt-4 text-[15px] font-medium">Sign in to roll to {what}</h1>
        <p className="mt-1 text-[13px] text-muted">The request waits here.</p>
        <button onClick={requestSignIn} className={`${primary} mt-4`}>
          Sign in
        </button>
      </section>
    );
  }
  return loaded.state === "error" ? <Problem text={loaded.error} /> : <Connecting />;
}

/**
 * What a person allows a client, the same on the consent screen and for a device: the rules
 * (starting from the preset its `scope` asks for), a name, an end, a rate, and asking for the
 * rest. `onAllow` throws to show an error.
 */
export function AllowForm({
  me,
  catalog,
  clientName,
  scope,
  onAllow,
  onDeny,
  head,
}: {
  me: Me;
  catalog: Catalog;
  clientName: string;
  scope?: string | null;
  onAllow: (a: Allowed) => Promise<void>;
  onDeny: () => void | Promise<void>;
  head: (busy: boolean) => ReactNode;
}) {
  const [rules, setRules] = useState<Rule[]>(() => {
    const preset = presetFor(scope ?? undefined, catalog.presets);
    return preset ? presetRules(preset) : [];
  });
  const [name, setName] = useState(clientName);
  const [expiry, setExpiry] = useState(2);
  const [rateText, setRateText] = useState("");
  const [askOtherwise, setAskOtherwise] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const rate = parseRate(rateText);
  const cap = lifetimeCap(catalog, rules);
  if (me.kind !== "operator" && me.kind !== "user") return <Problem text="Connect clients signed in to your account, not with a token." />;

  const allow = async () => {
    setBusy(true);
    setError(null);
    try {
      const ms = EXPIRIES[expiry].ms;
      await onAllow({ name: name.trim() || clientName, rules, expires_at: ms === null ? null : Date.now() + ms, rate_per_minute: rate.rate, ask_otherwise: askOtherwise });
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setBusy(false);
    }
  };
  const deny = async () => {
    setBusy(true);
    try {
      await onDeny();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setBusy(false);
    }
  };

  return (
    <div className="rise space-y-6">
      {head(busy)}

      <section className="rounded-xl border border-line bg-panel p-5">
        <Builder catalog={catalog} rules={rules} onChange={(r) => setRules(r)} />
      </section>

      <section className="grid gap-4 rounded-xl border border-line bg-panel p-5 sm:grid-cols-2">
        <label className="block space-y-1.5">
          <span className="block text-[12px] text-subtle">Name the connection</span>
          <input value={name} onChange={(e) => setName(e.target.value)} maxLength={64} className={`${input} w-full`} />
        </label>
        <label className="block space-y-1.5">
          <span className="block text-[12px] text-subtle">Ends after</span>
          <select value={expiry} onChange={(e) => setExpiry(Number(e.target.value))} className={`${input} w-full`}>
            {EXPIRIES.map((x, i) => (
              <option key={x.label} value={i}>
                {x.label}
              </option>
            ))}
          </select>
        </label>
        <label className="block space-y-1.5">
          <span className="block text-[12px] text-subtle">Requests a minute (optional)</span>
          <input value={rateText} onChange={(e) => setRateText(e.target.value)} inputMode="numeric" placeholder="No limit" className={`${input} w-full font-mono text-[12px]`} />
          {rate.problem && <span className="block text-[11px] text-danger">{rate.problem}</span>}
        </label>
        <label className="flex items-start gap-2 text-[13px] sm:col-span-2">
          <input type="checkbox" checked={askOtherwise} onChange={(e) => setAskOtherwise(e.target.checked)} className="mt-0.5" />
          <span>
            Ask me for anything else
            <span className="block text-[12px] text-muted">What these rules don&apos;t allow waits for your approval instead of being refused.</span>
          </span>
        </label>
      </section>

      <div className="space-y-1">
        {rules.length === 0 ? (
          <p className="text-[12px] text-danger">Nothing is allowed yet: pick a preset or tick an action.</p>
        ) : (
          rules.map((r, i) => (
            <p key={i} className="text-[12px] text-muted">
              {describeRule(r)}
            </p>
          ))
        )}
      </div>
      {cap && (EXPIRIES[expiry].ms ?? Infinity) > cap.days * 86_400_000 && (
        <p className="text-[12px] text-muted">
          This connection ends in {cap.days} {cap.days === 1 ? "day" : "days"}: {cap.product.endsWith("s") ? `roll ${cap.product}' policy` : `roll ${cap.product}'s policy`}.
        </p>
      )}
      {waitsForAdmin(me, rules, askOtherwise) && (
        <p className="text-[12px] text-accent">It reaches what an admin approves first: {clientName} connects now, and works once an admin approves it (they&apos;re mailed).</p>
      )}
      <ErrorNote error={error} />
      <div className="flex gap-3">
        <button onClick={allow} disabled={busy || rules.length === 0 || Boolean(rate.problem)} className={primary}>
          {busy ? <span className="shimmer">Connecting…</span> : `Allow ${clientName}`}
        </button>
        <button onClick={deny} disabled={busy} className={btn}>
          Deny
        </button>
      </div>
    </div>
  );
}
