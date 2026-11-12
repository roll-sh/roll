import { Hand } from "lucide-react";
import { useState } from "react";
import { accessApi, productName, type Approval } from "@/lib/access-api";
import { btn, Connecting, danger, EmptyPanel, ErrorNote, input, Panel, primary } from "../ui";
import { AccessShell, relative, usePoll } from "./shell";

/**
 * `/dashboard/access/approvals`: requests waiting on you (an `ask` action), live, with exactly what
 * would run, and what you decided lately. `?id=` (the link in a pending answer) brings one to the
 * top.
 */
export function Approvals({ focus }: { focus?: string }) {
  const pending = usePoll(() => accessApi.approvals("pending"), 2500);
  const recent = usePoll(() => accessApi.approvals(), 10_000);
  const refresh = async () => {
    await Promise.all([pending.refresh(), recent.refresh()]);
  };
  const waiting = [...(pending.data ?? [])].sort((a, b) => Number(b.id === focus) - Number(a.id === focus));
  const decided = (recent.data ?? []).filter((a) => a.state !== "pending").slice(0, 30);
  return (
    <AccessShell tab="approvals">
      <div className="space-y-6">
        <ErrorNote error={pending.error} />
        {pending.data === null && !pending.error ? (
          <Connecting />
        ) : waiting.length === 0 ? (
          <EmptyPanel>
            <Hand className="size-6 text-muted" />
            <h2 className="text-[15px] font-medium">Nothing waits for you</h2>
            <p className="max-w-md text-[13px] text-muted">
              When a token or an MCP client tries an action you set to <em>ask</em> (dropping a database, revealing a secret,
              landing on main), it waits here until you approve or deny it.
            </p>
          </EmptyPanel>
        ) : (
          <section className="space-y-3" aria-label="Waiting for you">
            {waiting.map((a) => (
              <Pending key={a.id} a={a} focused={a.id === focus} refresh={refresh} />
            ))}
          </section>
        )}
        {decided.length > 0 && (
          <Panel title="Decided lately">
            <ul className="divide-y divide-line">
              {decided.map((a) => (
                <li key={a.id} className="flex items-center gap-3 px-5 py-2.5 text-[12px]">
                  <span className={`w-16 shrink-0 ${a.state === "denied" ? "text-danger" : a.state === "expired" ? "text-subtle" : "text-ok"}`}>{a.state}</span>
                  <span className="min-w-0 flex-1 truncate text-muted">{a.summary}</span>
                  <span className="shrink-0 text-subtle">
                    {a.grant_name} · {relative(a.decided_at ?? a.created_at)}
                  </span>
                </li>
              ))}
            </ul>
          </Panel>
        )}
      </div>
    </AccessShell>
  );
}

function Pending({ a, focused, refresh }: { a: Approval; focused: boolean; refresh: () => Promise<void> }) {
  const [busy, setBusy] = useState<"approve" | "deny" | null>(null);
  const [reason, setReason] = useState("");
  const [denying, setDenying] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const decide = async (approve: boolean) => {
    setBusy(approve ? "approve" : "deny");
    setError(null);
    try {
      if (approve) await accessApi.approve(a.id);
      else await accessApi.deny(a.id, reason.trim() || undefined);
      await refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(null);
    }
  };
  const destructive = a.action === "delete" || a.action === "deploy";
  return (
    <article className={`rise rounded-xl border bg-panel p-5 ${focused ? "border-accent/60" : "border-line"}`}>
      <div className="flex flex-wrap items-start gap-3">
        <div className="min-w-0 flex-1">
          <p className="text-[12px] text-subtle">
            <b className="font-medium text-fg">{a.grant_name}</b> wants to <b className={`font-medium ${destructive ? "text-danger" : "text-fg"}`}>{a.action}</b> in{" "}
            {productName(a.product)}
            {a.tool ? (
              <>
                {" "}
                via <code className="font-mono">{a.tool}</code>
              </>
            ) : null}
          </p>
          <h2 className="mt-1 truncate font-mono text-[14px]">{a.resource ?? productName(a.product)}</h2>
          <p className="mt-1 font-mono text-[11px] text-subtle">
            {a.method} {a.route} · asked {relative(a.created_at)} · expires {relative(a.expires_at)}
          </p>
        </div>
        <div className="flex shrink-0 gap-2">
          <button onClick={() => decide(true)} disabled={busy !== null} className={destructive ? danger : primary}>
            {busy === "approve" ? <span className="shimmer">Approving…</span> : "Approve"}
          </button>
          <button onClick={() => (denying ? decide(false) : setDenying(true))} disabled={busy !== null} className={btn}>
            {busy === "deny" ? <span className="shimmer">Denying…</span> : "Deny"}
          </button>
        </div>
      </div>
      {a.args !== null && a.args !== undefined && (
        <pre className="mt-3 max-h-56 overflow-auto rounded-lg border border-line bg-bg p-3 font-mono text-[11px] leading-relaxed text-muted">
          {JSON.stringify(a.args, null, 2)}
        </pre>
      )}
      {denying && (
        <input
          autoFocus
          value={reason}
          onChange={(e) => setReason(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && decide(false)}
          placeholder="Why (the agent sees this; optional). Enter to deny."
          maxLength={500}
          className={`${input} mt-3 w-full`}
        />
      )}
      <ErrorNote error={error} />
    </article>
  );
}
