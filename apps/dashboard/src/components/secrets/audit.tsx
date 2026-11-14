import { Check, Copy, Download, Plus, Radio, Trash2 } from "lucide-react";
import { useEffect, useState } from "react";
import type { AuditEntry, AuditFilter, AuditFormat, Sink } from "@/lib/secrets-api";
import { Select } from "../agents/ui/select";
import { useSecrets } from "./secrets-page";
import { ActorMark, ago, button, card, dangerButton, ErrorNote, errorText, field, sized, ghost, primary, saveText, Section, stamp, Teach, useLoad } from "./ui";

const PAGE = 100;
const ALL = "*";
const SINCE = [
  { value: "all", label: "All time" },
  { value: "1", label: "Last 24 hours" },
  { value: "7", label: "Last 7 days" },
  { value: "30", label: "Last 30 days" },
] as const;
type Since = (typeof SINCE)[number]["value"];

/** Every reveal, export, resolve and write, newest first (values never appear here). */
export function AuditView() {
  const { api, envs, project } = useSecrets();
  const [env, setEnv] = useState(ALL);
  const [key, setKey] = useState("");
  const [actor, setActor] = useState("");
  const [action, setAction] = useState("");
  const [since, setSince] = useState<Since>("all");
  const [now] = useState(() => Date.now());
  const [rows, setRows] = useState<AuditEntry[] | null>(null);
  const [end, setEnd] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const filter: AuditFilter = {
    env: env === ALL ? undefined : env,
    key: key.trim() || undefined,
    actor: actor.trim() || undefined,
    action: action.trim() || undefined,
    since: since === "all" ? undefined : now - Number(since) * 86_400_000,
    limit: PAGE,
  };
  const sig = JSON.stringify(filter);

  // Filters typed into fields settle for a moment before they load.
  useEffect(() => {
    let live = true;
    const t = setTimeout(() => {
      api.audit(filter).then(
        (r) => {
          if (!live) return;
          setRows(r);
          setEnd(r.length < PAGE);
          setError(null);
        },
        (e) => live && setError(errorText(e)),
      );
    }, 200);
    return () => {
      live = false;
      clearTimeout(t);
    };
  }, [api, sig]);

  const older = () => {
    const last = rows?.[rows.length - 1];
    if (!last) return;
    api.audit({ ...filter, before: last.id }).then(
      (r) => {
        setRows((x) => [...(x ?? []), ...r]);
        setEnd(r.length < PAGE);
      },
      (e) => setError(errorText(e)),
    );
  };

  const [exporting, setExporting] = useState<AuditFormat | null>(null);
  const exportAs = async (format: AuditFormat) => {
    setExporting(format);
    setError(null);
    try {
      saveText(`${project.name}.audit.${format}`, await api.auditText(filter, format), format === "csv" ? "text/csv" : "application/x-ndjson");
    } catch (e) {
      setError(errorText(e));
    } finally {
      setExporting(null);
    }
  };

  return (
    <div className="mx-auto max-w-5xl space-y-8 px-4 py-5">
      <Streams />
      <div>
        <div className="flex flex-wrap items-center gap-2">
          <Select label="Environment" value={env} onChange={setEnv} options={[{ value: ALL, label: "All environments" }, ...envs.map((e) => ({ value: e.slug, label: e.name }))]} className="w-44" />
          <input value={key} onChange={(e) => setKey(e.target.value)} placeholder="Key" aria-label="Key" className={`${sized} w-40 font-mono`} />
          <input value={actor} onChange={(e) => setActor(e.target.value)} placeholder="Actor (operator, token:tok_…)" aria-label="Actor" className={`${sized} w-56 font-mono`} />
          <input value={action} onChange={(e) => setAction(e.target.value)} placeholder="Action (secrets.reveal, token.)" aria-label="Action" className={`${sized} w-56 font-mono`} />
          <Select label="Since" value={since} onChange={setSince} options={SINCE} className="w-36" />
          <span className="ml-auto flex items-center gap-1" title="These filters as a file, up to 10,000 rows (exporting is audited too)">
            {(["csv", "jsonl"] as const).map((f) => (
              <button key={f} className={button} disabled={!!exporting} onClick={() => exportAs(f)}>
                <Download className="size-3.5" /> {exporting === f ? "Exporting…" : f.toUpperCase()}
              </button>
            ))}
          </span>
        </div>
        <ErrorNote error={error} className="mt-3" />
        {rows && rows.length === 0 ? (
          <Teach title="Nothing in the audit log matches" cli={[`roll secrets audit -p ${project.name} --key DATABASE_URL`]}>
            Every reveal, export, resolve and write is recorded here with who did it, from where and to which keys. Values never are. Rows are kept for 400 days.
          </Teach>
        ) : (
          <div className="mt-3 overflow-x-auto">
            <table className="w-full min-w-[720px] text-[12px]">
              <thead>
                <tr className="text-left text-[11px] text-subtle">
                  <th className="py-1.5 pr-3 font-medium">When</th>
                  <th className="py-1.5 pr-3 font-medium">Actor</th>
                  <th className="py-1.5 pr-3 font-medium">Action</th>
                  <th className="py-1.5 pr-3 font-medium">Where</th>
                  <th className="py-1.5 font-medium">What</th>
                </tr>
              </thead>
              <tbody>
                {rows?.map((r) => (
                  <tr key={r.id} className="border-t border-line align-top">
                    <td className="py-1.5 pr-3 whitespace-nowrap text-muted" title={stamp(r.at)}>
                      {ago(r.at)}
                    </td>
                    <td className="max-w-40 py-1.5 pr-3 text-soft">
                      <ActorMark who={r.actor} />
                    </td>
                    <td className="py-1.5 pr-3 font-mono text-[11.5px] whitespace-nowrap">{r.action}</td>
                    <td className="py-1.5 pr-3 font-mono text-[11px] whitespace-nowrap text-muted">
                      {r.environment ?? "—"}
                      {r.path && r.path !== "/" ? r.path : ""}
                    </td>
                    <td className="py-1.5">
                      <span className="text-soft">{r.summary}</span>
                      {r.keys.length > 0 && (
                        <span className="mt-0.5 block truncate font-mono text-[10.5px] text-subtle" title={r.keys.join(", ")}>
                          {r.keys.join(" ")}
                        </span>
                      )}
                      {r.ip && <span className="ml-1.5 font-mono text-[10.5px] text-faint">{r.ip}</span>}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
            {rows && !end && (
              <button onClick={older} className={`${ghost} mx-auto mt-3 flex`}>
                Older entries
              </button>
            )}
          </div>
        )}
      </div>
    </div>
  );
}

/** Audit streams to a SIEM (`admin`): new rows in order, in signed batches. */
function Streams() {
  const { api } = useSecrets();
  const sinks = useLoad(() => api.sinks(), [api]);
  const [adding, setAdding] = useState(false);
  const [url, setUrl] = useState("");
  const [fromStart, setFromStart] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [made, setMade] = useState<{ host: string; secret: string } | null>(null);
  const [copied, setCopied] = useState(false);
  const [deleting, setDeleting] = useState<string | null>(null);
  // An HTTPS endpoint, or a roll bucket of this project's canvas (an archive).
  const urlOk = /^https:\/\/[^\s/]+/.test(url.trim()) || /^bucket:[a-z0-9][a-z0-9.-]{1,62}$/.test(url.trim());

  const add = async () => {
    setBusy(true);
    setError(null);
    try {
      const s = await api.createSink(url.trim(), fromStart);
      setMade({ host: host(s.url), secret: s.secret });
      setAdding(false);
      setUrl("");
      setFromStart(false);
      sinks.reload();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  const del = (s: Sink) =>
    api.deleteSink(s.id).then(
      () => (setDeleting(null), sinks.reload()),
      (e) => setError(errorText(e)),
    );

  return (
    <Section
      title="Streams"
      aside={
        <button className={button} onClick={() => setAdding((a) => !a)}>
          <Plus className="size-3.5" /> Add a stream
        </button>
      }
    >
      <p className="mb-2 text-[11.5px] text-muted">New audit rows go to each HTTPS endpoint in order, in batches of up to 200; a failed batch is sent again 5 minutes later.</p>
      <ErrorNote error={error ?? sinks.error} onDismiss={error ? () => setError(null) : undefined} className="mb-2" />
      {made && (
        <div className="fade-in mb-3 rounded-xl border border-accent/40 bg-panel p-3 shadow-card inset-shadow-highlight" role="status">
          <p className="text-[12.5px]">
            <b className="font-medium">{made.host}</b> <span className="text-muted">is added. Copy its signing secret now: it isn&apos;t shown again.</span>
          </p>
          <div className="mt-2 flex items-center gap-2">
            <code aria-label="Signing secret" className="min-w-0 flex-1 truncate rounded-md border border-line-2 bg-panel-2 px-2 py-1.5 font-mono text-[12px] select-all">
              {made.secret}
            </code>
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
          <p className="mt-2 text-[11px] text-subtle">
            Check each batch: <span className="font-mono">X-Roll-Signature-256</span> is <span className="font-mono">sha256=</span>HMAC-SHA256(secret, <span className="font-mono">X-Roll-Timestamp + &quot;.&quot; + body</span>) in hex.
          </p>
        </div>
      )}
      {adding && (
        <form
          className="rise mb-3 flex flex-wrap items-end gap-2 rounded-xl border border-line-2 bg-panel p-3"
          onSubmit={(e) => {
            e.preventDefault();
            if (urlOk && !busy) void add();
          }}
        >
          <label className="min-w-64 flex-1">
            <span className="mb-1 block text-[11px] font-medium text-subtle">HTTPS endpoint, or bucket:&lt;name&gt; for a roll bucket on this canvas</span>
            <input autoFocus value={url} onChange={(e) => setUrl(e.target.value)} placeholder="https://siem.example.com/roll  or  bucket:audit-archive" spellCheck={false} className={`${field} font-mono text-[12px]`} />
          </label>
          <label className="flex h-8 items-center gap-1.5 text-[12px] text-muted" title="Otherwise it starts with the rows written from now on">
            <input type="checkbox" checked={fromStart} onChange={(e) => setFromStart(e.target.checked)} className="accent-[var(--accent)]" />
            Send the existing log first
          </label>
          <button type="submit" className={primary} disabled={!urlOk || busy}>
            {busy ? "Adding…" : "Add stream"}
          </button>
          <button type="button" className={ghost} onClick={() => setAdding(false)}>
            Cancel
          </button>
        </form>
      )}
      {sinks.data && sinks.data.length === 0 && !adding ? (
        <p className="rounded-xl border border-dashed border-line-2 px-3 py-4 text-[12px] text-muted">No streams. Add one to send this project&apos;s audit log to a SIEM as it&apos;s written.</p>
      ) : (
        sinks.data && (
          <div className={`${card} divide-y divide-line`}>
            {sinks.data.map((s) => (
              <div key={s.id} className="flex flex-wrap items-center gap-x-3 gap-y-1 px-3 py-2">
                <Radio className={`size-3.5 ${s.status === "error" ? "text-danger" : s.status === "ok" ? "text-ok" : "text-subtle"}`} />
                <span className="font-mono text-[12px]" title={s.url}>
                  {host(s.url)}
                </span>
                <span className={`text-[11.5px] ${s.status === "error" ? "text-danger" : "text-muted"}`}>{s.status === "new" ? "nothing sent yet" : s.status}</span>
                {s.error && (
                  <span className="min-w-0 basis-full truncate pl-6.5 text-[11.5px] text-danger" title={s.error}>
                    {s.error}
                  </span>
                )}
                <span className="ml-auto text-[11px] text-subtle">
                  {s.delivered_at ? `last delivered ${ago(s.delivered_at)}` : `added ${ago(s.created_at)}`} · <ActorMark who={s.created_by} />
                </span>
                {deleting === s.id ? (
                  <span className="flex gap-1">
                    <button className={dangerButton} onClick={() => del(s)}>
                      Delete
                    </button>
                    <button className={ghost} onClick={() => setDeleting(null)}>
                      Keep
                    </button>
                  </span>
                ) : (
                  <button className={ghost} onClick={() => setDeleting(s.id)} aria-label={`Delete the stream to ${host(s.url)}`}>
                    <Trash2 className="size-3.5" />
                  </button>
                )}
              </div>
            ))}
          </div>
        )
      )}
    </Section>
  );
}

function host(url: string) {
  try {
    return new URL(url).host;
  } catch {
    return url;
  }
}
