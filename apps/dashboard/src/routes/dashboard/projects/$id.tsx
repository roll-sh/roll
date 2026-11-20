import { createFileRoute, Outlet, useLocation } from "@tanstack/react-router";
import { ProjectPage } from "@/components/workspace/project-page";
import { pageHead } from "@/lib/head";

/** A project: its canvas here, its other views (Previews) as child routes under the same header. */
export const Route = createFileRoute("/dashboard/projects/$id")({
  head: () => pageHead({ title: "Project" }),
  component: Page,
});

function Page() {
  const { id } = Route.useParams();
  const tab = useLocation({
    select: (l) => (/\/previews\/?$/.test(l.pathname) ? "previews" : /\/logs\/?$/.test(l.pathname) ? "logs" : /\/members\/?$/.test(l.pathname) ? "members" : "canvas"),
  });
  return (
    <ProjectPage id={id} tab={tab}>
      <Outlet />
    </ProjectPage>
  );
}
