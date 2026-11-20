import { Lock, Rocket, Trash2, Unlock } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { appsApi, formatFirewall, formatHeaders, parseAllowList, parseFirewall, parseHeaders, PRODUCTION, shortSha, type Backup, type Environment, type EnvRunField, type Service, type Snippet, formatRoutes, parseRoutes } from "@/lib/apps-api";
import { Field } from "../redis/shared";
import { CompareSection } from "./env-compare";
import { btn, Confirm, CopyButton, danger, input, Panel, primary, useCopy } from "../ui";
import { clock, CPU, MEMORY, mb, since, StateBadge, SureButton, type Runner } from "./shared";

/** A private environment's token: 32 of [a-z0-9], from the browser's CSPRNG. */
export function newAccessToken(): string {
  const alphabet = "abcdefghijklmnopqrstuvwxyz0123456789";
  let out = "";
  while (out.length < 32) {
    // 36 doesn't divide 256: bytes of 252 and up are dropped, so every character is as likely.
    for (const b of crypto.getRandomValues(new Uint8Array(48))) if (b < 252 && out.length < 32) out += alphabet[b % 36];
  }
  return out;
}

/** The link that lets a visitor into a private environment (once: then a cookie). */
export const accessLink = (url: string, token: string) => `${url.replace(/\/$/, "")}/?roll_access=${encodeURIComponent(token)}`;

/** The environment's instances, each restarted on its own (a fresh one answers before it leaves). */
export function InstancesPanel({ service, env, runner }: { service: Service; env: Environment; runner: Runner }) {
  if (env.instances.length === 0) return null;
  return (
    <Panel title="Instances" aside={<span className="text-[12px] text-muted">restart one on its own</span>}>
      <ul className="divide-y divide-line">
        {env.instances.map((i) => (
          <li key={i.id} className="flex flex-wrap items-center justify-between gap-3 px-5 py-2.5 text-[13px]">
            <span className="flex min-w-0 items-center gap-3">
              <StateBadge state={i.state} />
              <span className="truncate font-mono text-[12px]">{i.id}</span>
            </span>
            <span className="flex items-center gap-3 font-mono text-[11.5px] text-subtle">
              {i.started_at && <span>since {clock(i.started_at, true)}</span>}
              {i.in_flight > 0 && <span>{i.in_flight} in flight</span>}
              <SureButton
                className={btn}
                disabled={runner.busy !== null}
                sure="Sure? Restart it"
                title="Replace just this instance: a fresh one answers before it leaves"
                onSure={() => runner.run(`restart:${i.id}`, () => appsApi.restartInstance(service.id, env.name, i.id))}
              >
                Restart
              </SureButton>
            </span>
          </li>
        ))}
      </ul>
    </Panel>
  );
}

/**
 * The environment shown, its own settings: the branch that deploys to it, promoting another
 * environment's live image to it, private access, and removing it (production stays).
 */
