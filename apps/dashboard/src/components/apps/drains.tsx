import { Send, Trash2 } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { appsApi, type DrainSource, type LogDrain } from "@/lib/apps-api";
import { btn, CopyButton, input, Panel, primary, useCopy } from "../ui";
import { since, SureButton, type Runner } from "./shared";

const SOURCES: { value: DrainSource; label: string; title: string }[] = [
  { value: "runtime", label: "Runtime output", title: "What its instances write, and roll's lines about them" },
  { value: "http", label: "Requests", title: "The access log: a line a request" },
  { value: "build", label: "Builds", title: "Each deploy's build output" },
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

/** A project's log drains: HTTPS endpoints its services' output is POSTed to, each with what it's sent and for whom. */
export function DrainsPanel({ project, projectName, runner }: { project: string; projectName: string; runner: Runner }) {
  const [drains, setDrains] = useState<LogDrain[] | null>(null);
  const [all, setAll] = useState<{ id: string; name: string }[]>([]);
  const [url, setUrl] = useState("");
  const [sources, setSources] = useState<DrainSource[]>(["runtime"]);
  const [envs, setEnvs] = useState("");
  const [only, setOnly] = useState<string[]>([]);
  const [previews, setPreviews] = useState(false);
  const [headerText, setHeaderText] = useState("");
  const [secret, setSecret] = useState<string | null>(null);
  const [tested, setTested] = useState<Record<string, string>>({});
  const { copied, copy } = useCopy();
  const load = useCallback(() => appsApi.drains(project).then(setDrains, () => setDrains([])), [project]);
  useEffect(() => {
    void load();
    appsApi.services(project).then((s) => setAll(s.map((x) => ({ id: x.id, name: x.name }))), () => setAll([]));
  }, [load, project]);
  const parsed = parseHeaders(headerText);
  const ok = /^https?:\/\/\S+$/.test(url.trim()) && sources.length > 0 && parsed.bad.length === 0;
  const label = (d: LogDrain) => {
    const what = d.sources.map((s) => SOURCES.find((x) => x.value === s)?.label.toLowerCase() ?? s).join(", ");
    const who = d.services.length ? d.services.map((id) => all.find((s) => s.id === id)?.name ?? id).join(", ") : "every service";
    const where = d.environments.length ? d.environments.join(", ") : "every environment";
    return `${what} · ${who} · ${where}${d.previews ? ", previews too" : ""}${d.headers.length ? ` · headers ${d.headers.join(", ")}` : ""}`;
  };

  return (
    <Panel title="Log drains">
      <div className="space-y-3 p-5">
        <p className="text-[13px] text-muted">
          <span className="text-fg">{projectName}</span>'s services send what each drain asks for, as it's written, to its HTTPS endpoint: batches of NDJSON, signed like webhooks, with the drain's own
          headers (a vendor's API key). Best effort: a batch that can't be delivered after three tries is dropped.
        </p>
        {drains && drains.length > 0 && (
          <ul className="divide-y divide-line rounded-lg border border-line">
            {drains.map((d) => (
              <li key={d.id} className="flex flex-wrap items-center gap-x-3 gap-y-1 px-3 py-2">
                <span className="min-w-0 truncate font-mono text-[12px]">{d.host}</span>
                <span className="min-w-0 flex-1 text-[11.5px] text-subtle">{label(d)}</span>
                {tested[d.id] && <span className="text-[11.5px] text-muted">{tested[d.id]}</span>}
                <span className="text-[11.5px] text-subtle">added {since(d.created_at, Date.now())}</span>
                <button
                  type="button"
                  className={btn}
                  disabled={runner.busy !== null}
                  title="Send one test line now (not retried)"
                  onClick={() =>
                    void runner.run("drain-test", () => appsApi.testDrain(project, d.id)).then((r) => r && setTested((t) => ({ ...t, [d.id]: r.ok ? `delivered (${r.code})` : `not delivered: ${r.error ?? `answered ${r.code}`}` })))
                  }
                >
                  <Send className="size-3.5" /> Test
                </button>
                <SureButton
                  className="p-1 text-subtle hover:text-fg"
                  label={`Remove the drain to ${d.host}`}
                  sure="Sure? Remove"
                  disabled={runner.busy !== null}
                  onSure={() => runner.run("drain", () => appsApi.removeDrain(project, d.id).then(load))}
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
            void runner.run("drain", async () => {
              const d = await appsApi.addDrain(project, {
                url: url.trim(),
                sources,
                ...(environments.length ? { environments } : {}),
                ...(only.length ? { services: only } : {}),
                ...(previews ? { previews } : {}),
                ...(Object.keys(parsed.headers).length ? { headers: parsed.headers } : {}),
              });
              setSecret(d.secret ?? null);
              setUrl("");
              setHeaderText("");
              await load();
            });
          }}
        >
          <div className="flex gap-2">
            <input value={url} onChange={(e) => setUrl(e.target.value)} placeholder="https://http-intake.logs.datadoghq.com/api/v2/logs" spellCheck={false} className={`${input} min-w-0 flex-1 font-mono text-[12px]`} aria-label="Drain URL" />
            <button type="submit" className={primary} disabled={!ok || runner.busy !== null}>
              Add
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
            placeholder={"Headers, one a line (sealed, never shown again):\nDD-API-KEY: …"}
            aria-label="Headers, Name: value a line"
            spellCheck={false}
            className={`${input} w-full resize-y py-1.5 font-mono text-[12px] ${parsed.bad.length ? "border-danger/60" : ""}`}
          />
          {parsed.bad.length > 0 && <p className="text-[11.5px] text-danger">Not a header (Name: value): {parsed.bad.join(", ")}</p>}
        </form>
      </div>
    </Panel>
  );
}
