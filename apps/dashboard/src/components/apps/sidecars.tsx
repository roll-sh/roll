import { Plus, Trash2 } from "lucide-react";
import { useState } from "react";
import { appsApi, type Service, type Sidecar } from "@/lib/apps-api";
import { input, Panel, primary } from "../ui";
import { SureButton, type Runner } from "./shared";

const NAME = /^[a-z0-9-]{1,20}$/;
const VAR = /^[A-Za-z_][A-Za-z0-9_]*$/;

/** `KEY=value` lines as a sidecar's env, and the lines that aren't. */
export function parseEnvLines(text: string): { env: Record<string, string>; bad: string[] } {
  const env: Record<string, string> = {};
  const bad: string[] = [];
  for (const line of text.split("\n").map((l) => l.trim()).filter(Boolean)) {
    const i = line.indexOf("=");
    const k = i > 0 ? line.slice(0, i).trim() : "";
    if (!VAR.test(k)) bad.push(line);
    else env[k] = line.slice(i + 1);
  }
  return { env, bad };
}

/**
 * Containers beside each instance (Cloud Run's sidecars): in its network namespace, so the app
 * reaches one on localhost, started, frozen and stopped with it. Each gets its own settings and
 * only the variables it names; its image is pinned with each deployment. Saved at once; the next
 * deploy runs them.
 */
export function SidecarsPanel({ service, runner }: { service: Service; runner: Runner }) {
  const list = service.run.sidecars ?? [];
  const [name, setName] = useState("");
  const [image, setImage] = useState("");
  const [command, setCommand] = useState("");
  const [vars, setVars] = useState("");
  const [envText, setEnvText] = useState("");
  const parsed = parseEnvLines(envText);
  const variables = vars.split(",").map((v) => v.trim()).filter(Boolean);
  const ok = NAME.test(name) && !list.some((s) => s.name === name) && image.trim() !== "" && !/\s/.test(image.trim()) && parsed.bad.length === 0 && variables.every((v) => VAR.test(v)) && list.length < 4;
  const save = (next: Sidecar[]) => runner.run("sidecars", () => appsApi.update(service.id, { run: { sidecars: next } }));
  const busy = runner.busy !== null;
  return (
    <Panel title="Sidecars" aside={<span className="text-[12px] text-muted">beside each instance, on its localhost; the next deploy runs them</span>}>
      {list.length > 0 && (
        <ul className="divide-y divide-line border-b border-line">
          {list.map((s) => (
            <li key={s.name} className="flex flex-wrap items-center gap-x-3 gap-y-1 px-5 py-2.5 text-[12.5px]">
              <span className="font-medium">{s.name}</span>
              <code className="min-w-0 flex-1 truncate font-mono text-[12px]">{s.image}</code>
              {s.command && <code className="min-w-0 max-w-[40%] truncate font-mono text-[12px] text-muted">{s.command}</code>}
              <span className="text-[11.5px] text-subtle">
                {[...Object.keys(s.env ?? {}), ...(s.variables ?? []).map((v) => `$${v}`)].join(", ") || "no variables"} · {s.cpu ?? 0.25} vCPU · {s.memory_mb ?? 128} MB
              </span>
              <SureButton className="p-1 text-subtle hover:text-fg" label={`Remove the sidecar ${s.name}`} sure="Sure? Remove" disabled={busy} onSure={() => save(list.filter((x) => x.name !== s.name))}>
                <Trash2 className="size-3.5" />
              </SureButton>
            </li>
          ))}
        </ul>
      )}
      <form
        className="space-y-2 px-5 py-4"
        onSubmit={(e) => {
          e.preventDefault();
          const next: Sidecar = { name, image: image.trim(), ...(command.trim() ? { command: command.trim() } : {}), ...(Object.keys(parsed.env).length ? { env: parsed.env } : {}), ...(variables.length ? { variables } : {}) };
          void save([...list, next]).then((r) => {
            if (!r) return;
            setName("");
            setImage("");
            setCommand("");
            setVars("");
            setEnvText("");
          });
        }}
      >
        <div className="flex flex-wrap gap-2">
          <input value={name} onChange={(e) => setName(e.target.value.toLowerCase())} placeholder="db-proxy" aria-label="Sidecar name" spellCheck={false} className={`${input} w-36 font-mono text-[12px]`} />
          <input value={image} onChange={(e) => setImage(e.target.value)} placeholder="gcr.io/cloud-sql-connectors/cloud-sql-proxy:2" aria-label="Image" spellCheck={false} className={`${input} min-w-48 flex-1 font-mono text-[12px]`} />
          <button type="submit" className={primary} disabled={!ok || busy}>
            <Plus className="size-3.5" /> Add
          </button>
        </div>
        <input value={command} onChange={(e) => setCommand(e.target.value)} placeholder="Command (default: the image's own)" aria-label="Command" spellCheck={false} className={`${input} w-full font-mono text-[12px]`} />
        <input
          value={vars}
          onChange={(e) => setVars(e.target.value)}
          placeholder="The environment's variables it gets, by name: DATABASE_URL, …"
          aria-label="Variables it gets"
          spellCheck={false}
          className={`${input} w-full font-mono text-[12px]`}
        />
        <textarea
          value={envText}
          onChange={(e) => setEnvText(e.target.value)}
          rows={2}
          placeholder={"Plain settings, shown as they are (never a secret): KEY=value a line"}
          aria-label="Settings, KEY=value a line"
          spellCheck={false}
          className={`${input} w-full resize-y py-1.5 font-mono text-[12px] ${parsed.bad.length ? "border-danger/60" : ""}`}
        />
        {parsed.bad.length > 0 && <p className="text-[11.5px] text-danger">Not KEY=value: {parsed.bad.join(", ")}</p>}
      </form>
    </Panel>
  );
}
