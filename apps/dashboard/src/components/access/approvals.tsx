import { Bell, Hand } from "lucide-react";
import { useEffect, useState } from "react";
import { accessApi, productName, REMEMBER, rememberable, type Approval, type ChatTarget, type Remember } from "@/lib/access-api";
import { createIntegrationsApi, type ChatInstall } from "@/lib/integrations-api";
import { call } from "@/lib/roll-api";
import { useShownOwner } from "../agents/use-connection";
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
        <NotifyMe />
        <ChatSetting />
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
  const [remember, setRemember] = useState<Remember>("once");
  const [error, setError] = useState<string | null>(null);
  const decide = async (approve: boolean) => {
    setBusy(approve ? "approve" : "deny");
    setError(null);
    try {
      if (approve) await accessApi.approve(a.id, remember);
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
          {rememberable(a) && (
            <select
              value={remember}
              onChange={(e) => setRemember(e.target.value as Remember)}
              className={input}
              aria-label="How long this approval also allows the same action on this resource"
              title={`Also allow ${a.action} on ${a.resource} without asking`}
            >
              {REMEMBER.map((r) => (
                <option key={r.value} value={r.value}>
                  {r.label}
                </option>
              ))}
            </select>
          )}
          <button onClick={() => decide(true)} disabled={busy !== null} className={destructive ? danger : primary}>
            {busy === "approve" ? <span className="shimmer">Approving…</span> : "Approve"}
          </button>
          <button onClick={() => (denying ? decide(false) : setDenying(true))} disabled={busy !== null} className={btn}>
            {busy === "deny" ? <span className="shimmer">Denying…</span> : "Deny"}
          </button>
        </div>
      </div>
      {a.args !== null && a.args !== undefined && <Args args={a.args} />}
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

/** Ask the browser for notifications, so a new approval reaches you while roll is in the background. */
function NotifyMe() {
  const [perm, setPerm] = useState<NotificationPermission | "unsupported">("unsupported");
  useEffect(() => setPerm(typeof Notification === "undefined" ? "unsupported" : Notification.permission), []);
  if (perm !== "default") return null;
  return (
    <p className="flex flex-wrap items-center gap-2 text-[12px] text-muted">
      <Bell className="size-3.5" />
      Get a notification when a request starts waiting, while roll is open in the background.
      <button onClick={() => void Notification.requestPermission().then(setPerm)} className={btn}>
        Notify me
      </button>
    </p>
  );
}

const integrations = createIntegrationsApi(call);

/**
 * Post approvals to a Slack channel too (roll code's chat ops: docs/permissions.md, "Approvals in
 * chat"); the people the install allows can then approve or deny from Slack.
 */
function ChatSetting() {
  const shown = useShownOwner();
  const [current, setCurrent] = useState<ChatTarget | null | undefined>(undefined);
  const [installs, setInstalls] = useState<ChatInstall[]>([]);
  const [install, setInstall] = useState("");
  const [channel, setChannel] = useState("");
  const [approvers, setApprovers] = useState("");
  const [open, setOpen] = useState(false);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    accessApi
      .notify()
      .then((n) => {
        setCurrent(n.chat);
        setInstall(n.chat?.install ?? "");
        setChannel(n.chat?.channel ?? "");
        setApprovers(n.chat?.approvers?.join(", ") ?? "");
      })
      .catch(() => setCurrent(null));
  }, []);
  useEffect(() => {
    if (!shown?.owner) return;
    integrations
      .list(shown.owner)
      .then((i) => setInstalls(i.slack.installs))
      .catch(() => setInstalls([]));
  }, [shown?.owner]);
  const save = async (chat: ChatTarget | null) => {
    setError(null);
    try {
      await accessApi.setNotify(chat);
      setCurrent(chat);
      setOpen(false);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };
  if (current === undefined) return null;
  const name = (id: string) => installs.find((i) => i.id === id)?.team_name ?? id;
  return (
    <div className="space-y-2 text-[12px] text-muted">
      <p className="flex flex-wrap items-center gap-2">
        {current ? (
          <>
            Approvals are also posted to <b className="font-medium text-fg">#{current.channel}</b> in {name(current.install)}, where{" "}
            {current.approvers.length === 1 ? "one person" : `${current.approvers.length} people`} you named can decide them.
          </>
        ) : (
          "Approvals can be posted to a Slack channel, where the people you name approve or deny them."
        )}
        <button onClick={() => setOpen((o) => !o)} className={btn}>
          {current ? "Change" : "Post to Slack"}
        </button>
        {current && (
          <button onClick={() => void save(null)} className={btn}>
            Stop
          </button>
        )}
      </p>
      {open && (
        <form
          className="rise flex flex-wrap items-center gap-2"
          onSubmit={(e) => {
            e.preventDefault();
            const ids = approverIds(approvers);
            if (install && channel.trim() && ids.length) void save({ install, channel: channel.trim().replace(/^#/, ""), approvers: ids });
          }}
        >
          <select value={install} onChange={(e) => setInstall(e.target.value)} className={input} aria-label="Slack workspace">
            <option value="">{installs.length ? "Slack workspace…" : "Connect Slack in the agents' integrations first"}</option>
            {installs.map((i) => (
              <option key={i.id} value={i.id}>
                {i.team_name}
              </option>
            ))}
          </select>
          <input value={channel} onChange={(e) => setChannel(e.target.value)} placeholder="Channel id (C0123…)" className={`${input} font-mono text-[12px]`} aria-label="Channel id" />
          <input
            value={approvers}
            onChange={(e) => setApprovers(e.target.value)}
            placeholder="Who may decide: Slack member ids (U0123…, U0456…)"
            className={`${input} min-w-64 flex-1 font-mono text-[12px]`}
            aria-label="Approvers' Slack member ids"
          />
          <button type="submit" disabled={!install || !channel.trim() || approverIds(approvers).length === 0} className={primary}>
            Save
          </button>
        </form>
      )}
      <ErrorNote error={error} />
    </div>
  );
}

/** Slack member ids typed as a list (commas, spaces or new lines). */
function approverIds(text: string): string[] {
  return [...new Set(text.split(/[\s,]+/).map((s) => s.trim().replace(/^@/, "")).filter(Boolean))];
}

/**
 * What a request would do, readable: a push's ref updates as a list, SQL, CQL or a Redis command as
 * code, anything else as its JSON (secrets in it were redacted when it was asked).
 */
function Args({ args }: { args: unknown }) {
  const box = "mt-3 max-h-56 overflow-auto rounded-lg border border-line bg-bg p-3 font-mono text-[11px] leading-relaxed text-muted";
  const o = (args && typeof args === "object" ? args : {}) as Record<string, unknown>;
  if (Array.isArray(o.updates)) {
    const zero = (s: unknown) => typeof s === "string" && /^0+$/.test(s);
    return (
      <ul className={box}>
        {(o.updates as { ref: string; old: string; new: string; action: string }[]).map((u) => (
          <li key={u.ref}>
            <span className={u.action === "delete" || u.action === "deploy" ? "text-danger" : "text-fg"}>{u.action}</span> {u.ref}{" "}
            <span className="text-subtle">{zero(u.old) ? "(new)" : zero(u.new) ? "(deleted)" : `${u.old.slice(0, 8)} → ${u.new.slice(0, 8)}`}</span>
          </li>
        ))}
      </ul>
    );
  }
  const code = typeof o.sql === "string" ? o.sql : typeof o.query === "string" ? o.query : Array.isArray(o.command) ? (o.command as unknown[]).join(" ") : null;
  if (code !== null) return <pre className={`${box} whitespace-pre-wrap text-fg`}>{code}</pre>;
  return <pre className={box}>{JSON.stringify(args, null, 2)}</pre>;
}
