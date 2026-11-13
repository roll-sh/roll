import { HorizonMark } from "@roll/brand";
import { ExternalLink, LucideProvider } from "lucide-react";
import { useEffect, useState } from "react";
import {
  accessApi,
  denyRedirect,
  describeRule,
  EXPIRIES,
  parseAuthorize,
  presetFor,
  parseRate,
  presetRules,
  type AuthorizeRequest,
  type Catalog,
  type Me,
  type OAuthClient,
  type Rule,
} from "@/lib/access-api";
import { requestSignIn, subscribeToken } from "@/lib/code-token";
import { SignIn } from "../code/sign-in";
import { btn, Connecting, ErrorNote, input, primary } from "../ui";
import { Builder } from "./builder";

type Loaded = { me: Me; catalog: Catalog; client: OAuthClient };

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
        <main className="mx-auto w-full max-w-3xl flex-1 px-4 py-10">{"error" in req ? <Problem text={req.error} /> : <Ask req={req} />}</main>
      </div>
    </LucideProvider>
  );
}

function Problem({ text }: { text: string }) {
  return (
    <section className="rise rounded-xl border border-line bg-panel p-6">
      <HorizonMark state="error" size={28} />
      <h1 className="mt-4 text-[15px] font-medium">This connection request can&apos;t be used</h1>
      <p className="mt-1 text-[13px] text-muted">{text} Start connecting again from your MCP client.</p>
    </section>
  );
}

function Ask({ req }: { req: AuthorizeRequest }) {
  const [loaded, setLoaded] = useState<Loaded | null>(null);
  const [signedOut, setSignedOut] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [rules, setRules] = useState<Rule[]>([]);
  const [name, setName] = useState("");
  const [expiry, setExpiry] = useState(2);
  const [rateText, setRateText] = useState("");
  const [askOtherwise, setAskOtherwise] = useState(false);
  const rate = parseRate(rateText);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    const load = async () => {
      try {
        const [me, catalog, client] = await Promise.all([accessApi.me(), accessApi.catalog(), accessApi.client(req.client_id)]);
        setSignedOut(false);
        setLoaded({ me, catalog, client });
        setName(client.name);
        const preset = presetFor(req.scope, catalog.presets);
        if (preset) setRules(presetRules(preset));
      } catch (e) {
        const msg = e instanceof Error ? e.message : String(e);
        if (/token|401|unauthori/i.test(msg)) {
          setSignedOut(true);
          requestSignIn();
        } else setError(msg);
      }
    };
    void load();
    return subscribeToken(() => void load());
  }, [req.client_id, req.scope]);

  if (signedOut) {
    return (
      <section className="rise rounded-xl border border-line bg-panel p-6">
        <HorizonMark state="sleep" size={28} />
        <h1 className="mt-4 text-[15px] font-medium">Sign in to roll to connect this client</h1>
        <p className="mt-1 text-[13px] text-muted">Use roll&apos;s operator token. The request waits here.</p>
        <button onClick={requestSignIn} className={`${primary} mt-4`}>
          Sign in
        </button>
      </section>
    );
  }
  if (!loaded) return error ? <Problem text={error} /> : <Connecting />;
  const { me, catalog, client } = loaded;
  if (me.kind !== "operator") return <Problem text="Only roll's operator can connect clients, and you're signed in with a token." />;
  if (!client.redirect_uris.includes(req.redirect_uri)) return <Problem text="The redirect isn't one this client registered." />;

  const back = new URL(req.redirect_uri);
  const allow = async () => {
    setBusy(true);
    setError(null);
    try {
      const ms = EXPIRIES[expiry].ms;
      const { redirect } = await accessApi.authorize({
        client_id: req.client_id,
        redirect_uri: req.redirect_uri,
        code_challenge: req.code_challenge,
        code_challenge_method: req.code_challenge_method,
        state: req.state,
        resource: req.resource,
        name: name.trim() || client.name,
        rules,
        expires_at: ms === null ? null : Date.now() + ms,
        rate_per_minute: rate.rate,
        ask_otherwise: askOtherwise,
      });
      window.location.assign(redirect);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setBusy(false);
    }
  };

  return (
    <div className="rise space-y-6">
      <section className="flex items-start gap-4">
        <HorizonMark state={busy ? "thinking" : "idle"} size={36} interactive />
        <div className="min-w-0">
          <h1 className="text-xl font-semibold tracking-[-0.04em]">
            {client.name} wants to use roll
          </h1>
          <p className="mt-1 text-[12px] text-subtle">
            Any app can register under any name: check the address it returns to below, and that you started this from that app.
          </p>
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
            <span className="block text-[12px] text-muted">What these rules don't allow waits for your approval instead of being refused.</span>
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
      <ErrorNote error={error} />
      <div className="flex gap-3">
        <button onClick={allow} disabled={busy || rules.length === 0 || Boolean(rate.problem)} className={primary}>
          {busy ? <span className="shimmer">Connecting…</span> : `Allow ${client.name}`}
        </button>
        <button onClick={() => window.location.assign(denyRedirect(req.redirect_uri, req.state))} disabled={busy} className={btn}>
          Deny
        </button>
      </div>
    </div>
  );
}
