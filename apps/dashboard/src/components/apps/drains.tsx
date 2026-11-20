import { Pencil, Send, Trash2 } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { appsApi, type DrainFormat, type DrainSource, type LogDrain } from "@/lib/apps-api";
import { btn, CopyButton, input, Panel, primary, useCopy } from "../ui";
import { since, SureButton, type Runner } from "./shared";

const SOURCES: { value: DrainSource; label: string; title: string }[] = [
  { value: "runtime", label: "Runtime output", title: "What its instances write, and roll's lines about them" },
  { value: "http", label: "Requests", title: "The access log: a line a request" },
  { value: "build", label: "Builds", title: "Each deploy's build output" },
  { value: "audit", label: "Audit log", title: "Every change people make, as it's written (a SIEM's copy)" },
  { value: "traces", label: "Traces", title: "The spans its instances send (OTLP drains get them at /v1/traces)" },
  { value: "metrics", label: "Metrics", title: "The metrics its instances send or roll scrapes (OTLP drains get them at /v1/metrics)" },
];

/** `Name: value` a line, as a vendor's docs show its key; what doesn't parse is named. */
export function parseHeaders(text: string): { headers: Record<string, string>; bad: string[] } {
  const headers: Record<string, string> = {};
  const bad: string[] = [];
  for (const raw of text.split("\n")) {
    const line = raw.trim();
    if (!line) continue;
    const i = line.indexOf(":");
    const name = i > 0 ? line.slice(0, i).trim() : "";
    const value = i > 0 ? line.slice(i + 1).trim() : "";
    if (!/^[A-Za-z0-9_-]{1,64}$/.test(name) || !value) bad.push(line);
    else headers[name] = value;
  }
  return { headers, bad };
}

/** How a drain's lines go, with the URL each takes. */
const FORMATS: { value: DrainFormat; label: string; placeholder: string; url: RegExp }[] = [
  { value: "ndjson", label: "NDJSON", placeholder: "https://http-intake.logs.datadoghq.com/api/v2/logs", url: /^https?:\/\/\S+$/ },
  { value: "otlp", label: "OpenTelemetry", placeholder: "https://otlp.example.com/v1/logs", url: /^https?:\/\/\S+$/ },
  { value: "syslog", label: "Syslog", placeholder: "syslog+tls://logs.papertrailapp.com:12345", url: /^syslog(\+tls)?:\/\/[^/\s]+:\d+\/?$/ },
];

