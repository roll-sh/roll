import { ChevronRight, Plug, ShieldCheck } from "lucide-react";
import { useEffect, useState } from "react";
import {
  accessApi,
  CLIENT_NAMES,
  CLIENT_WHERE,
  CLIENTS,
  describeRule,
  mcpConfig,
  mcpUrl,
  reach,
  statusOf,
  type Catalog,
  type Grant,
  type McpClient,
  type Rule,
} from "@/lib/access-api";
import { useWorkspace } from "@/lib/workspace-store";
import { Link } from "../link";
import { btn, Connecting, CopyButton, danger, EmptyPanel, ErrorNote, input, Panel, primary, useCopy } from "../ui";
import { Builder } from "./builder";
import { AccessShell, relative, usePoll } from "./shell";

/** `/dashboard/access`: how to connect a client, and every token and connection with what it may do. */
export function AccessOverview() {
  const { data: grants, error, refresh } = usePoll(() => accessApi.tokens(), 10_000);
  const { data: me } = usePoll(() => accessApi.me(), 60_000);
  return (
    <AccessShell tab="overview">
      <div className="space-y-6">
        {me?.locked === false && (
          <p className="rounded-lg border border-line bg-panel px-4 py-3 text-[13px] text-muted">
            This daemon is open (no <code className="font-mono">ROLL_ADMIN_TOKEN</code>): tokens keep well-behaved clients to their
            rules, but anything on this machine can call the API as the operator. Set <code className="font-mono">ROLL_ADMIN_TOKEN</code>{" "}
            for tokens to protect anything.
          </p>
        )}
        <ConnectPanel />
        <ErrorNote error={error} />
        {grants === null && !error ? <Connecting /> : <Grants grants={grants ?? []} refresh={refresh} />}
      </div>
    </AccessShell>
  );
}

/** One client at a time: its config with the server's address, OAuth first. */
export function ConnectPanel({ token }: { token?: string }) {
  const [client, setClient] = useState<McpClient>("claude-code");
  const { copied, copy } = useCopy();
  const url = mcpUrl();
  const cfg = mcpConfig(client, url, token);
  return (
    <Panel
      title={
        <span className="flex items-center gap-2">
          <Plug className="size-4 text-muted" /> Connect an MCP client
        </span>
      }
      aside={
        <span className="flex min-w-0 items-center gap-1 rounded-lg border border-line bg-bg px-2 py-1">
          <code className="truncate font-mono text-[11px] text-muted">{url}</code>
          <CopyButton id="url" text={url} copied={copied} onCopy={copy} />
        </span>
      }
    >
      <div className="flex gap-1 overflow-x-auto border-b border-line px-3 [scrollbar-width:none]" role="tablist">
        {CLIENTS.map((c) => (
          <button
            key={c}
            role="tab"
            aria-selected={c === client}
            onClick={() => setClient(c)}
            className={`-mb-px shrink-0 border-b px-2.5 py-2 text-[12px] transition-colors duration-150 ${
              c === client ? "border-fg text-fg" : "border-transparent text-muted hover:text-fg"
            }`}
          >
            {CLIENT_NAMES[c]}
          </button>
        ))}
      </div>
      <div className="space-y-3 p-5">
        <p className="text-[13px] text-muted">{CLIENT_WHERE[client]}</p>
        <div className="relative rounded-lg border border-line bg-bg">
          <pre className="overflow-x-auto p-3 pr-10 font-mono text-[12px] leading-relaxed">{cfg.text}</pre>
          <span className="absolute top-2 right-2">
            <CopyButton id={`cfg-${client}`} text={cfg.text} copied={copied} onCopy={copy} />
          </span>
        </div>
        {!token && (
          <p className="text-[12px] text-subtle">
            With OAuth you choose what the client may do when it connects, and can narrow or revoke it here. For CI or a client
            without OAuth, <Link href="/dashboard/access/new" className="text-accent hover:underline">make a token</Link> and send it as{" "}
            <code className="font-mono">Authorization: Bearer</code>.
          </p>
        )}
      </div>
    </Panel>
  );
}

