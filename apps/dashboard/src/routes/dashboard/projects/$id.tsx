import { createFileRoute, Outlet, useLocation } from "@tanstack/react-router";
import { ProjectPage } from "@/components/workspace/project-page";
import { pageHead } from "@/lib/head";

/** A project: its canvas here, Previews as a child under the same header (the canvas stays
 * mounted between the two); Secrets is a page of its own. */
export const Route = createFileRoute("/dashboard/projects/$id")({
  head: () => pageHead({ title: "Project" }),
  component: Page,
});

function Page() {
  const { id } = Route.useParams();
  const view = useLocation({ select: (l) => /\/(previews|secrets)\/?$/.exec(l.pathname)?.[1] });
  if (view === "secrets") return <Outlet />;
  return (
    <ProjectPage id={id} tab={view === "previews" ? "previews" : "canvas"}>
      <Outlet />
    </ProjectPage>
  );
}
