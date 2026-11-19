import { RotateCw, Scissors, ShieldAlert, ShieldCheck, Snowflake } from "lucide-react";
import { useState, type ReactNode } from "react";
import { canRotate, type CutOff, type Environment, type Holder, type Link, type Reach } from "@/lib/secrets-api";
import { Dialog } from "../agents/ui/dialog";
import { useSecrets } from "./secrets-page";
import { ActorMark, ago, button, dangerButton, ErrorNote, errorText, field, ghost, label, until, useLoad } from "./ui";

type Step = { kind: "reach" } | { kind: "confirm" } | { kind: "done"; result: CutOff };

/**
 * Reach (docs/secrets.md, "Reach"): who and what can read a key's value — tokens and identities
 * whose scope covers it, untrusted code, syncs, leases, shares, projects and folders it flows to —
 * and who read it in 30 days. Names only, a project admin's. "Cut off…" cuts all of it at once
 * (an incident), after a confirmation that lists exactly what goes; then the key is rotated. A
 * token that reaches beyond the project (`skipped`) only the operator revokes: it's listed apart.
 */
export function ReachDialog({ open, onClose, env, path, keyName, link }: { open: boolean; onClose: () => void; env: Environment; path: string; keyName: string; link?: Link | null }) {
  const [step, setStep] = useState<Step>({ kind: "reach" });
  const [busy, setBusy] = useState(false);
  const title = step.kind === "reach" ? `Who can read ${keyName}` : step.kind === "confirm" ? `Cut off ${keyName}` : `Cut off ${keyName}: done`;
  const lead =
    step.kind === "reach"
      ? `In ${env.name}${path === "/" ? "" : ` ${path}`}: credentials that reach it, where it goes by itself, and who read it lately. Names only.`
      : step.kind === "confirm"
        ? "For an incident: everything below loses access now. Revoked tokens don't come back; identities, syncs and a freeze can be restored after you rotate."
        : undefined;
  return (
    <Dialog open={open} onClose={onClose} onExited={() => setStep({ kind: "reach" })} busy={busy} title={title} lead={lead} className="w-[min(640px,100%)]">
      {open && <Body env={env} path={path} keyName={keyName} link={link ?? null} step={step} setStep={setStep} setBusy={setBusy} onClose={onClose} />}
    </Dialog>
  );
}

function Body({ env, path, keyName, link, step, setStep, setBusy, onClose }: { env: Environment; path: string; keyName: string; link: Link | null; step: Step; setStep: (s: Step) => void; setBusy: (b: boolean) => void; onClose: () => void }) {
  const { api } = useSecrets();
  const reach = useLoad(() => api.reach(env.slug, path, keyName), [api, env.slug, path, keyName]);
  const r = reach.data;
  if (reach.error) return <ErrorNote error={reach.error} />;
  if (!r) return <p className="text-[12px] text-subtle">Looking…</p>;
  if (step.kind === "confirm") return <Confirm env={env} path={path} keyName={keyName} r={r} setBusy={setBusy} onBack={() => setStep({ kind: "reach" })} onDone={(result) => (setStep({ kind: "done", result }), reach.reload())} />;
  if (step.kind === "done") return <Done env={env} path={path} keyName={keyName} link={link} result={step.result} onClose={onClose} />;
  const flows = [
    ...r.syncs.map((s) => `${s.kind} ${summary(s.target)}${s.paused_at ? " (paused)" : ""}`),
    ...r.projects.map((p) => `shared with project ${p}`),
    ...r.imported_by.map((f) => `imported by ${f}`),
    ...r.leases.map((l) => `lease of ${l.key} for ${l.consumer}, ends ${until(l.expires_at)}`),
    ...r.shares.map((s) => `one-time share ${s.note || s.id}, ends ${until(s.expires_at)}`),
    ...(r.truncated ? ["and more folders than were followed (256)"] : []),
  ];
  return (
    <div className="space-y-4 text-[12.5px]">
      {r.frozen && <Frozen env={env} path={path} by={r.frozen.by} why={r.frozen.why} at={r.frozen.at} onLifted={reach.reload} />}
      <p className="flex items-start gap-2 text-muted">
        {r.untrusted ? <ShieldAlert className="mt-0.5 size-3.5 shrink-0 text-danger" /> : <ShieldCheck className="mt-0.5 size-3.5 shrink-0 text-ok" />}
        {r.untrusted ? `Checks, sandboxes and previews running code roll hasn't vetted can read it: protect ${env.name} to stop that.` : `${env.name} is protected: code roll hasn't vetted never reads it.`}
      </p>
      <Part title="Tokens" empty="No token reaches it.">
        {r.tokens.map((h) => (
          <HolderRow key={h.id} h={h} />
        ))}
      </Part>
      <Part title="Workload identities" empty="No identity reaches it.">
        {r.identities.map((h) => (
          <HolderRow key={h.id} h={h} />
        ))}
      </Part>
      <Part title="Where it goes by itself" empty="No sync, lease, share or import takes it anywhere.">
        {flows.map((f) => (
          <li key={f} className="truncate px-3 py-2 font-mono text-[11.5px] text-soft">
            {f}
          </li>
        ))}
      </Part>
      <Part title="Read in the last 30 days" empty="Nobody read it in 30 days." note={`Not counted: ${r.uncounted}.`}>
        {r.readers.map((x) => (
          <li key={x.actor} className="flex items-center justify-between gap-3 px-3 py-2">
            <ActorMark who={x.actor} />
            <span className="shrink-0 text-[11.5px] text-subtle">
              {x.reads} {x.reads === 1 ? "read" : "reads"} · {ago(x.last_at)}
            </span>
          </li>
        ))}
      </Part>
      <div className="flex items-center justify-between gap-3 border-t border-line pt-3">
        <p className="text-[11.5px] text-subtle">Leaked? Cut all of this off at once, then rotate.</p>
        <button type="button" className={dangerButton} onClick={() => setStep({ kind: "confirm" })}>
          <Scissors className="size-3.5" /> Cut off…
        </button>
      </div>
    </div>
  );
}