export function EnvironmentPanel({ service, env, runner, onDeleted }: { service: Service; env: Environment; runner: Runner; onDeleted: () => void }) {
  const [branch, setBranch] = useState(env.branch ?? "");
  const sources = service.environments.filter((e) => e.name !== env.name && e.active);
  const [from, setFrom] = useState(sources[0]?.name ?? "");
  // What a promotion puts live, looked at before it does: the deployment (sent as `expect`) and
  // the variables `from` has that this environment lacks (names only).
  const [promoting, setPromoting] = useState<{ from: string; deployment: string; revision: string | null; missing: string[] } | null>(null);
  const look = async () => {
    const source = service.environments.find((e) => e.name === from)?.active;
    if (!source) return;
    const [theirs, ours] = await Promise.all([appsApi.variables(service.id, from), appsApi.variables(service.id, env.name)]);
    const have = new Set(ours.vars.map((v) => v.name));
    const missing = theirs.vars.filter((v) => (v.source === "service" || v.source === "reference" || v.source === "secret") && !have.has(v.name)).map((v) => v.name);
    setPromoting({ from, deployment: source.id, revision: source.revision, missing });
  };
  const [link, setLink] = useState<string | null>(null);
  const [pauseFrom, setPauseFrom] = useState("fri 19:00");
  const [pauseTo, setPauseTo] = useState("mon 07:00");
  const [freezeWhy, setFreezeWhy] = useState("");
  const [password, setPassword] = useState("");
  const [hook, setHook] = useState<string | null>(null);
  const [deleting, setDeleting] = useState(false);
  const [notice, setNotice] = useState(env.maintenance?.message ?? "");
  const closed = env.egress?.mode === "roll";
  const egressText = (env.egress?.hosts ?? []).join("\n");
  const [egressHosts, setEgressHosts] = useState(egressText);
  useEffect(() => setEgressHosts(egressText), [egressText]);
  const hostList = egressHosts.split(/[\s,]+/).filter(Boolean);
  const allowText = (env.allow_ips ?? []).map((r) => (r.description ? `${r.cidr} ${r.description}` : r.cidr)).join("\n");
  const [allow, setAllow] = useState(allowText);
  useEffect(() => setAllow(allowText), [allowText]);
  const fwText = formatFirewall(env.firewall ?? []);
  const [fw, setFw] = useState(fwText);
  useEffect(() => setFw(fwText), [fwText]);
  const hdrText = formatHeaders(env.headers ?? []);
  const [hdr, setHdr] = useState(hdrText);
  useEffect(() => setHdr(hdrText), [hdrText]);
  const routeText = formatRoutes(env.routes ?? []);
  const [routes, setRoutes] = useState(routeText);
  useEffect(() => setRoutes(routeText), [routeText]);
  const rateText = env.rate_limit ? String(env.rate_limit.per_minute) : "";
  const [rate, setRate] = useState(rateText);
  useEffect(() => setRate(rateText), [rateText]);
  const { copied, copy } = useCopy();
  const repo = service.source.kind === "repo";
  // A Git URL's checks roll can read: through its GitHub App, or GitLab's and Bitbucket's with its Git login.
  const ciReadable =
    service.source.kind === "git" && (!!service.github || (!!service.git_login && /^https?:\/\/(gitlab\.|bitbucket\.org\/)/i.test(service.source.url)));
  const busy = runner.busy !== null;
  const branchChanged = branch.trim() !== (env.branch ?? "");

  const makePrivate = async () => {
    const token = newAccessToken();
    const done = await runner.run("access", () => appsApi.putEnvironment(service.id, env.name, { access: { token } }));
    if (done && env.url) setLink(accessLink(env.url, token));
  };

  return (
    <Panel title={<>Environment <span className="font-mono text-[13px] text-muted">{env.name}</span></>}>
      <div className="divide-y divide-line">
        {service.run.tcp && <p className="px-5 pb-2 pt-5 text-[11px] font-medium uppercase tracking-[0.06em] text-subtle">Network</p>}

        {service.run.tcp && (
          <div className="flex flex-wrap items-center justify-between gap-3 px-5 py-4">
            <div>
              <p className="text-[13px] font-medium">TCP address</p>
              <p className="mt-1 text-[12px] text-muted">
                {env.tcp ? "Clients connect here; each connection wakes the service and keeps it up while it's open." : "Its port comes once the node has a TCP range (ROLL_APPS_TCP_PORTS)."}
              </p>
            </div>
            {env.tcp && (
              <span className="flex items-center gap-1">
                <code className="font-mono text-[12px]">{env.tcp}</code>
                <CopyButton id="tcp" text={env.tcp} copied={copied} onCopy={copy} />
              </span>
            )}
          </div>
        )}
        <p className="px-5 pb-2 pt-5 text-[11px] font-medium uppercase tracking-[0.06em] text-subtle">Deploys</p>

        {(repo || service.source.kind === "git") && (
          <form
            className="flex flex-wrap items-end gap-3 px-5 py-4"
            onSubmit={(e) => {
              e.preventDefault();
              void runner.run("branch", () => appsApi.putEnvironment(service.id, env.name, { branch: branch.trim() || null }));
            }}
          >
            <div className="min-w-56 flex-1">
              <Field label="Deploys on pushes to" hint={env.branch ? undefined : "no branch: deploys only when asked"}>
                <input value={branch} onChange={(e) => setBranch(e.target.value)} placeholder="a branch" spellCheck={false} className={`${input} w-full font-mono text-[12px]`} />
              </Field>
            </div>
            <button type="submit" className={primary} disabled={!branchChanged || busy}>
              {runner.busy === "branch" ? <span className="shimmer">Saving…</span> : "Save"}
            </button>
          </form>
        )}

        {(repo || ciReadable) && (
          <div className="flex flex-wrap items-center justify-between gap-3 px-5 py-4">
            <div>
              <p className="text-[13px] font-medium">Wait for CI</p>
              <p className="mt-1 text-[12px] text-muted">
                {env.wait_for_ci
                  ? env.waiting_for_ci
                    ? `Push ${env.waiting_for_ci.slice(0, 7)} deploys once its checks pass.`
                    : "A push deploys once its commit's checks pass; a failed check doesn't deploy."
                  : "A push deploys at once, whatever its checks say."}
              </p>
            </div>
            <button className={btn} disabled={busy} onClick={() => runner.run("ci", () => appsApi.putEnvironment(service.id, env.name, { wait_for_ci: !env.wait_for_ci }))} aria-pressed={!!env.wait_for_ci}>
              {env.wait_for_ci ? "Deploy at once" : "Wait for checks"}
            </button>
          </div>
        )}

        {service.source.kind === "git" && !ciReadable && (
          <div className="px-5 py-4">
            <p className="text-[13px] font-medium">Wait for CI</p>
            <p className="mt-1 text-[12px] text-muted">
              To deploy only after your tests pass, connect the service to roll's GitHub App, or give a GitLab or Bitbucket repository a Git login with a token (Settings), to wait for its checks here; or deploy from the workflow with roll's GitHub
              Action (<span className="font-mono">sdk/apps-github-action</span>, after the test job) and clear this environment's branch so a push doesn't deploy first.
            </p>
          </div>
        )}

        {service.source.kind === "image" && !service.source.image.includes("@") && (
          <div className="flex flex-wrap items-center justify-between gap-3 px-5 py-4">
            <div>
              <p className="text-[13px] font-medium">Follow the tag</p>
              <p className="mt-1 text-[12px] text-muted">
                {env.follow_tag
                  ? `When ${service.source.image} points at a new image, ${env.name} deploys it (checked every 5 minutes).`
                  : `${env.name} runs the digest it pinned until someone deploys; a push to the tag changes nothing.`}
              </p>
            </div>
            <button className={btn} disabled={busy} aria-pressed={!!env.follow_tag} onClick={() => runner.run("follow", () => appsApi.putEnvironment(service.id, env.name, { follow_tag: !env.follow_tag }))}>
              {env.follow_tag ? "Stop following" : "Follow the tag"}
            </button>
          </div>
        )}

        <div className="flex flex-wrap items-center justify-between gap-3 px-5 py-4">
          <div>
            <p className="text-[13px] font-medium">Redeploy when secrets change</p>
            <p className="mt-1 text-[12px] text-muted">
              {env.redeploy_on_secrets
                ? `When the project's ${env.name} secrets change (a rotated key), ${env.name} redeploys the same image with them, checked each minute.`
                : "A changed project secret waits for the next deploy; the Redeploy banner says when one did."}
            </p>
          </div>
          <button className={btn} disabled={busy} aria-pressed={!!env.redeploy_on_secrets} onClick={() => runner.run("secrets", () => appsApi.putEnvironment(service.id, env.name, { redeploy_on_secrets: !env.redeploy_on_secrets }))}>
            {env.redeploy_on_secrets ? "Stop redeploying" : "Redeploy on changes"}
          </button>
        </div>

        <div className="flex flex-wrap items-center justify-between gap-3 px-5 py-4">
          <div>
            <p className="text-[13px] font-medium">Traces</p>
            <p className="mt-1 text-[12px] text-muted">
              {env.traces
                ? "Its instances send OpenTelemetry traces to roll (OTEL_EXPORTER_OTLP_ENDPOINT and OTEL_SERVICE_NAME are set); they show on the Metrics tab."
                : "With an OpenTelemetry SDK in the app, turn this on and the next deploy sends its traces to roll, no other setup."}
            </p>
          </div>
          <button className={btn} disabled={busy} aria-pressed={!!env.traces} onClick={() => runner.run("traces", () => appsApi.putEnvironment(service.id, env.name, { traces: !env.traces }))}>
            {env.traces ? "Turn off" : "Turn on"}
          </button>
        </div>

        <div className="flex flex-wrap items-center justify-between gap-3 px-5 py-4">
          <div>
            <p className="text-[13px] font-medium">Hold deploys</p>
            <p className="mt-1 text-[12px] text-muted">
              {env.held
                ? "Pushes, deploy hooks and tag moves build without going live; you put one live from Deployments."
                : "Release on your own schedule: pushes still build, and wait for you to put them live."}
            </p>
          </div>
          <button className={btn} disabled={busy} aria-pressed={!!env.held} onClick={() => runner.run("hold", () => appsApi.putEnvironment(service.id, env.name, { held: !env.held }))}>
            {env.held ? "Resume deploys" : "Hold deploys"}
          </button>
        </div>

        <div className="flex flex-wrap items-center justify-between gap-3 px-5 py-4">
          <div className="min-w-0 flex-1">
            <p className="text-[13px] font-medium">Pause on a schedule</p>
            <p className="mt-1 text-[12px] text-muted">
              {env.pause_schedule
                ? `Paused ${env.pause_schedule.windows.map((w) => `${w.from} – ${w.to}`).join(", ")}${env.pause_schedule.timezone ? ` (${env.pause_schedule.timezone})` : " (UTC)"}; resumed as each ends. Pausing or resuming by hand holds until the window ends.`
                : "Nothing runs in the windows you set (a staging site at night): paused as one starts, resumed as it ends."}
            </p>
          </div>
          {env.pause_schedule ? (
            <button className={btn} disabled={busy} onClick={() => runner.run("pause-schedule", () => appsApi.putEnvironment(service.id, env.name, { pause_schedule: null }))}>
              Remove the schedule
            </button>
          ) : (
            <form
              className="flex gap-2"
              onSubmit={(e) => {
                e.preventDefault();
                void runner.run("pause-schedule", () => appsApi.putEnvironment(service.id, env.name, { pause_schedule: { windows: [{ from: pauseFrom.trim(), to: pauseTo.trim() }], timezone: Intl.DateTimeFormat().resolvedOptions().timeZone } }));
              }}
            >
              <input value={pauseFrom} onChange={(e) => setPauseFrom(e.target.value)} aria-label="From" className={`${input} w-24 font-mono text-[12px]`} />
              <input value={pauseTo} onChange={(e) => setPauseTo(e.target.value)} aria-label="To" className={`${input} w-24 font-mono text-[12px]`} />
              <button type="submit" className={btn} disabled={busy}>
                Pause then
              </button>
            </form>
          )}
        </div>

        <div className="flex flex-wrap items-center justify-between gap-3 px-5 py-4">
          <div className="min-w-0 flex-1">
            <p className="text-[13px] font-medium">Freeze deploys</p>
            <p className="mt-1 text-[12px] text-muted">
              {env.freeze
                ? `${env.frozen ? "Frozen now" : "Not frozen right now"}${env.freeze.reason ? `: ${env.freeze.reason}` : ""}${env.freeze.until ? `, until ${new Date(env.freeze.until).toLocaleString()}` : ""}${env.freeze.windows?.length ? `, ${env.freeze.windows.map((w) => `${w.from}–${w.to}`).join(", ")}` : ""}. A rollback always works.`
                : "A change window: pushes and hooks build without going live, deploying by hand needs an override, a rollback always works."}
            </p>
          </div>
          {env.freeze ? (
            <button className={btn} disabled={busy} onClick={() => runner.run("unfreeze", () => appsApi.putEnvironment(service.id, env.name, { freeze: null }))}>
              Lift the freeze
            </button>
          ) : (
            <form
              className="flex gap-2"
              onSubmit={(e) => {
                e.preventDefault();
                void runner.run("freeze", () => appsApi.putEnvironment(service.id, env.name, { freeze: freezeWhy.trim() ? { reason: freezeWhy.trim() } : {} })).then((r) => r && setFreezeWhy(""));
              }}
            >
              <input value={freezeWhy} onChange={(e) => setFreezeWhy(e.target.value)} placeholder="why (optional)" aria-label="Why" maxLength={200} className={`${input} w-44 text-[12px]`} />
              <button type="submit" className={btn} disabled={busy}>
                Freeze
              </button>
            </form>
          )}
        </div>

        <div className="flex flex-wrap items-center justify-between gap-3 px-5 py-4">
          <div>
            <p className="text-[13px] font-medium">Web Vitals</p>
            <p className="mt-1 text-[12px] text-muted">
              {env.vitals
                ? "Its pages carry roll's small script, which measures each page load in the visitor's browser (no cookie, no id); the Metrics tab shows the p75s."
                : "Measure how fast its pages are for real visitors: roll adds a small script to its HTML pages, nothing to install."}
            </p>
          </div>
          <button className={btn} disabled={busy} aria-pressed={!!env.vitals} onClick={() => runner.run("vitals", () => appsApi.putEnvironment(service.id, env.name, { vitals: !env.vitals }))}>
            {env.vitals ? "Turn off" : "Turn on"}
          </button>
        </div>

        <SnippetsRow service={service.id} env={env} runner={runner} busy={busy} />
        <ConfigRow service={service.id} env={env} runner={runner} busy={busy} />

        {(service.source.kind === "repo" || service.source.kind === "git") && (
          <div className="flex flex-wrap items-center justify-between gap-3 px-5 py-4">
            <div>
              <p className="text-[13px] font-medium">Report deploys</p>
              <p className="mt-1 text-[12px] text-muted">
                {env.report !== false
                  ? `Each deploy posts a commit status (roll/apps/${service.name}/${env.name})${service.source.kind === "git" ? ", and on GitHub a Deployment," : ""} on its commit.`
                  : "Its deploys post nothing to the Git host: something else reports them (roll previews does for its own environments)."}
              </p>
            </div>
            <button className={btn} disabled={busy} aria-pressed={env.report !== false} onClick={() => runner.run("report", () => appsApi.putEnvironment(service.id, env.name, { report: env.report === false }))}>
              {env.report !== false ? "Turn off" : "Turn on"}
            </button>
          </div>
        )}

        <div className="space-y-3 px-5 py-4">
          <div className="flex flex-wrap items-center justify-between gap-3">
            <div>
              <p className="text-[13px] font-medium">Outbound traffic</p>
              <p className="mt-1 text-[12px] text-muted">
                {closed
                  ? `From its next deploy, its code reaches roll (linked databases, the project's services) and only the hosts below, through roll's egress proxy.`
                  : "Its code reaches anything on the internet. Limit it to roll and the hosts you list, as roll previews does for a fork's preview."}
              </p>
            </div>
            <button
              className={btn}
              disabled={busy}
              aria-pressed={closed}
              onClick={() => runner.run("egress", () => appsApi.putEnvironment(service.id, env.name, { egress: closed ? null : { mode: "roll", hosts: hostList } }))}
            >
              {closed ? "Open it" : "Limit to roll"}
            </button>
          </div>
          {closed && (
            <div className="flex flex-wrap items-end gap-2">
              <Field label="Hosts it may reach (api.stripe.com, *.sentry.io)">
                <textarea className={`${input} h-16 w-full max-w-sm font-mono text-[12px]`} value={egressHosts} onChange={(e) => setEgressHosts(e.target.value)} spellCheck={false} />
              </Field>
              <button className={btn} disabled={busy || egressHosts === egressText} onClick={() => runner.run("egress", () => appsApi.putEnvironment(service.id, env.name, { egress: { mode: "roll", hosts: hostList } }))}>
                Save
              </button>
            </div>
          )}
          {closed && !!env.egress_refused?.length && (
            <p className="text-[12px] text-muted">
              Refused lately: <span className="font-mono">{env.egress_refused.map((r) => (r.private ? `${r.host} (private address)` : r.host)).join(", ")}</span>
            </p>
          )}
        </div>

        <div className="space-y-3 px-5 py-4">
          <div className="flex flex-wrap items-center justify-between gap-3">
            <div>
              <p className="text-[13px] font-medium">Deploy hook</p>
              <p className="mt-1 text-[12px] text-muted">
                {service.source.kind === "git"
                  ? env.hook
                    ? `Add its URL as the repository's push webhook (GitHub: Settings → Webhooks, content type application/json, push and pull request events; GitLab: Webhooks, push and merge request events): a push to ${env.branch ?? "its branch"} deploys ${env.name} at once instead of within five minutes, and pull requests get previews.`
                    : "A secret URL for the repository's push webhook: pushes deploy at once instead of within five minutes."
                  : env.hook
                    ? `A POST to its URL deploys ${env.name}, from CI or a registry's webhook${service.source.kind === "image" ? " (?image= another tag of the same image)" : ""}.`
                    : "A secret URL that deploys this environment, without an API token."}
              </p>
            </div>
            <div className="flex gap-2">
              {env.hook && (
                <SureButton className={btn} disabled={busy} sure="Sure? It stops working" onSure={() => runner.run("hook", () => appsApi.removeHook(service.id, env.name)).then(() => setHook(null))}>
                  Remove
                </SureButton>
              )}
              <button
                className={btn}
                disabled={busy}
                title={env.hook ? "A new URL; the old one stops working" : undefined}
                onClick={async () => {
                  const url = await runner.run("hook", () => appsApi.createHook(service.id, env.name));
                  if (typeof url === "string") setHook(url);
                }}
              >
                {env.hook ? "New URL" : "Create"}
              </button>
            </div>
          </div>
          {hook && (
            <div className="rise rounded-lg border border-line bg-bg px-3 py-2">
              <p className="text-[11px] text-subtle">The hook's URL, shown once (only its hash is kept):</p>
              <div className="mt-1 flex items-center gap-1">
                <code className="min-w-0 flex-1 truncate font-mono text-[11.5px]">{hook}</code>
                <CopyButton id="hook-url" text={hook} copied={copied} onCopy={copy} />
              </div>
            </div>
          )}
        </div>

        <div className="flex flex-wrap items-end gap-3 px-5 py-4">
          <div className="min-w-56 flex-1">
            <p className="text-[13px] font-medium">Promote</p>
            <p className="mt-1 text-[12px] text-muted">
              {sources.length ? (
                <>Deploy what another environment runs here: its image, no build, with {env.name}'s variables.</>
              ) : (
                <>Another environment with a live deployment can be promoted here.</>
              )}
            </p>
          </div>
          {sources.length > 0 && (
            <>
              <select value={from} onChange={(e) => setFrom(e.target.value)} className={`${input} font-mono text-[12px]`} aria-label="Environment to promote">
                {sources.map((e) => (
                  <option key={e.name} value={e.name}>
                    {e.name}
                  </option>
                ))}
              </select>
              <button className={btn} disabled={!from || busy || promoting !== null} onClick={() => void runner.run("promote-look", look)}>
                <Rocket className="size-3.5" /> Promote {from}…
              </button>
            </>
          )}
        </div>
        {promoting && (
          <div className="rise mx-5 mb-4 space-y-2 rounded-lg border border-line bg-bg px-3 py-3 text-[13px]">
            <p>
              Put <span className="font-mono">{promoting.deployment}</span>
              {promoting.revision ? <span className="font-mono text-muted"> ({shortSha(promoting.revision)})</span> : null} from {promoting.from} live in{" "}
              <span className="font-mono">{env.name}</span>, with {env.name}'s variables and no build?
            </p>
            {promoting.missing.length > 0 && (
              <p className="text-[12px] text-danger">
                {env.name} lacks {promoting.missing.length === 1 ? "a variable" : "variables"} {promoting.from} has: <span className="font-mono">{promoting.missing.join(", ")}</span>.
              </p>
            )}
            <div className="flex gap-2">
              <button
                type="button"
                className={primary}
                disabled={busy}
                onClick={() =>
                  void runner.run("promote", () => appsApi.promote(service.id, env.name, promoting.from, promoting.deployment)).then((d) => d && setPromoting(null))
                }
              >
                {runner.busy === "promote" ? <span className="shimmer">Promoting…</span> : "Promote"}
              </button>
              <button type="button" className={btn} onClick={() => setPromoting(null)}>
                Cancel
              </button>
            </div>
          </div>
        )}

        <CompareSection service={service} env={env} runner={runner} />

        <p className="px-5 pb-2 pt-5 text-[11px] font-medium uppercase tracking-[0.06em] text-subtle">Who gets in</p>

        <div className="space-y-3 px-5 py-4">
          <div className="flex flex-wrap items-center justify-between gap-3">
            <div>
              <p className="text-[13px] font-medium">{env.signed_in ? "Signed in to roll" : env.password ? "Password" : env.private ? "Private" : "Public"}</p>
              <p className="mt-1 text-[12px] text-muted">
                {env.signed_in
                  ? "Only people signed in to roll who can open this project get in: anyone else is sent to sign in first, before anything wakes."
                  : env.password
                    ? "Visitors type its password on roll's page (a session for seven days); nothing wakes before they do."
                    : env.private
                    ? "Only visitors with its link get in; everyone else gets a 401 before anything wakes."
                    : "Anyone with its URL can open it."}
              </p>
            </div>
            <div className="flex gap-2">
              {!env.signed_in && (
                <button className={btn} disabled={busy} title="People signed in to roll who can open this project, instead of a link" onClick={() => runner.run("access", () => appsApi.putEnvironment(service.id, env.name, { access: { signed_in: true } })).then(() => setLink(null))}>
                  Require sign-in
                </button>
              )}
              {env.private && !env.signed_in && (
                <button className={btn} disabled={busy} onClick={makePrivate} title="A new link; the old one stops working">
                  New link
                </button>
              )}
              <button
                className={btn}
                disabled={busy}
                onClick={() => (env.private ? runner.run("access", () => appsApi.putEnvironment(service.id, env.name, { access: null })).then(() => setLink(null)) : makePrivate())}
              >
                {env.private ? <Unlock className="size-3.5" /> : <Lock className="size-3.5" />} {env.private ? "Make public" : "Make private"}
              </button>
            </div>
          </div>
          {!env.signed_in && (
            <form
              className="flex flex-wrap items-center gap-2"
              onSubmit={(e) => {
                e.preventDefault();
                void runner.run("access", () => appsApi.putEnvironment(service.id, env.name, { access: { password } })).then((r) => {
                  if (!r) return;
                  setPassword("");
                  setLink(null);
                });
              }}
            >
              <input type="password" value={password} onChange={(e) => setPassword(e.target.value)} placeholder={env.password ? "a new password (ends every session)" : "a password visitors type (8 or more)"} aria-label="Password" autoComplete="new-password" className={`${input} min-w-48 flex-1 text-[12px]`} />
              <button type="submit" className={btn} disabled={busy || password.length < 8}>
                {env.password ? "Change password" : "Use a password"}
              </button>
            </form>
          )}
          {link && (
            <div className="rise rounded-lg border border-line bg-bg px-3 py-2">
              <p className="text-[11px] text-subtle">The private link, shown once (only its hash is kept):</p>
              <div className="mt-1 flex items-center gap-1">
                <code className="min-w-0 flex-1 truncate font-mono text-[11.5px]">{link}</code>
                <CopyButton id="access-link" text={link} copied={copied} onCopy={copy} />
              </div>
            </div>
          )}
        </div>

        {service.run.type === "web" && service.run.public !== false && (
          <div className="px-5 py-4">
            <div className="flex flex-wrap items-center justify-between gap-3">
              <div>
                <p className="text-[13px] font-medium">Allowed networks</p>
                <p className="mt-1 text-[12px] text-muted">
                  {env.allow_ips?.length
                    ? `Only ${env.allow_ips.length === 1 ? "this network gets" : `these ${env.allow_ips.length} networks get`} in through its public URLs; everyone else gets a 403. The private network is unaffected.`
                    : "Anyone with its URL gets in. List networks to let only them in."}
                </p>
              </div>
              <button className={btn} disabled={busy || allow === allowText} onClick={() => runner.run("allow", () => appsApi.putEnvironment(service.id, env.name, { allow_ips: parseAllowList(allow) }))}>
                Save
              </button>
            </div>
            <textarea
              value={allow}
              onChange={(e) => setAllow(e.target.value)}
              rows={Math.min(6, Math.max(2, allow.split("\n").length))}
              placeholder={"203.0.113.0/24 office\n2001:db8::/32"}
              spellCheck={false}
              aria-label="Allowed networks, one per line"
              className={`${input} mt-2 w-full font-mono text-[12px]`}
            />
          </div>
        )}

        {service.run.type === "web" && service.run.public !== false && !service.run.tcp && (
          <div className="px-5 py-4">
            <div className="flex flex-wrap items-center justify-between gap-3">
              <div>
                <p className="text-[13px] font-medium">Firewall</p>
                <p className="mt-1 text-[12px] text-muted">
                  {env.firewall?.length
                    ? `${env.firewall.length} ${env.firewall.length === 1 ? "rule" : "rules"}, in order: the first that matches a request decides.`
                    : "No rules. One a line: deny /admin/*, deny method=DELETE, allow /admin/health, deny ua=BadBot, limit 10 /login method=POST (10 a minute a client), challenge /* (a browser check)."}
                </p>
              </div>
              <button className={btn} disabled={busy || fw === fwText} onClick={() => runner.run("firewall", () => appsApi.putEnvironment(service.id, env.name, { firewall: parseFirewall(fw) }))}>
                Save
              </button>
            </div>
            <textarea
              value={fw}
              onChange={(e) => setFw(e.target.value)}
              rows={Math.min(8, Math.max(2, fw.split("\n").length))}
              placeholder={"allow /admin/health\ndeny /admin/*"}
              spellCheck={false}
              aria-label="Firewall rules, one a line"
              className={`${input} mt-2 w-full font-mono text-[12px]`}
            />
          </div>
        )}

        {service.run.type === "web" && !service.run.tcp && (
          <div className="px-5 py-4">
            <div className="flex flex-wrap items-center justify-between gap-3">
              <div>
                <p className="text-[13px] font-medium">Response headers</p>
                <p className="mt-1 text-[12px] text-muted">
                  One a line, on every answer or a path's: <code className="font-mono">/api/* Cache-Control: no-store</code>. The app's own header wins unless the line starts with <code className="font-mono">!</code>.
                </p>
              </div>
              <button className={btn} disabled={busy || hdr === hdrText} onClick={() => runner.run("headers", () => appsApi.putEnvironment(service.id, env.name, { headers: parseHeaders(hdr) }))}>
                Save
              </button>
            </div>
            <textarea
              value={hdr}
              onChange={(e) => setHdr(e.target.value)}
              rows={Math.min(8, Math.max(2, hdr.split("\n").length))}
              placeholder={"Strict-Transport-Security: max-age=63072000\nX-Frame-Options: DENY"}
              spellCheck={false}
              aria-label="Response headers, one a line"
              className={`${input} mt-2 w-full font-mono text-[12px]`}
            />
          </div>
        )}

        {service.run.type === "web" && !service.run.tcp && (
          <div className="px-5 py-4">
            <div className="flex flex-wrap items-center justify-between gap-3">
              <div>
                <p className="text-[13px] font-medium">Routes to other services</p>
                <p className="mt-1 text-[12px] text-muted">
                  One a line, a path and a service of the project: <code className="font-mono">/api/* api</code> sends those paths to its {env.name} environment, as they came. The first that matches decides.
                </p>
              </div>
              <button className={btn} disabled={busy || routes === routeText} onClick={() => runner.run("routes", () => appsApi.putEnvironment(service.id, env.name, { routes: parseRoutes(routes) }))}>
                Save
              </button>
            </div>
            <textarea
              value={routes}
              onChange={(e) => setRoutes(e.target.value)}
              rows={Math.min(8, Math.max(2, routes.split("\n").length))}
              placeholder={"/api/* api\n/auth/* auth"}
              spellCheck={false}
              aria-label="Routes to other services, one a line"
              className={`${input} mt-2 w-full font-mono text-[12px]`}
            />
          </div>
        )}

        {service.run.type === "web" && service.run.public !== false && (
          <div className="flex flex-wrap items-center justify-between gap-3 px-5 py-4">
            <div>
              <p className="text-[13px] font-medium">Rate limit</p>
              <p className="mt-1 text-[12px] text-muted">
                {env.rate_limit
                  ? `Each visitor's address gets ${env.rate_limit.per_minute} requests a minute (${env.rate_limit.burst ?? env.rate_limit.per_minute} at once); more get a 429 that says when to come back.`
                  : "No limit per visitor."}
              </p>
            </div>
            <div className="flex items-center gap-2">
              <input
                value={rate}
                onChange={(e) => setRate(e.target.value.replace(/[^0-9]/g, ""))}
                placeholder="per minute"
                aria-label="Requests a minute per visitor"
                className={`${input} w-28 font-mono text-[12px]`}
              />
              <button className={btn} disabled={busy || rate === rateText} onClick={() => runner.run("rate", () => appsApi.putEnvironment(service.id, env.name, { rate_limit: rate ? { per_minute: Number(rate) } : null }))}>
                {rate ? "Save" : "Remove"}
              </button>
            </div>
          </div>
        )}

        {service.run.type === "web" && (
          <div className="flex flex-wrap items-center justify-between gap-3 px-5 py-4">
            <div className="min-w-0 flex-1">
              <p className="text-[13px] font-medium">Maintenance</p>
              <p className="mt-1 text-[12px] text-muted">
                {env.maintenance
                  ? `Visitors get roll's maintenance page${env.maintenance.message ? " with your note" : ""}; nothing wakes. Deployment URLs still answer.`
                  : "Serving. In maintenance its public names answer a maintenance page instead, and nothing wakes."}
              </p>
              {!env.maintenance && (
                <input value={notice} onChange={(e) => setNotice(e.target.value.slice(0, 500))} placeholder="What visitors read (optional)" className={`${input} mt-2 w-full text-[12px]`} aria-label="Maintenance note" />
              )}
            </div>
            <button
              className={env.maintenance ? primary : btn}
              disabled={busy}
              aria-pressed={!!env.maintenance}
              onClick={() => runner.run("maintenance", () => appsApi.putEnvironment(service.id, env.name, { maintenance: env.maintenance ? false : { message: notice.trim() || null } }))}
            >
              {env.maintenance ? "End maintenance" : "Start maintenance"}
            </button>
          </div>
        )}

        {service.run.type === "web" && (
          <div className="flex flex-wrap items-center justify-between gap-3 px-5 py-4">
            <div className="min-w-0 flex-1">
              <p className="text-[13px] font-medium">Your pages</p>
              <p className="mt-1 text-[12px] text-muted">Your own HTML instead of roll's, for while the app can't answer or wake, and for maintenance. At most 64 KiB each, served as it is.</p>
            </div>
            <div className="flex flex-wrap gap-2">
              {(["error", "maintenance"] as const).map((k) => (
                <span key={k} className="inline-flex items-center gap-1">
                  <label className={`${btn} cursor-pointer`}>
                    {env.pages?.[k] ? `Replace ${k} page` : `${k === "error" ? "Error" : "Maintenance"} page…`}
                    <input
                      type="file"
                      accept=".html,text/html"
                      className="sr-only"
                      disabled={busy}
                      onChange={async (e) => {
                        const f = e.target.files?.[0];
                        e.target.value = "";
                        if (f) await runner.run(`page:${k}`, async () => appsApi.putEnvironment(service.id, env.name, { pages: { [k]: await f.text() } }));
                      }}
                    />
                  </label>
                  {env.pages?.[k] && (
                    <button className="p-1 text-[12px] text-subtle hover:text-fg" disabled={busy} onClick={() => runner.run(`page:${k}`, () => appsApi.putEnvironment(service.id, env.name, { pages: { [k]: null } }))}>
                      Use roll's
                    </button>
                  )}
                </span>
              ))}
            </div>
          </div>
        )}

        <p className="px-5 pb-2 pt-5 text-[11px] font-medium uppercase tracking-[0.06em] text-subtle">Scale</p>

        {env.scaling && (
          <div className="px-5 py-4">
            <p className="text-[13px] font-medium">Following {service.run.scale?.group ?? "its work"}</p>
            <p className="mt-1 text-[12px] text-muted">
              {env.scaling.backlog == null ? "Its backlog hasn't been read yet." : `${env.scaling.backlog.toLocaleString()} messages behind`}: {env.scaling.wanted} of {service.run.max_instances} instances
              wanted, read every 10 s.
            </p>
          </div>
        )}

        <OwnRun service={service} env={env} runner={runner} />

        {service.run.volume && <Backups service={service} env={env} runner={runner} />}

        {env.name !== PRODUCTION && (
          <div className="px-5 py-4">
            {deleting ? (
              <Confirm
                word={env.name}
                action="Delete environment"
                busy={runner.busy === "delete-env"}
                onCancel={() => setDeleting(false)}
                onConfirm={async () => {
                  if (await runner.run("delete-env", () => appsApi.removeEnvironment(service.id, env.name).then(() => true))) onDeleted();
                }}
              >
                Stops its instances and removes its domains, variables and volume. Its deployments stay in the history.
              </Confirm>
            ) : (
              <div className="flex flex-wrap items-center justify-between gap-3">
                <p className="text-[13px] text-muted">Tear {env.name} down.</p>
                <button className={danger} disabled={busy} onClick={() => setDeleting(true)}>
                  <Trash2 className="size-3.5" /> Delete environment
                </button>
              </div>
            )}
          </div>
        )}
      </div>
    </Panel>
  );
}

const OWN: { key: EnvRunField; label: string; web?: boolean }[] = [
  { key: "min_instances", label: "Min instances" },
  { key: "max_instances", label: "Max instances" },
  { key: "cpu", label: "CPU" },
  { key: "memory_mb", label: "Memory" },
  { key: "sleep_after_s", label: "Sleep after (s)", web: true },
];

/** The environment's own size and scale (Railway's per-environment settings): each blank one is the service's. */
function OwnRun({ service, env, runner }: { service: Service; env: Environment; runner: Runner }) {
  const web = service.run.type === "web";
  const fields = OWN.filter((f) => web || !f.web);
  const saved = (k: EnvRunField) => (env.run?.[k] == null ? "" : String(env.run[k]));
  const [own, setOwn] = useState<Record<string, string>>(() => Object.fromEntries(fields.map((f) => [f.key, saved(f.key)])));
  const changed = fields.filter((f) => (own[f.key] ?? "") !== saved(f.key));
  const busy = runner.busy !== null;
  const ofService = (k: EnvRunField) => {
    const v = service.run[k as keyof typeof service.run] as number | undefined;
    return v == null ? "" : k === "memory_mb" ? mb(v) : k === "cpu" ? `${v} vCPU` : String(v);
  };
  const select = (k: EnvRunField, options: number[], show: (n: number) => string) => (
    <select value={own[k] ?? ""} onChange={(e) => setOwn((o) => ({ ...o, [k]: e.target.value }))} className={`${input} w-full`}>
      <option value="">service's ({ofService(k)})</option>
      {options.map((n) => (
        <option key={n} value={String(n)}>
          {show(n)}
        </option>
      ))}
    </select>
  );
  return (
    <form
      className="space-y-3 px-5 py-4"
      onSubmit={(e) => {
        e.preventDefault();
        const run = Object.fromEntries(changed.map((f) => [f.key, own[f.key]?.trim() ? Number(own[f.key]) : null]));
        void runner.run("own-run", () => appsApi.putEnvironment(service.id, env.name, { run }));
      }}
    >
      <div>
        <p className="text-[13px] font-medium">Its own size</p>
        <p className="mt-1 text-[12px] text-muted">Over the service's settings, for {env.name} only (staging small, production warm). Blank is the service's; it applies with the next deploy.</p>
      </div>
      <div className="grid grid-cols-2 gap-3 sm:grid-cols-5">
        {fields.map((f) => (
          <Field key={f.key} label={f.label}>
            {f.key === "cpu" ? (
              select("cpu", CPU, (n) => `${n} vCPU`)
            ) : f.key === "memory_mb" ? (
              select("memory_mb", MEMORY, mb)
            ) : (
              <input
                value={own[f.key] ?? ""}
                onChange={(e) => setOwn((o) => ({ ...o, [f.key]: e.target.value.replace(/\D/g, "") }))}
                placeholder={ofService(f.key)}
                inputMode="numeric"
                className={`${input} w-full`}
              />
            )}
          </Field>
        ))}
      </div>
      {changed.length > 0 && (
        <div className="flex justify-end">
          <button type="submit" className={primary} disabled={busy}>
            {runner.busy === "own-run" ? <span className="shimmer">Saving…</span> : "Save"}
          </button>
        </div>
      )}
    </form>
  );
}

const size = (n: number) => (n >= 1 << 30 ? `${(n / (1 << 30)).toFixed(1)} GB` : n >= 1 << 20 ? `${(n / (1 << 20)).toFixed(1)} MB` : `${Math.max(1, Math.round(n / 1024))} KB`);

/** The volume's backups (daily, the newest seven kept): back up now, or restore one after typing the environment's name. */
/** How full the volume was when last measured: a hairline meter, the danger colour from 90% (`volume.full`). */
function VolumeMeter({ used, sizeGb }: { used: { bytes: number; at: number }; sizeGb: number }) {
  const pct = Math.min(100, Math.round((used.bytes / (sizeGb * 2 ** 30)) * 100));
  return (
    <div className="mt-2 w-64 max-w-full">
      <div className="h-1 overflow-hidden rounded-full bg-fg/[0.08]" role="meter" aria-valuemin={0} aria-valuemax={100} aria-valuenow={pct} aria-label="Volume used">
        <div className={`h-full rounded-full ${pct >= 90 ? "bg-danger" : "bg-fg/40"}`} style={{ width: `${pct}%` }} />
      </div>
      <p className={`mt-1 text-[11.5px] ${pct >= 90 ? "text-danger" : "text-subtle"}`}>
        {size(used.bytes)} of {sizeGb} GB ({pct}%), measured {since(used.at, Date.now())}
      </p>
    </div>
  );
}

function Backups({ service, env, runner }: { service: Service; env: Environment; runner: Runner }) {
  const [list, setList] = useState<Backup[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [restoring, setRestoring] = useState<Backup | null>(null);
  // Whose backups are listed: this environment's, or another's to seed it from.
  const [of, setOf] = useState(env.name);
  const busy = runner.busy !== null;
  const load = useCallback(() => {
    appsApi.backups(service.id, of).then(
      (l) => {
        setList(l);
        setError(null);
      },
      (e: unknown) => setError(e instanceof Error ? e.message : String(e)),
    );
  }, [service.id, of]);
  const schedule = service.run.volume?.backups ?? { every_hours: 24, keep: 7 };
  const others = service.environments.filter((e) => e.name !== env.name);
  useEffect(load, [load]);
  return (
    <div className="space-y-3 px-5 py-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <p className="text-[13px] font-medium">Volume backups</p>
          <p className="mt-1 text-[12px] text-muted">
            {service.run.volume!.mount} is copied to object storage{" "}
            {schedule.every_hours === 0 ? "when asked" : schedule.every_hours === 24 ? "once a day" : `every ${schedule.every_hours} hours`}; the newest {schedule.keep} are kept.
          </p>
          {env.volume_used && <VolumeMeter used={env.volume_used} sizeGb={service.run.volume!.size_gb} />}
        </div>
        <span className="flex flex-wrap items-center gap-2">
          {others.length > 0 && (
            <select value={of} onChange={(e) => setOf(e.target.value)} className={`${input} font-mono text-[12px]`} aria-label="Whose backups">
              <option value={env.name}>{env.name}'s backups</option>
              {others.map((e) => (
                <option key={e.name} value={e.name}>
                  {e.name}'s backups
                </option>
              ))}
            </select>
          )}
          <button className={btn} disabled={busy} onClick={() => runner.run("backup", () => appsApi.backUp(service.id, env.name)).then(load)}>
            {runner.busy === "backup" ? <span className="shimmer">Backing up…</span> : "Back up now"}
          </button>
        </span>
      </div>
      {error ? (
        <p className="text-[12px] text-muted">{error}</p>
      ) : list === null ? null : list.length === 0 ? (
        <p className="text-[12px] text-subtle">No backups yet.</p>
      ) : (
        <ul className="divide-y divide-line rounded-lg border border-line">
          {list.map((b) => (
            <li key={b.id} className="flex items-center gap-3 px-3 py-2 text-[12.5px]">
              <span className="flex-1">{new Date(b.at).toLocaleString()}</span>
              <span className="font-mono text-[12px] text-subtle">{size(b.size)}</span>
              <button className={btn} disabled={busy} onClick={() => setRestoring(b)}>
                Restore
              </button>
            </li>
          ))}
        </ul>
      )}
      {restoring && (
        <Confirm
          word={env.name}
          action="Restore"
          busy={runner.busy === "restore"}
          onCancel={() => setRestoring(null)}
          onConfirm={() =>
            runner.run("restore", () => appsApi.restore(service.id, env.name, restoring.id, of === env.name ? undefined : of)).then((ok) => {
              if (ok) setRestoring(null);
            })
          }
        >
          {env.name}'s volume becomes {of === env.name ? "what it was" : `${of}'s as it was`} on {new Date(restoring.at).toLocaleString()}: what's on it now is replaced (backed up first). It sleeps meanwhile and wakes on the next request. Type <span className="font-mono text-fg">{env.name}</span> to restore.
        </Confirm>
      )}
    </div>
  );
}


/** Its own HTML in every page (an analytics, consent or chat tag): one box before `</head>`, one
 *  before `</body>`, saved together. */
function SnippetsRow({ service, env, runner, busy }: { service: string; env: Environment; runner: Runner; busy: boolean }) {
  const of = (at: Snippet["at"]) => (env.snippets ?? []).filter((s) => s.at === at).map((s) => s.html).join("\n");
  const [head, setHead] = useState(() => of("head"));
  const [body, setBody] = useState(() => of("body_end"));
  const changed = head !== of("head") || body !== of("body_end");
  const save = () => {
    const snippets: Snippet[] = [...(head.trim() ? [{ at: "head" as const, html: head }] : []), ...(body.trim() ? [{ at: "body_end" as const, html: body }] : [])];
    void runner.run("snippets", () => appsApi.putEnvironment(service, env.name, { snippets: snippets.length ? snippets : null }));
  };
  return (
    <div className="space-y-2 px-5 py-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <p className="text-[13px] font-medium">Snippets</p>
          <p className="mt-1 text-[12px] text-muted">Its own HTML in every page roll serves for it, without a deploy: an analytics or chat tag.</p>
        </div>
        <button className={btn} disabled={busy || !changed} onClick={save}>
          Save snippets
        </button>
      </div>
      <textarea value={head} onChange={(e) => setHead(e.target.value)} rows={2} aria-label="Before </head>" placeholder="Before </head>: <script defer src=…></script>" className={`${input} w-full font-mono text-[12px]`} spellCheck={false} />
      <textarea value={body} onChange={(e) => setBody(e.target.value)} rows={2} aria-label="Before </body>" placeholder="Before </body>" className={`${input} w-full font-mono text-[12px]`} spellCheck={false} />
    </div>
  );
}

/** Its runtime config as JSON: saved as a whole; its instances have it at their next read (the
 *  first one needs a deploy, to give them `ROLL_CONFIG_URL`). */
function ConfigRow({ service, env, runner, busy }: { service: string; env: Environment; runner: Runner; busy: boolean }) {
  const shown = env.config ? JSON.stringify(env.config, null, 2) : "";
  const [text, setText] = useState(shown);
  let parsed: Record<string, unknown> | null = null;
  let bad: string | null = null;
  if (text.trim()) {
    try {
      const v = JSON.parse(text);
      if (v && typeof v === "object" && !Array.isArray(v)) parsed = v;
      else bad = "an object of keys and values";
    } catch (e) {
      bad = e instanceof Error ? e.message : "not JSON";
    }
  }
  const save = () => void runner.run("config", () => appsApi.putEnvironment(service, env.name, { config: parsed }));
  return (
    <div className="space-y-2 px-5 py-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <p className="text-[13px] font-medium">Runtime config{env.config_version ? <span className="ml-2 font-mono text-[11.5px] text-subtle">v{env.config_version}</span> : null}</p>
          <p className="mt-1 text-[12px] text-muted">Values its instances read at <code className="font-mono">$ROLL_CONFIG_URL</code> while they run (a flag, a banner): a change needs no deploy.</p>
        </div>
        <div className="flex gap-2">
          {env.config_version != null && (
            <SureButton
              className={btn}
              disabled={busy}
              sure="Sure? Redeploy to finish"
              onSure={() => void runner.run("config-rotate", () => appsApi.rotateConfig(service, env.name))}
              title="A new address, if the old one leaked: the old one keeps working until a deploy made after this is live"
            >
              Rotate address
            </SureButton>
          )}
          <button className={btn} disabled={busy || text === shown || bad !== null} onClick={save}>
            Save config
          </button>
        </div>
      </div>
      {env.config_rotated_at != null && (
        <p className="flex flex-wrap items-center gap-2 text-[11.5px] text-subtle">
          Address rotated {since(env.config_rotated_at, Date.now())}: the one before works until a deploy made after that is live.
          {env.active && env.active.created_at < env.config_rotated_at && (
            <button type="button" className="text-accent hover:underline disabled:opacity-50" disabled={busy} onClick={() => void runner.run("redeploy", () => appsApi.redeploy(service, env.name, { message: "the runtime config's new address" }))}>
              Redeploy now
            </button>
          )}
        </p>
      )}
      <textarea value={text} onChange={(e) => setText(e.target.value)} rows={4} aria-label="Runtime config (JSON)" placeholder='{ "new_checkout": true }' className={`${input} w-full font-mono text-[12px]`} spellCheck={false} />
      {bad && <p className="text-[12px] text-danger">{bad}</p>}
    </div>
  );
}
