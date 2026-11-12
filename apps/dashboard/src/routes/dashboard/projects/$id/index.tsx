import { createFileRoute } from "@tanstack/react-router";
import { ProjectPage } from "@/components/workspace/project-page";
import { pageHead } from "@/lib/head";

export const Route = createFileRoute("/dashboard/projects/$id/")({
  head: () => pageHead({ title: "Project" }),
  component: Page,
});

function Page() {
  const { id } = Route.useParams();
  return <ProjectPage id={id} />;
}
