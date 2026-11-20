import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { ProjectMembers } from "@/components/workspace/members";
import { pageHead } from "@/lib/head";
import { loadWorkspace, useWorkspace } from "@/lib/workspace-store";

/** A project's members and pending invites (docs/members.md). */
export const Route = createFileRoute("/dashboard/projects/$id/members")({
  head: () => pageHead({ title: "Members" }),
  component: Page,
});

function Page() {
  const { id } = Route.useParams();
  const ws = useWorkspace();
  const navigate = useNavigate();
  const project = ws.projects.find((p) => p.id === id);
  if (!project) return null;
  return (
    <ProjectMembers
      key={project.id}
      project={project}
      onLeft={() => {
        void loadWorkspace();
        void navigate({ href: "/dashboard" });
      }}
    />
  );
}