/** A project's log drains: HTTPS endpoints its services' output is POSTed to, each with what it's sent and for whom. */
export function DrainsPanel({ project, projectName, runner }: { project: string; projectName: string; runner: Runner }) {
  const [drains, setDrains] = useState<LogDrain[] | null>(null);
  const [all, setAll] = useState<{ id: string; name: string }[]>([]);
  const [url, setUrl] = useState("");
  const [format, setFormat] = useState<DrainFormat>("ndjson");
  const [sources, setSources] = useState<DrainSource[]>(["runtime"]);
  const [envs, setEnvs] = useState("");
  const [only, setOnly] = useState<string[]>([]);
  const [previews, setPreviews] = useState(false);
  const [headerText, setHeaderText] = useState("");
  const [secret, setSecret] = useState<string | null>(null);
  const [tested, setTested] = useState<Record<string, string>>({});
  /** The drain the form edits (its URL fixed: a new URL is a new drain), or null to add one. */
  const [editing, setEditing] = useState<LogDrain | null>(null);
  const fmt = FORMATS.find((f) => f.value === (editing?.format ?? format)) ?? FORMATS[0];
  const { copied, copy } = useCopy();
  // A read that failed says so (an empty list would read as "none set").
  const [failed, setFailed] = useState<string | null>(null);
  const load = useCallback(
    () =>
      appsApi.drains(project).then(
        (d) => {
          setDrains(d);
          setFailed(null);
        },
        (e: unknown) => setFailed(e instanceof Error ? e.message : String(e)),
      ),
    [project],
  );
  useEffect(() => {
    void load();
    appsApi.services(project).then((s) => setAll(s.map((x) => ({ id: x.id, name: x.name }))), () => setAll([]));
  }, [load, project]);
  const parsed = parseHeaders(headerText);
  const ok = (editing !== null || fmt.url.test(url.trim())) && sources.length > 0 && parsed.bad.length === 0 && (fmt.value !== "syslog" || !Object.keys(parsed.headers).length);
  const clear = () => {
    setEditing(null);
    setUrl("");
    setFormat("ndjson");
    setSources(["runtime"]);
    setEnvs("");
    setOnly([]);
    setPreviews(false);
    setHeaderText("");
  };
  const edit = (d: LogDrain) => {
    setEditing(d);
    setUrl(d.host);
    setSources(d.sources);
    setEnvs(d.environments.join(", "));
    setOnly(d.services);
    setPreviews(d.previews);
    setHeaderText("");
  };
  const label = (d: LogDrain) => {
    const what = d.sources.map((s) => SOURCES.find((x) => x.value === s)?.label.toLowerCase() ?? s).join(", ");
    const who = d.services.length ? d.services.map((id) => all.find((s) => s.id === id)?.name ?? id).join(", ") : "every service";
    const where = d.environments.length ? d.environments.join(", ") : "every environment";
    const as = d.format && d.format !== "ndjson" ? `${FORMATS.find((f) => f.value === d.format)?.label ?? d.format} · ` : "";
    return `${as}${what} · ${who} · ${where}${d.previews ? ", previews too" : ""}${d.headers.length ? ` · headers ${d.headers.join(", ")}` : ""}`;
  };

  return (
    <Panel title="Log drains">
      <div className="space-y-3 p-5">
        <p className="text-[13px] text-muted">
          <span className="text-fg">{projectName}</span>'s services send what each drain asks for, as it's written: batches of NDJSON signed like webhooks, OpenTelemetry logs to a collector's or
          a vendor's <span className="font-mono">/v1/logs</span>, or syslog over TLS (Papertrail, a syslog server), with the drain's own headers (a vendor's API key) where it's HTTP. Best effort: a
          batch that can't be delivered after three tries is dropped.
        </p>
        {failed && <p className="text-[12.5px] text-danger">Its drains couldn't be read: {failed}</p>}
        {drains && drains.length > 0 && (
          <ul className="divide-y divide-line rounded-lg border border-line">
            {drains.map((d) => (
              <li key={d.id} className="flex flex-wrap items-center gap-x-3 gap-y-1 px-3 py-2">
                <span className="min-w-0 truncate font-mono text-[12px]">{d.host}</span>
                <span className="min-w-0 flex-1 text-[11.5px] text-subtle">{label(d)}</span>
                {tested[d.id] && <span className="text-[11.5px] text-muted">{tested[d.id]}</span>}
                <span className="text-[11.5px] text-subtle">added {since(d.created_at, Date.now())}</span>
                <button type="button" className={btn} disabled={runner.busy !== null} aria-pressed={editing?.id === d.id} onClick={() => (editing?.id === d.id ? clear() : edit(d))}>
                  <Pencil className="size-3.5" /> Edit
                </button>
                <button
                  type="button"
                  className={btn}
                  disabled={runner.busy !== null}
                  title="Send one test line now (not retried)"
                  onClick={() =>
                    void runner.run("drain-test", () => appsApi.testDrain(project, d.id)).then((r) => r && setTested((t) => ({ ...t, [d.id]: r.ok ? (r.code ? `delivered (${r.code})` : "delivered") : `not delivered: ${r.error ?? `answered ${r.code}`}` })))
                  }
                >
                  <Send className="size-3.5" /> Test
                </button>
                <SureButton
                  className="p-1 text-subtle hover:text-fg"
                  label={`Remove the drain to ${d.host}`}
                  sure="Sure? Remove"
                  disabled={runner.busy !== null}
                  onSure={() =>
                    runner.run("drain", async () => {
                      await appsApi.removeDrain(project, d.id);
                      if (editing?.id === d.id) clear();
                      await load();
                    })
                  }
                >
                  <Trash2 className="size-3.5" />
                </SureButton>
              </li>
            ))}
          </ul>
        )}
        {secret && (
          <div className="rise rounded-lg border border-line bg-bg px-3 py-2">
            <p className="text-[11px] text-subtle">Its signing secret, shown once: check X-Roll-Signature-256 with it.</p>
            <div className="mt-1 flex items-center gap-1">
              <code className="min-w-0 flex-1 truncate font-mono text-[11.5px]">{secret}</code>
              <CopyButton id="drain-secret" text={secret} copied={copied} onCopy={copy} />
            </div>
          </div>
        )}
        <form
          className="space-y-2"
          onSubmit={(e) => {
            e.preventDefault();
            const environments = envs.split(",").map((s) => s.trim()).filter(Boolean);
            const headers = Object.keys(parsed.headers).length ? { headers: parsed.headers } : {};
            void runner.run("drain", async () => {
              if (editing) {
                // A cleared filter goes back to every; services only when changed, and never one since
                // deleted (it would be refused, and can't be unticked); headers only when typed (they replace them all).
                const same = only.length === editing.services.length && only.every((id) => editing.services.includes(id));
                const services = same ? {} : { services: only.filter((id) => all.some((s) => s.id === id)) };
                await appsApi.updateDrain(project, editing.id, { sources, environments, previews, ...services, ...headers });
              } else {
                const d = await appsApi.addDrain(project, {
                  url: url.trim(),
                  ...(format !== "ndjson" ? { format } : {}),
                  sources,
                  ...(environments.length ? { environments } : {}),
                  ...(only.length ? { services: only } : {}),
                  ...(previews ? { previews } : {}),
                  ...headers,
                });
                setSecret(d.secret ?? null);
              }
              clear();
              await load();
            });
          }}
        >
          <div className="flex gap-2">
            <select
              value={fmt.value}
              disabled={editing !== null}
              onChange={(e) => setFormat(e.target.value as DrainFormat)}
              aria-label="Format"
              title={editing ? "A drain keeps its format: add a new one for another" : undefined}
              className={`${input} !w-auto text-[12px]`}
            >
              {FORMATS.map((f) => (
                <option key={f.value} value={f.value}>
                  {f.label}
                </option>
              ))}
            </select>
            <input
              value={url}
              onChange={(e) => setUrl(e.target.value)}
              readOnly={editing !== null}
              title={editing ? "A drain keeps its URL: add a new one for another" : undefined}
              placeholder={fmt.placeholder}
              spellCheck={false}
              className={`${input} min-w-0 flex-1 font-mono text-[12px] ${editing ? "text-muted" : ""}`}
              aria-label="Drain URL"
            />
            {editing && (
              <button type="button" className={btn} onClick={clear}>
                Cancel
              </button>
            )}
            <button type="submit" className={primary} disabled={!ok || runner.busy !== null}>
              {editing ? "Save" : "Add"}
            </button>
          </div>
          <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-[12px] text-muted" role="group" aria-label="What it's sent">
            {SOURCES.map((s) => (
              <label key={s.value} className="inline-flex items-center gap-1" title={s.title}>
                <input type="checkbox" checked={sources.includes(s.value)} onChange={(x) => setSources((v) => (x.target.checked ? [...v, s.value] : v.filter((y) => y !== s.value)))} />
                {s.label}
              </label>
            ))}
            <label className="inline-flex items-center gap-1" title="Each pull request's preview environment too">
              <input type="checkbox" checked={previews} onChange={(x) => setPreviews(x.target.checked)} />
              Include previews
            </label>
          </div>
          <div className="flex flex-wrap items-center gap-2">
            <input value={envs} onChange={(e) => setEnvs(e.target.value)} placeholder="every environment" aria-label="Only these environments (comma-separated)" spellCheck={false} className={`${input} w-44 font-mono text-[12px]`} />
            {all.length > 1 && (
              <div className="flex flex-wrap gap-x-3 gap-y-1 text-[12px] text-muted" role="group" aria-label="Only these services (none: every one)">
                {all.map((s) => (
                  <label key={s.id} className="inline-flex items-center gap-1">
                    <input type="checkbox" checked={only.includes(s.id)} onChange={(x) => setOnly((o) => (x.target.checked ? [...o, s.id] : o.filter((v) => v !== s.id)))} />
                    {s.name}
                  </label>
                ))}
              </div>
            )}
          </div>
          <textarea
            value={headerText}
            onChange={(e) => setHeaderText(e.target.value)}
            rows={2}
            placeholder={
              editing
                ? editing.headers.length
                  ? `Its headers (${editing.headers.join(", ")}) stay unless you type new ones here,\nwhich replace them all: Name: value a line`
                  : "Headers, one a line (sealed, never shown again):\nDD-API-KEY: …"
                : "Headers, one a line (sealed, never shown again):\nDD-API-KEY: …"
            }
            aria-label="Headers, Name: value a line"
            spellCheck={false}
            className={`${input} w-full resize-y py-1.5 font-mono text-[12px] ${parsed.bad.length ? "border-danger/60" : ""}`}
          />
          {parsed.bad.length > 0 && <p className="text-[11.5px] text-danger">Not a header (Name: value): {parsed.bad.join(", ")}</p>}
          {fmt.value === "syslog" && Object.keys(parsed.headers).length > 0 && <p className="text-[11.5px] text-danger">A syslog drain is sent no headers: its token is in its host and port.</p>}
        </form>
      </div>
    </Panel>
  );
}
