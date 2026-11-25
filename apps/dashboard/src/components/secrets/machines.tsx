import { Check, Copy, Plus } from "lucide-react";
import { useState } from "react";
import { ROLL_API } from "@/lib/roll-api";
import type { Identity, MachineChain, MadeBootstrap } from "@/lib/secrets-api";
import { useSecrets } from "./secrets-page";
import { ActorMark, ago, button, dangerButton, ErrorNote, errorText, field, ghost, label, primary, until, useLoad } from "./ui";

/** What a machine with a bootstrap token sets before its first start (docs/secrets.md, "On a VM"). */
const setupLine = (i: Identity, env: string) => `ROLL_API_URL=${ROLL_API} ROLL_IDENTITY=${i.id} ROLL_BOOTSTRAP_TOKEN=… roll run --env ${env} -- ./server`;

/**
 * Make a machine identity's bootstrap token: minutes to be exchanged, the addresses it works from,
 * how long each machine credential lives. The token is in the answer once and is shown until Done;
 * nothing fetches it again.
 */
export function BootstrapMaker({ identity, env, onMade, open: initiallyOpen = false }: { identity: Identity; env: string; onMade?: () => void; open?: boolean }) {
  const { api } = useSecrets();
  const [open, setOpen] = useState(initiallyOpen);
  const [minutes, setMinutes] = useState("10");
  const [hours, setHours] = useState("24");
  const [ips, setIps] = useState("");
  const [busy, setBusy] = useState(false);
  const [made, setMade] = useState<(MadeBootstrap & { minutes: number; ips: string[] }) | null>(null);
  const [copied, setCopied] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const m = Number(minutes);
  const h = Number(hours);
  const ipList = ips.split(/[\s,]+/).filter(Boolean);
  const ok = Number.isInteger(m) && m >= 1 && m <= 60 && Number.isInteger(h) && h >= 1 && h <= 720;

  if (made) {
    return (
      <div className="fade-in rounded-lg border border-accent/40 bg-panel p-3" role="status">
        <p className="text-[12.5px]">
          <span className="text-muted">
            A bootstrap token for one machine of <b className="font-medium text-fg">{identity.name}</b>. Copy it now: it isn&apos;t shown again. It works once, within {made.minutes} min
            {made.ips.length ? `, from ${made.ips.join(", ")}` : ""}.
          </span>
        </p>
        <div className="mt-2 flex items-center gap-2">
          <code className="min-w-0 flex-1 truncate rounded-md border border-line-2 bg-panel-2 px-2 py-1.5 font-mono text-[12px] select-all">{made.secret}</code>
          <button
            className={button}
            onClick={() => {
              void navigator.clipboard?.writeText(made.secret);
              setCopied(true);
              setTimeout(() => setCopied(false), 1200);
            }}
          >
            {copied ? <Check className="size-3.5 text-ok" /> : <Copy className="size-3.5" />} Copy
          </button>
          <button className={ghost} onClick={() => setMade(null)}>
            Done
          </button>
        </div>
        <p className="mt-2 font-mono text-[11px] break-all text-subtle">{setupLine(identity, env)}</p>
      </div>
    );
  }
  if (!open) {
    return (
      <button className={button} onClick={() => setOpen(true)} disabled={!!identity.disabled_at} title={identity.disabled_at ? "Enable the identity first" : undefined}>
        <Plus className="size-3.5" /> Make a bootstrap token
      </button>
    );
  }
  return (
    <form
      className="rise space-y-3 rounded-lg border border-line-2 bg-panel p-3"
      onSubmit={async (e) => {
        e.preventDefault();
        if (!ok) return;
        setBusy(true);
        setError(null);
        try {
          const r = await api.createBootstrap(identity.id, {
            minutes: m,
            credential_hours: h,
            ...(ipList.length && { allowed_ips: ipList }),
          });
          setMade({ ...r, minutes: m, ips: ipList });
          setOpen(false);
          onMade?.();
        } catch (err) {
          setError(errorText(err));
        }
        setBusy(false);
      }}
      onKeyDown={(e) => e.key === "Escape" && setOpen(false)}
    >
      <div className="grid gap-3 sm:grid-cols-[7rem_9rem_minmax(0,1fr)]">
        <label>
          <span className={label}>Minutes (1–60)</span>
          <input value={minutes} onChange={(e) => setMinutes(e.target.value)} inputMode="numeric" className={`${field} font-mono`} />
        </label>
        <label>
          <span className={label}>Credential hours (1–720)</span>
          <input value={hours} onChange={(e) => setHours(e.target.value)} inputMode="numeric" className={`${field} font-mono`} />
        </label>
        <label>
          <span className={label}>Allowed addresses (CIDRs; empty for any)</span>
          <input value={ips} onChange={(e) => setIps(e.target.value)} placeholder="the machine's address, e.g. 198.51.100.7/32" className={`${field} font-mono`} />
        </label>
      </div>
      <p className="text-[11.5px] leading-snug text-muted">
        One token per machine, for its provisioning (cloud-init, Ansible), never a repository. The machine exchanges it once for its own credential and replaces that at half its life. A token used twice revokes the machine.
      </p>
      <ErrorNote error={error} onDismiss={() => setError(null)} />
      <div className="flex justify-end gap-2">
        <button type="button" className={ghost} onClick={() => setOpen(false)}>
          Cancel
        </button>
        <button type="submit" className={primary} disabled={!ok || busy}>
          {busy ? "Making…" : "Make token"}
        </button>
      </div>
    </form>
  );
}

