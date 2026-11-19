import { ExternalLink } from "lucide-react";
import { useEffect, useState } from "react";
import { appsApi, type StatusPage } from "@/lib/apps-api";
import { input, Panel, primary } from "../ui";
import { SureButton, type Runner } from "./shared";

/**
 * A project's status page (Better Stack's, Instatus'): its uptime checks and heartbeats, up or
 * down and their last 90 days, on a public address of its own. Names only, never a path or an
 * error.
 */
export function StatusPagePanel({ project, projectName, runner }: { project: string; projectName: string; runner: Runner }) {
  const [page, setPage] = useState<StatusPage | null | undefined>(undefined);
  const [title, setTitle] = useState("");
  useEffect(() => {
    appsApi.statusPage(project).then(setPage, () => setPage(null));
  }, [project]);
  const busy = runner.busy !== null;
  const save = (t: string) => runner.run("status-page", async () => setPage(await appsApi.putStatusPage(project, { title: t.trim() || undefined })));
  if (page === undefined) return null;
  return (
    <Panel title="Status page" aside={<span className="text-[12px] text-muted">its uptime checks and heartbeats, in public</span>}>
      {page ? (
        <div className="flex flex-wrap items-center gap-3 px-5 py-4 text-[12.5px]">
          <a href={page.url} target="_blank" rel="noreferrer" className="inline-flex min-w-0 flex-1 items-center gap-1 truncate font-mono text-[12px] text-accent hover:underline">
            {page.url} <ExternalLink className="size-3.5 shrink-0" />
          </a>
          <span className="text-muted">{page.title}</span>
          <SureButton className="text-[12px] text-muted hover:text-fg" label="Take the status page down" sure="Sure? Take it down" disabled={busy} onSure={() => runner.run("status-page", async () => { await appsApi.deleteStatusPage(project); setPage(null); })}>
            Take down
          </SureButton>
        </div>
      ) : (
        <form
          className="flex flex-wrap items-center gap-2 px-5 py-4"
          onSubmit={(e) => {
            e.preventDefault();
            void save(title);
          }}
        >
          <input value={title} onChange={(e) => setTitle(e.target.value)} placeholder={`${projectName} status`} aria-label="Title" maxLength={80} className={`${input} min-w-48 flex-1 text-[12px]`} />
          <button type="submit" className={primary} disabled={busy}>
            Publish a status page
          </button>
          <span className="w-full text-[12px] text-muted">Every check and heartbeat of the project's environments, by name: never a path, an address or an error.</span>
        </form>
      )}
    </Panel>
  );
}