function Grants({ grants, refresh }: { grants: Grant[]; refresh: () => Promise<void> }) {
  const live = grants.filter((g) => statusOf(g) === "active");
  const old = grants.filter((g) => statusOf(g) !== "active");
  const [showOld, setShowOld] = useState(false);
  if (grants.length === 0) {
    return (
      <EmptyPanel>
        <ShieldCheck className="size-6 text-muted" />
        <h2 className="text-[15px] font-medium">No tokens or connections yet</h2>
        <p className="max-w-md text-[13px] text-muted">
          A token gives an agent, a CI job or a script exactly what you choose: which products, which projects, which actions, and
          for how long. Connections from MCP clients show up here too.
        </p>
        <Link href="/dashboard/access/new" className={btn}>
          Make a token
        </Link>
      </EmptyPanel>
    );
  }
  return (
    <Panel title="Tokens and connections" aside={<span className="font-mono text-[12px] text-subtle">{live.length} active</span>}>
      <ul className="divide-y divide-line">
        {live.map((g) => (
          <GrantRow key={g.id} g={g} refresh={refresh} />
        ))}
      </ul>
      {old.length > 0 && (
        <div className="border-t border-line">
          <button onClick={() => setShowOld((s) => !s)} className="flex w-full items-center gap-1.5 px-5 py-2.5 text-[12px] text-subtle hover:text-fg">
            <ChevronRight className={`size-3.5 transition-transform duration-150 ${showOld ? "rotate-90" : ""}`} />
            {old.length} revoked or expired
          </button>
          {showOld && (
            <ul className="divide-y divide-line border-t border-line">
              {old.map((g) => (
                <GrantRow key={g.id} g={g} refresh={refresh} />
              ))}
            </ul>
          )}
        </div>
      )}
    </Panel>
  );
}

const REACH_TONE: Record<ReturnType<typeof reach>, string> = {
  "read only": "text-muted border-line",
  "can change things": "text-fg border-line-2",
  "can destroy or ship": "text-accent border-accent/40",
};

