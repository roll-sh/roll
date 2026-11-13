import { Trash2 } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { appsApi, type LogDrain } from "@/lib/apps-api";
import { CopyButton, input, Panel, primary, useCopy } from "../ui";
import { since, type Runner } from "./shared";

/** A project's log drains: HTTPS endpoints its services' runtime output is POSTed to. */
export function DrainsPanel({ project, projectName, runner }: { project: string; projectName: string; runner: Runner }) {
  const [drains, setDrains] = useState<LogDrain[] | null>(null);
  const [url, setUrl] = useState("");
  const [secret, setSecret] = useState<string | null>(null);
  const { copied, copy } = useCopy();
  const load = useCallback(() => appsApi.drains(project).then(setDrains, () => setDrains([])), [project]);
  useEffect(() => {
    void load();
  }, [load]);
  const ok = /^https?:\/\/\S+$/.test(url.trim());

  return (
    <Panel title="Log drains">
      <div className="space-y-3 p-5">
        <p className="text-[13px] text-muted">
          Every service in <span className="text-fg">{projectName}</span> sends its output, as it's written, to these HTTPS endpoints: batches of NDJSON, signed like webhooks. Best effort: a batch
          that can't be delivered after three tries is dropped.
        </p>
        {drains && drains.length > 0 && (
          <ul className="divide-y divide-line rounded-lg border border-line">
            {drains.map((d) => (
              <li key={d.id} className="flex items-center gap-3 px-3 py-2">
                <span className="min-w-0 flex-1 truncate font-mono text-[12px]">{d.host}</span>
                <span className="text-[11.5px] text-subtle">added {since(d.created_at, Date.now())}</span>
                <button
                  className="p-1 text-subtle hover:text-fg"
                  aria-label={`Remove the drain to ${d.host}`}
                  disabled={runner.busy !== null}
                  onClick={() => runner.run("drain", () => appsApi.removeDrain(project, d.id).then(load))}
                >
                  <Trash2 className="size-3.5" />
                </button>
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
          className="flex gap-2"
          onSubmit={(e) => {
            e.preventDefault();
            void runner.run("drain", async () => {
              const d = await appsApi.addDrain(project, url.trim());
              setSecret(d.secret ?? null);
              setUrl("");
              await load();
            });
          }}
        >
          <input value={url} onChange={(e) => setUrl(e.target.value)} placeholder="https://logs.example.com/roll" spellCheck={false} className={`${input} min-w-0 flex-1 font-mono text-[12px]`} aria-label="Drain URL" />
          <button type="submit" className={primary} disabled={!ok || runner.busy !== null}>
            Add
          </button>
        </form>
      </div>
    </Panel>
  );
}
