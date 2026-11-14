import { DrainsPanel } from "./drains";
import { NotificationsPanel } from "./notifications";
import { RunnerError, useRunner } from "./shared";
import { SpendPanel } from "./spend";

/** A project's roll apps settings, which cover all of its services: where its notifications
 *  go, its log drains, and its spending limits. The same panels as a service's Settings → The
 *  project. */
export function ProjectApps({ project, projectName }: { project: string; projectName: string }) {
  const runner = useRunner(() => {});
  return (
    <div className="space-y-4">
      <NotificationsPanel project={project} projectName={projectName} runner={runner} />
      <DrainsPanel project={project} projectName={projectName} runner={runner} />
      <SpendPanel project={project} projectName={projectName} runner={runner} />
      <RunnerError runner={runner} />
    </div>
  );
}