/** What a cut-off would cut, from reach: one line per kind with its count and names. */
export function plan(r: Reach): { what: string; names: string[] }[] {
  const lines: [string, string, string[]][] = [
    ["token revoked", "tokens revoked", r.tokens.filter((h) => !h.skipped).map((h) => h.name)],
    ["identity disabled, its logins revoked", "identities disabled, their logins revoked", r.identities.filter((h) => !h.disabled_at).map((h) => h.name)],
    ["lease ended, its credential revoked", "leases ended, their credentials revoked", r.leases.map((l) => `${l.key} for ${l.consumer}`)],
    ["one-time share expired", "one-time shares expired", r.shares.map((s) => s.note || s.id)],
    ["project no longer sharing the folder", "projects no longer sharing the folder", r.projects],
    ["sync paused", "syncs paused", r.syncs.filter((s) => !s.paused_at).map((s) => `${s.kind} ${summary(s.target)}`.trim())],
  ];
  return lines.filter(([, , names]) => names.length > 0).map(([one, many, names]) => ({ what: names.length === 1 ? one : many, names }));
}

function Confirm({ env, path, keyName, r, setBusy, onBack, onDone }: { env: Environment; path: string; keyName: string; r: Reach; setBusy: (b: boolean) => void; onBack: () => void; onDone: (c: CutOff) => void }) {
  const { api } = useSecrets();
  const [why, setWhy] = useState("");
  const [freeze, setFreeze] = useState(false);
  const [running, setRunning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const lines = plan(r);
  const folder = `${env.name}${path === "/" ? "" : ` ${path}`}`;
  const run = async () => {
    setRunning(true);
    setBusy(true);
    setError(null);
    try {
      onDone(await api.cutOff(env.slug, path, keyName, { why: why.trim(), freeze }));
    } catch (e) {
      setError(errorText(e));
    } finally {
      setRunning(false);
      setBusy(false);
    }
  };
  return (
    <div className="space-y-4 text-[12.5px]">
      {lines.length ? (
        <ul className="divide-y divide-line overflow-hidden rounded-lg border border-line-2 bg-panel">
          {lines.map((l) => (
            <li key={l.what} className="px-3 py-2">
              <p className="text-fg">
                <span className="font-mono tabular-nums">{l.names.length}</span> {l.what}
              </p>
              <p className="mt-0.5 break-words font-mono text-[11.5px] text-subtle">{l.names.join(", ")}</p>
            </li>
          ))}
        </ul>
      ) : (
        <p className="text-[12px] text-subtle">Nothing reaches {keyName} now. You can still freeze the folder.</p>
      )}
      <Skipped names={r.tokens.filter((h) => h.skipped).map((h) => h.name)} />
      {r.unattributed_shares > 0 && (
        <p className="text-[11.5px] text-subtle">
          {r.unattributed_shares} other open {r.unattributed_shares === 1 ? "share doesn't" : "shares don't"} say where {r.unattributed_shares === 1 ? "its value" : "their values"} came from and {r.unattributed_shares === 1 ? "stays" : "stay"}: revoke them under Access if they might hold it.
        </p>
      )}
      <div>
        <label htmlFor="cut-off-why" className={label}>
          Why (on every revoked token and in the audit log)
        </label>
        <input id="cut-off-why" value={why} onChange={(e) => setWhy(e.target.value)} placeholder="INC-123: the key was in a CI log" className={field} maxLength={300} autoFocus />
      </div>
      <label className="flex items-start gap-2 text-muted">
        <input type="checkbox" checked={freeze} onChange={(e) => setFreeze(e.target.checked)} className="mt-0.5 accent-[var(--accent)]" />
        <span>
          Also freeze {folder}: reading its values answers 423 to everyone but its admins (services that read it start failing) until an admin unfreezes it. Admins can still rotate.
        </span>
      </label>
      <ErrorNote error={error} />
      <div className="flex items-center justify-end gap-2 border-t border-line pt-3">
        <button type="button" className={ghost} onClick={onBack} disabled={running}>
          Back
        </button>
        <button type="button" className={dangerButton} onClick={run} disabled={running || !why.trim() || (lines.length === 0 && !freeze)}>
          <Scissors className="size-3.5" /> {running ? "Cutting off…" : "Cut off now"}
        </button>
      </div>
    </div>
  );
}

function Done({ env, path, keyName, link, result: c, onClose }: { env: Environment; path: string; keyName: string; link: Link | null; result: CutOff; onClose: () => void }) {
  const { api, go } = useSecrets();
  const [rotated, setRotated] = useState<string | null>(null);
  const [rotating, setRotating] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const rows: [string, string[]][] = (
    [
      ["Tokens revoked", c.tokens.map((x) => x.name)],
      ["Identities disabled", c.identities.map((x) => x.name)],
      ["Identity logins revoked", c.logins ? [`${c.logins}`] : []],
      ["Leases ended", c.leases.map((x) => x.name)],
      ["Shares expired", c.shares.map((x) => x.name || x.id)],
      ["Projects no longer sharing it", c.projects],
      ["Syncs paused", c.syncs.map((x) => x.name)],
    ] as [string, string[]][]
  ).filter(([, n]) => n.length > 0);
  const rotate = async () => {
    setRotating(true);
    setError(null);
    try {
      await api.rotate(env.slug, keyName, path);
      setRotated(`Rotated ${keyName}: its old credential ends after its grace.`);
    } catch (e) {
      setError(errorText(e));
    } finally {
      setRotating(false);
    }
  };
  return (
    <div className="space-y-4 text-[12.5px]">
      {rows.length || c.frozen ? (
        <ul className="divide-y divide-line overflow-hidden rounded-lg border border-line-2 bg-panel">
          {rows.map(([what, names]) => (
            <li key={what} className="px-3 py-2">
              <p className="text-fg">{what}</p>
              <p className="mt-0.5 break-words font-mono text-[11.5px] text-subtle">{names.join(", ")}</p>
            </li>
          ))}
          {c.frozen && (
            <li className="flex items-center gap-2 px-3 py-2 text-fg">
              <Snowflake className="size-3.5 text-accent" /> {env.name}
              {path === "/" ? "" : ` ${path}`} is frozen
            </li>
          )}
        </ul>
      ) : (
        <p className="text-[12px] text-subtle">Nothing was left to cut.</p>
      )}
      <Skipped names={c.skipped.map((x) => x.name)} />
      {c.batches > 1 && <p className="text-[11.5px] text-subtle">The tokens were revoked in {c.batches} calls of at most 500.</p>}
      {c.failed.length > 0 && (
        <ErrorNote error={`Not everything was cut: ${c.failed.map((f) => `${f.what}${f.ids.length ? ` (${f.ids.join(", ")})` : ""}: ${f.why}`).join("; ")}. Run the cut-off again to cut what's left.`} />
      )}
      <section className="rounded-lg border border-line-2 bg-panel px-3 py-2.5">
        <h3 className="text-[12px] font-medium text-fg">Now rotate {keyName}</h3>
        {link && canRotate(link) ? (
          <p className="mt-1 text-[11.5px] text-subtle">roll issues a new credential; whoever held the old one loses it after its grace.</p>
        ) : (
          <p className="mt-1 text-[11.5px] text-subtle">Issue a new value where it comes from and set it here. Managed credentials of the project are under Health, Rotations.</p>
        )}
        {rotated && <p className="mt-1.5 text-[11.5px] text-ok">{rotated}</p>}
        <ErrorNote error={error} className="mt-1.5" />
        <div className="mt-2 flex gap-2">
          {link && canRotate(link) ? (
            <button type="button" className={button} onClick={rotate} disabled={rotating || !!rotated}>
              <RotateCw className="size-3.5" /> {rotating ? "Rotating…" : "Rotate now"}
            </button>
          ) : (
            <button type="button" className={button} onClick={() => (onClose(), go({ view: "health" }))}>
              <RotateCw className="size-3.5" /> Open rotations
            </button>
          )}
        </div>
      </section>
      <div className="flex justify-end border-t border-line pt-3">
        <button type="button" className={ghost} onClick={onClose}>
          Close
        </button>
      </div>
    </div>
  );
}

function Frozen({ env, path, by, why, at, onLifted }: { env: Environment; path: string; by: string; why: string; at: number; onLifted: () => void }) {
  const { api } = useSecrets();
  const [error, setError] = useState<string | null>(null);
  const [lifting, setLifting] = useState(false);
  const lift = async () => {
    setLifting(true);
    setError(null);
    try {
      await api.unfreeze(env.slug, path);
      onLifted();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setLifting(false);
    }
  };
  return (
    <div className="rounded-lg border border-accent/40 bg-accent/[0.06] px-3 py-2.5">
      <div className="flex items-start justify-between gap-3">
        <p className="flex items-start gap-2 text-fg">
          <Snowflake className="mt-0.5 size-3.5 shrink-0 text-accent" />
          <span>
            Frozen {ago(at)} by {by}: {why}. Only its admins read its values.
          </span>
        </p>
        <button type="button" className={button} onClick={lift} disabled={lifting}>
          {lifting ? "Unfreezing…" : "Unfreeze"}
        </button>
      </div>
      <ErrorNote error={error} className="mt-1.5" />
    </div>
  );
}

/** Tokens that reach beyond this project: a project's cut-off leaves them, for the operator. */
function Skipped({ names }: { names: string[] }) {
  if (!names.length) return null;
  return (
    <div className="rounded-lg border border-line-2 bg-panel px-3 py-2">
      <p className="text-fg">
        <span className="font-mono tabular-nums">{names.length}</span> {names.length === 1 ? "token stays" : "tokens stay"}: {names.length === 1 ? "it reaches" : "they reach"} beyond this project, so only the operator revokes {names.length === 1 ? "it" : "them"}. Ask them to.
      </p>
      <p className="mt-0.5 break-words font-mono text-[11.5px] text-subtle">{names.join(", ")}</p>
    </div>
  );
}

function Part({ title, empty, note, children }: { title: string; empty: string; note?: string; children: ReactNode[] }) {
  return (
    <section>
      <h3 className="mb-1.5 text-[11px] font-medium text-subtle">{title}</h3>
      {children.length ? <ul className="divide-y divide-line overflow-hidden rounded-lg border border-line-2 bg-panel">{children}</ul> : <p className="text-[12px] text-faint">{empty}</p>}
      {note && <p className="mt-1.5 text-[11px] text-faint">{note}</p>}
    </section>
  );
}

function HolderRow({ h }: { h: Holder }) {
  return (
    <li className="flex items-center justify-between gap-3 px-3 py-2">
      <span className={`min-w-0 truncate ${h.disabled_at ? "text-subtle line-through" : "text-fg"}`}>{h.name}</span>
      <span className="flex shrink-0 items-center gap-1.5 text-[11px] text-subtle">
        {h.disabled_at ? (
          <span>disabled {ago(h.disabled_at)}</span>
        ) : (
          h.actions.map((a) => (
            <span key={a} className={`rounded border px-1 font-mono ${a === "reveal" ? "border-accent/40 text-accent" : "border-line-2"}`}>
              {a}
            </span>
          ))
        )}
        {h.skipped && (
          <span title={h.skipped} className="rounded border border-line-2 px-1">
            operator revokes
          </span>
        )}
        <span>{h.last_used_at ? `used ${ago(h.last_used_at)}` : "never used"}</span>
        {h.expires_at && <span>· ends {until(h.expires_at)}</span>}
      </span>
    </li>
  );
}

/** A sync's target in a few words (its kind's own fields: repo, project, namespace…). */
function summary(target: unknown): string {
  if (!target || typeof target !== "object") return "";
  return Object.entries(target as Record<string, unknown>)
    .filter(([, v]) => typeof v === "string" && v.length < 80)
    .map(([k, v]) => `${k}=${v as string}`)
    .slice(0, 3)
    .join(" ");
}
