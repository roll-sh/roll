import { createFileRoute } from "@tanstack/react-router";
import { ProjectLogs } from "@/components/apps/project-logs";
import { pageHead } from "@/lib/head";
import { useWorkspace } from "@/lib/workspace-store";

/** A project's logs: every service's runtime log in one environment, followed together. */
export const Route = createFileRoute("/dashboard/projects/$id/logs")({
  head: () => pageHead({ title: "Logs" }),
  component: Page,
});

function Page() {
  const { id } = Route.useParams();
  const ws = useWorkspace();
  const project = ws.projects.find((p) => p.id === id);
  if (!project) return null;
  return <ProjectLogs key={project.id} project={project} />;
}
