import { Download } from "lucide-react";
import { appsApi } from "@/lib/apps-api";
import { useWorkspace } from "@/lib/workspace-store";
import { btn, Panel } from "../ui";
import { DrainsPanel } from "./drains";
import { NotificationsPanel } from "./notifications";
import { RunnerError, useRunner } from "./shared";
import { SpendPanel } from "./spend";

/** A project's roll apps settings, which cover all of its services: where its notifications
 *  go, its log drains, and its spending limits. The same panels as a service's Settings → The
 *  project. */
export function ProjectApps({ project, projectName }: { project: string; projectName: string }) {
  const runner = useRunner(() => {});
  const ws = useWorkspace();
  // The project's services as a file to keep with the code (`roll up --all` makes what's missing).
  const download = () =>
    runner.run("export", async () => {
      const names: Record<string, string> = {};
      for (const [key, v] of ws.inventory) if (v.kind === "postgres" || v.kind === "redis") names[key] = v.name;
      const { roll_toml } = await appsApi.exportProject(project, names);
      const a = document.createElement("a");
      a.href = URL.createObjectURL(new Blob([roll_toml], { type: "text/plain" }));
      a.download = "roll.toml";
      a.click();
      URL.revokeObjectURL(a.href);
    });
  return (
    <div className="space-y-4">
      <Panel
        title="As a file"
        aside={
          <button type="button" className={btn} disabled={runner.busy !== null} onClick={() => void download()}>
            <Download className="size-3.5" /> {runner.busy === "export" ? <span className="shimmer">Writing…</span> : "Download roll.toml"}
          </button>
        }
      >
        <p className="px-5 py-4 text-[13px] text-muted">
          {projectName}'s services, their settings and the databases they link, as the roll.toml `roll up --all` reads: keep it with the code. Variables go by name only, never a value.
        </p>
      </Panel>
      <NotificationsPanel project={project} projectName={projectName} runner={runner} />
      <DrainsPanel project={project} projectName={projectName} runner={runner} />
      <SpendPanel project={project} projectName={projectName} runner={runner} />
      <RunnerError runner={runner} />
    </div>
  );
}
