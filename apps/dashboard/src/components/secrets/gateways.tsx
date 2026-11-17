import { Check, Copy, Network, Plus, Trash2 } from "lucide-react";
import { useState } from "react";
import type { Gateway } from "@/lib/secrets-api";
import { useSecrets } from "./secrets-page";
import { ActorMark, ago, button, card, dangerButton, ErrorNote, errorText, field, ghost, iconButton, label, primary, Section, useLoad } from "./ui";

/** A link reports every 30 s; it's online while it does. */
function state(g: Gateway): { online: boolean; text: string } {
  const s = g.status;
  if (s.online) return { online: true, text: `online on ${s.node ?? "a node"}${s.version ? `, roll ${s.version}` : ""}` };
  if (s.last_seen_at) return { online: false, text: `offline, last seen ${ago(s.last_seen_at)}` };
  return { online: false, text: "never connected" };
}

/**
 * Gateways into private networks (docs/secrets.md, "Gateways"): a process the team runs inside a
 * VPC dials out to roll, and roll's conversations with private databases and directories travel
 * through it. The gateway's own allow-list decides what it reaches; roll shows it as reported.
 */
export function Gateways() {
  const { api } = useSecrets();
  const list = useLoad(() => api.gateways(), [api]);
  const [adding, setAdding] = useState(false);
  const [name, setName] = useState("");
  const [withToken, setWithToken] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [deleting, setDeleting] = useState<string | null>(null);
  // The new gateway's token, shown once; dropped when dismissed.
  const [made, setMade] = useState<{ id: string; token: string } | null>(null);
  const [copied, setCopied] = useState(false);
  const ok = /^[A-Za-z0-9_-]{1,64}$/.test(name);

  const add = async () => {
    setBusy(true);
    setError(null);
    try {
      const m = await api.addGateway({ name, token: withToken });
      if (m.token) setMade({ id: m.gateway.id, token: m.token });
      setAdding(false);
      setName("");
      list.reload();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  const del = (g: Gateway) =>
    api.deleteGateway(g.id).then(
      () => (setDeleting(null), list.reload()),
      (e) => (setDeleting(null), setError(errorText(e))),
    );
  const copy = (text: string) =>
    navigator.clipboard.writeText(text).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    });

  return (
    <Section
      title="Gateways"
      aside={
        <button className={button} onClick={() => setAdding((a) => !a)}>
          <Plus className="size-3.5" /> Add a gateway
        </button>
      }
    >
      <p className="mb-2 text-[11.5px] text-muted">
        For databases and directories with a private address. Run <span className="font-mono">roll secrets gateways up</span> inside the network: it dials out to roll, reaches only what its{" "}
        <span className="font-mono">--allow</span> list names, and relays bytes; TLS still runs from roll to the server.
      </p>
      <ErrorNote error={error ?? list.error} onDismiss={error ? () => setError(null) : undefined} className="mb-2" />
      {made && (
        <div className="rise mb-3 rounded-xl border border-line-2 bg-panel p-3">
          <p className="mb-1.5 text-[11.5px] text-muted">The gateway&apos;s token, shown once. Start it inside the network with:</p>
          <div className="flex items-start gap-2">
            <code className="min-w-0 flex-1 break-all rounded-lg bg-canvas px-2 py-1.5 font-mono text-[11.5px]">
              ROLL_GATEWAY_TOKEN={made.token} roll secrets gateways up --gateway {made.id} --allow 10.0.0.0/8:5432
            </code>
            <button className={`${iconButton} shrink-0`} onClick={() => void copy(made.token)} aria-label="Copy the token" title="Copy the token">
              {copied ? <Check className="size-3.5 text-ok" /> : <Copy className="size-3.5" />}
            </button>
          </div>
          <button className={`${ghost} mt-2`} onClick={() => setMade(null)}>
            Done
          </button>
        </div>
      )}
      {adding && (
        <form
          className="rise mb-3 grid gap-2 rounded-xl border border-line-2 bg-panel p-3 sm:grid-cols-[11rem_1fr]"
          onSubmit={(e) => {
            e.preventDefault();
            if (ok && !busy) void add();
          }}
        >
          <label>
            <span className={label}>Name</span>
            <input autoFocus value={name} onChange={(e) => setName(e.target.value)} placeholder="vpc-prod" spellCheck={false} className={`${field} font-mono text-[12px]`} />
          </label>
          <label className="flex items-end gap-2 pb-1.5 text-[12px] text-muted">
            <input type="checkbox" checked={withToken} onChange={(e) => setWithToken(e.target.checked)} />
            Make its token now (off: it logs in through a workload identity made for it)
          </label>
          <div className="sm:col-start-2 flex gap-2">
            <button type="submit" className={primary} disabled={!ok || busy}>
              {busy ? "Adding…" : "Add gateway"}
            </button>
            <button type="button" className={ghost} onClick={() => setAdding(false)}>
              Cancel
            </button>
          </div>
        </form>
      )}
      {list.data && list.data.length === 0 && !adding ? (
        <p className="rounded-xl border border-dashed border-line-2 px-3 py-4 text-[12px] text-muted">No gateways. Add one to reach a database or directory that only has a private address.</p>
      ) : (
        list.data && (
          <div className={`${card} divide-y divide-line`}>
            {list.data.map((g) => {
              const st = state(g);
              return (
                <div key={g.id} className="flex flex-wrap items-center gap-x-3 gap-y-1 px-3 py-2">
                  <Network className="size-3.5 text-muted" />
                  <span className="font-mono text-[12px]">{g.name}</span>
                  <span className="font-mono text-[11px] text-subtle">{g.id}</span>
                  <span className="flex items-center gap-1.5 text-[11.5px] text-muted">
                    <span className={`size-1.5 rounded-full ${st.online ? "bg-accent" : "bg-line-2"}`} aria-hidden />
                    {st.text}
                  </span>
                  <span className="ml-auto text-[11px] text-subtle">
                    added {ago(g.created_at)} · <ActorMark who={g.created_by} />
                  </span>
                  {deleting === g.id ? (
                    <span className="flex gap-1">
                      <button className={dangerButton} onClick={() => del(g)}>
                        Delete
                      </button>
                      <button className={ghost} onClick={() => setDeleting(null)}>
                        Keep
                      </button>
                    </span>
                  ) : (
                    <button className={ghost} onClick={() => setDeleting(g.id)} aria-label={`Delete the gateway ${g.name}`}>
                      <Trash2 className="size-3.5" />
                    </button>
                  )}
                  {g.status.allow.length > 0 && (
                    <div className="flex w-full flex-wrap gap-1 pl-6" title="The gateway's own allow-list, as it reported it">
                      {g.status.allow.map((a) => (
                        <span key={a} className="rounded-md border border-line px-1.5 py-0.5 font-mono text-[10.5px] text-subtle">
                          {a}
                        </span>
                      ))}
                    </div>
                  )}
                </div>
              );
            })}
          </div>
        )
      )}
    </Section>
  );
}

/** Which of the project's gateways a database or directory is reached through, if any. */
export function GatewayPicker({ value, onChange, title = "Reached through", none = "roll directly (a public address)" }: { value: string; onChange: (id: string) => void; title?: string; none?: string }) {
  const { api } = useSecrets();
  const list = useLoad(() => api.gateways(), [api]);
  return (
    <label>
      <span className={label}>{title}</span>
      <select value={value} onChange={(e) => onChange(e.target.value)} className={`${field} text-[12px]`}>
        <option value="">{none}</option>
        {(list.data ?? []).map((g) => (
          <option key={g.id} value={g.id}>
            gateway {g.name} ({g.status.online ? "online" : "offline"})
          </option>
        ))}
      </select>
    </label>
  );
}