function GrantRow({ g, refresh }: { g: Grant; refresh: () => Promise<void> }) {
  const ws = useWorkspace();
  const projectName = (id: string) => ws.projects.find((p) => p.id === id)?.name ?? id;
  const [open, setOpen] = useState(false);
  const [confirm, setConfirm] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [editing, setEditing] = useState(false);
  const [secret, setSecret] = useState<string | null>(null);
  const { copied, copy } = useCopy();
  const status = statusOf(g);
  const r = reach(g);
  const rotate = async () => {
    setBusy(true);
    setError(null);
    try {
      setSecret((await accessApi.rotate(g.id)).token);
      await refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
  const revoke = async () => {
    setBusy(true);
    try {
      await accessApi.revoke(g.id);
      await refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
      setConfirm(false);
    }
  };
  return (
    <li className={`px-5 py-3.5 ${status === "active" ? "" : "opacity-60"}`}>
      <div className="flex items-start gap-3">
        <button onClick={() => setOpen((o) => !o)} className="mt-0.5 text-subtle hover:text-fg" aria-expanded={open} aria-label={open ? "Hide rules" : "Show rules"}>
          <ChevronRight className={`size-3.5 transition-transform duration-150 motion-reduce:transition-none ${open ? "rotate-90" : ""}`} />
        </button>
        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-center gap-2">
            <span className="truncate text-[14px] font-medium">{g.name}</span>
            <span className="rounded border border-line px-1.5 text-[11px] text-subtle">{g.kind === "oauth" ? (g.client?.name ?? "OAuth") : "token"}</span>
            {status === "active" ? (
              <span className={`rounded border px-1.5 text-[11px] ${REACH_TONE[r]}`}>{r}</span>
            ) : (
              <span className="rounded border border-line px-1.5 text-[11px] text-subtle">{status}</span>
            )}
          </div>
          <p className="mt-1 truncate text-[12px] text-muted">{g.rules.map((x) => describeRule(x, projectName)).join("  ·  ")}</p>
          <p className="mt-1 font-mono text-[11px] text-subtle">
            {g.id} · used {relative(g.last_used_at)}
            {g.last_used_ip ? ` from ${g.last_used_ip}` : ""} · {g.expires_at === null ? "never expires" : `expires ${relative(g.expires_at)}`}
            {g.ip_allow.length > 0 ? ` · only ${g.ip_allow.join(", ")}` : ""}
          </p>
        </div>
        {status === "active" && !confirm && (
          <span className="flex shrink-0 gap-1.5">
            <button onClick={() => setEditing((e) => !e)} className={btn} aria-expanded={editing}>
              Edit
            </button>
            {g.kind === "token" && (
              <button onClick={rotate} disabled={busy} className={btn} title="A new secret with the same rules; the old one stops at once">
                Rotate
              </button>
            )}
            <button onClick={() => setConfirm(true)} className={btn}>
              Revoke
            </button>
          </span>
        )}
        {confirm && (
          <span className="flex shrink-0 gap-1.5">
            <button onClick={revoke} disabled={busy} className={danger}>
              {busy ? <span className="shimmer">Revoking…</span> : "Revoke now"}
            </button>
            <button onClick={() => setConfirm(false)} className={btn}>
              Keep
            </button>
          </span>
        )}
      </div>
      {secret && (
        <div className="rise mt-3 ml-6 space-y-1.5">
          <div className="flex items-center gap-1 rounded-lg border border-line bg-bg px-3 py-2">
            <code className="min-w-0 flex-1 truncate font-mono text-[12px]">{secret}</code>
            <CopyButton id={`rotated-${g.id}`} text={secret} copied={copied} onCopy={copy} />
          </div>
          <p className="text-[12px] text-subtle">The new secret: copy it now. The old one stopped working.</p>
        </div>
      )}
      {editing && (
        <EditGrant
          g={g}
          onDone={async () => {
            setEditing(false);
            await refresh();
          }}
        />
      )}
      {open && (
        <ul className="rise mt-3 ml-6 space-y-1.5">
          {g.rules.map((x, i) => (
            <li key={i} className="rounded-lg border border-line bg-bg px-3 py-2 text-[12px] text-muted">
              {describeRule(x, projectName)}
            </li>
          ))}
          <li className="pt-1">
            <Link href={`/dashboard/access/audit?grant=${encodeURIComponent(g.id)}`} className="text-[12px] text-accent hover:underline">
              What it did →
            </Link>
          </li>
        </ul>
      )}
      <ErrorNote error={error} />
    </li>
  );
}

/** Change a token's or connection's rules and name in place (its secret stays). */
function EditGrant({ g, onDone }: { g: Grant; onDone: () => Promise<void> }) {
  const [catalog, setCatalog] = useState<Catalog | null>(null);
  const [rules, setRules] = useState<Rule[]>(g.rules);
  const [name, setName] = useState(g.name);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    accessApi
      .catalog()
      .then(setCatalog)
      .catch((e) => setError(e instanceof Error ? e.message : String(e)));
  }, []);
  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      await accessApi.update(g.id, { name: name.trim(), rules });
      await onDone();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setBusy(false);
    }
  };
  return (
    <div className="rise mt-3 ml-6 space-y-4 rounded-lg border border-line bg-bg p-4">
      <label className="block max-w-sm space-y-1.5">
        <span className="block text-[12px] text-subtle">Name</span>
        <input value={name} onChange={(e) => setName(e.target.value)} maxLength={64} className={`${input} w-full`} />
      </label>
      {catalog ? <Builder catalog={catalog} rules={rules} onChange={(r) => setRules(r)} /> : !error && <Connecting />}
      <ErrorNote error={error} />
      <div className="flex gap-2">
        <button onClick={save} disabled={busy || !name.trim() || rules.length === 0} className={primary}>
          {busy ? <span className="shimmer">Saving…</span> : "Save"}
        </button>
        <button onClick={() => void onDone()} className={btn}>
          Cancel
        </button>
      </div>
      <p className="text-[12px] text-subtle">Changes take effect on the token&apos;s next request.</p>
    </div>
  );
}
