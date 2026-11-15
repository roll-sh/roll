import { createFileRoute } from "@tanstack/react-router";
import { DeployRepo } from "@/components/apps/deploy-repo";
import { pageHead } from "@/lib/head";

export const Route = createFileRoute("/dashboard/deploy")({
  head: () => pageHead({ title: "Deploy to roll" }),
  component: function DeployRoute() {
    const { repo, template, project } = Route.useSearch() as { repo?: string; template?: string; project?: string };
    return <DeployRepo repo={repo} template={template} project={project} />;
  },
});
