import { createFileRoute } from "@tanstack/react-router";
import { ProjectMetrics } from "@/components/apps/project-metrics";
import { pageHead } from "@/lib/head";
import { useWorkspace } from "@/lib/workspace-store";

/** A project's services side by side in one environment: the worst first (Railway's, Vercel's Observability). */
export const Route = createFileRoute("/dashboard/projects/$id/metrics")({
  head: () => pageHead({ title: "Metrics" }),
  component: Page,
});

function Page() {
  const { id } = Route.useParams();
  const ws = useWorkspace();
  const project = ws.projects.find((p) => p.id === id);
  if (!project) return null;
  return <ProjectMetrics key={project.id} project={project} />;
}
