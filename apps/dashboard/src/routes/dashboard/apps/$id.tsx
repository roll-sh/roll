import { createFileRoute } from "@tanstack/react-router";
import { ServicePage } from "@/components/apps/service-page";
import { pageHead } from "@/lib/head";

export const Route = createFileRoute("/dashboard/apps/$id")({
  head: () => pageHead({ title: "Service" }),
  component: function ServiceRoute() {
    const { id } = Route.useParams();
    const { env, tab } = Route.useSearch();
    return <ServicePage key={id} id={id} env={env} tab={tab} />;
  },
});
