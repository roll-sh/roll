import { createFileRoute } from "@tanstack/react-router";
import { SearchClusters } from "@/components/search/clusters";
import { pageHead } from "@/lib/head";

export const Route = createFileRoute("/dashboard/search/")({
  head: () => pageHead({ title: "Search" }),
  component: SearchClusters,
});
