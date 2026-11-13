import { createFileRoute } from "@tanstack/react-router";
import { feedbackFrom } from "@/components/workspace/preview-feedback";
import { ProjectPreviews } from "@/components/workspace/previews";
import { pageHead } from "@/lib/head";
import { useWorkspace } from "@/lib/workspace-store";

/** A project's previews (docs/previews.md); `?preview=pv_…` opens one, `&go=<service>` sends
 * the browser on to that service with its link (how a private preview's 401 page lets a signed-in
 * person in), and `&fb_…` is a note from the preview page's feedback button to post. */
export const Route = createFileRoute("/dashboard/projects/$id/previews")({
  head: () => pageHead({ title: "Previews" }),
  component: Page,
});

function Page() {
  const { id } = Route.useParams();
  const search = Route.useSearch() as Record<string, unknown>;
  const { preview, go } = search as { preview?: string; go?: string };
  const ws = useWorkspace();
  const project = ws.projects.find((p) => p.id === id);
  return project ? <ProjectPreviews key={project.id} project={project} inventory={ws.inventory} open={preview || undefined} go={go || undefined} feedback={feedbackFrom(search)} /> : null;
}
