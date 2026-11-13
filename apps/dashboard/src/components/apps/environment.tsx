import { Lock, Rocket, Trash2, Unlock } from "lucide-react";
import { useState } from "react";
import { appsApi, PRODUCTION, type Environment, type EnvRunField, type Service } from "@/lib/apps-api";
import { Field } from "../redis/shared";
import { btn, Confirm, CopyButton, danger, input, Panel, primary, useCopy } from "../ui";
import { CPU, MEMORY, mb, type Runner } from "./shared";

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

/**
 * The environment shown, its own settings: the branch that deploys to it, promoting another
 * environment's live image to it, private access, and removing it (production stays).
 */
export function EnvironmentPanel({ service, env, runner, onDeleted }: { service: Service; env: Environment; runner: Runner; onDeleted: () => void }) {
  const [branch, setBranch] = useState(env.branch ?? "");
  const sources = service.environments.filter((e) => e.name !== env.name && e.active);
  const [from, setFrom] = useState(sources[0]?.name ?? "");
  const [link, setLink] = useState<string | null>(null);
  const [deleting, setDeleting] = useState(false);
  const { copied, copy } = useCopy();
  const repo = service.source.kind === "repo";
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
        {repo && (
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

        {repo && (
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

        <OwnRun service={service} env={env} runner={runner} />

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
              <button className={btn} disabled={!from || busy} onClick={() => runner.run("promote", () => appsApi.promote(service.id, env.name, from))}>
                <Rocket className="size-3.5" /> {runner.busy === "promote" ? <span className="shimmer">Promoting…</span> : `Promote ${from}`}
              </button>
            </>
          )}
        </div>

        <div className="space-y-3 px-5 py-4">
          <div className="flex flex-wrap items-center justify-between gap-3">
            <div>
              <p className="text-[13px] font-medium">{env.private ? "Private" : "Public"}</p>
              <p className="mt-1 text-[12px] text-muted">
                {env.private ? "Only visitors with its link get in; everyone else gets a 401 before anything wakes." : "Anyone with its URL can open it."}
              </p>
            </div>
            <div className="flex gap-2">
              {env.private && (
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

