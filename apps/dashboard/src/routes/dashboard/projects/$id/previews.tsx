import { createFileRoute } from "@tanstack/react-router";
import { ProjectPreviews } from "@/components/workspace/previews";
import { pageHead } from "@/lib/head";
import { useWorkspace } from "@/lib/workspace-store";

/** A project's previews (docs/previews.md); `?preview=pv_…` opens one. */
export const Route = createFileRoute("/dashboard/projects/$id/previews")({
  head: () => pageHead({ title: "Previews" }),
  component: Page,
});

function Page() {
  const { id } = Route.useParams();
  const { preview } = Route.useSearch() as { preview?: string };
  const ws = useWorkspace();
  const project = ws.projects.find((p) => p.id === id);
  return project ? <ProjectPreviews key={project.id} project={project} inventory={ws.inventory} open={preview || undefined} /> : null;
}
