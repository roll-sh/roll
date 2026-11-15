import { createFileRoute } from "@tanstack/react-router";
import { DeployRepo } from "@/components/apps/deploy-repo";
import { pageHead } from "@/lib/head";

export const Route = createFileRoute("/dashboard/deploy")({
  head: () => pageHead({ title: "Deploy to roll" }),
  component: function DeployRoute() {
    const { repo } = Route.useSearch() as { repo?: string };
    return <DeployRepo repo={repo} />;
  },
});
