import { createFileRoute } from "@tanstack/react-router";
import { SearchClusterPage } from "@/components/search/cluster";
import { pageHead } from "@/lib/head";

export const Route = createFileRoute("/dashboard/search/$id")({
  head: () => pageHead({ title: "Search" }),
  component: function SearchClusterRoute() {
    const { id } = Route.useParams();
    return <SearchClusterPage key={id} id={id} />;
  },
});