const STATE: Record<MachineChain["state"], string> = {
  waiting: "token not exchanged yet",
  live: "live",
  ended: "ended",
  revoked: "revoked",
};

/** One line of where a machine stands. */
function standing(m: MachineChain): string {
  if (m.state === "revoked") return `revoked ${m.revoked_at ? ago(m.revoked_at) : ""}${m.revoked_why ? `: ${m.revoked_why}` : ""}`;
  if (m.state === "waiting") return `token not exchanged yet · ends ${m.expires_at ? until(m.expires_at) : ""}`;
  const refreshed = m.refreshed_at ? `credential replaced ${ago(m.refreshed_at)}` : STATE[m.state];
  return m.state === "live" ? `${refreshed} · ends ${m.expires_at ? until(m.expires_at) : ""}` : `ended · ${refreshed}`;
}

/** A machine identity's machines: one chain each (its bootstrap token's id), live or not, with revoke and a new token. */
export function Machines({ identity, env }: { identity: Identity; env: string }) {
  const { api } = useSecrets();
  const machines = useLoad(() => api.machines(identity.id), [api, identity.id]);
  const [error, setError] = useState<string | null>(null);
  const [sure, setSure] = useState<string | null>(null);
  const rows = machines.data ?? [];
  return (
    <div className="space-y-2 border-t border-line px-3 py-2.5">
      <BootstrapMaker identity={identity} env={env} onMade={machines.reload} />
      <ErrorNote error={error ?? machines.error} onDismiss={error ? () => setError(null) : undefined} />
      {machines.data && rows.length === 0 ? (
        <p className="text-[11.5px] text-muted">No machines yet. Each machine gets its own bootstrap token.</p>
      ) : (
        <ul className="divide-y divide-line rounded-lg border border-line-2">
          {rows.map((m) => {
            const on = m.state === "live" || m.state === "waiting";
            return (
              <li key={m.chain} className={`flex flex-wrap items-center gap-x-3 gap-y-1 px-2.5 py-1.5 ${on ? "" : "opacity-60"}`}>
                <span className={`size-1.5 rounded-full ${m.state === "live" ? "bg-accent" : "bg-line-3"}`} aria-hidden />
                <span className="font-mono text-[11.5px]">{m.chain}</span>
                <span className="text-[11px] text-subtle">{standing(m)}</span>
                <span className="ml-auto flex items-center gap-2 text-[11px] text-subtle">
                  made {ago(m.created_at)} · <ActorMark who={m.created_by} /> · {m.credential_hours} h credentials
                </span>
                {on &&
                  (sure === m.chain ? (
                    <span className="flex gap-1">
                      <button className={dangerButton} onClick={() => api.revokeMachine(identity.id, m.chain).then(machines.reload, (e) => setError(errorText(e)))} title="Its credential and its logins' tokens stop working at once">
                        Revoke
                      </button>
                      <button className={ghost} onClick={() => setSure(null)}>
                        Keep
                      </button>
                    </span>
                  ) : (
                    <button className={ghost} onClick={() => setSure(m.chain)}>
                      Revoke
                    </button>
                  ))}
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}
