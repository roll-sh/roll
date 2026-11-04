import { createFileRoute } from "@tanstack/react-router";
import { StorageBucket } from "@/components/storage/bucket";
import { pageHead } from "@/lib/head";

export const Route = createFileRoute("/dashboard/storage/$bucket")({
  head: () => pageHead({ title: "Storage" }),
  component: function StorageBucketPage() {
    const { bucket } = Route.useParams();
    return <StorageBucket key={bucket} name={bucket} />;
  },
});
