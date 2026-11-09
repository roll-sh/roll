import { createFileRoute } from "@tanstack/react-router";
import { NosqlDatabase } from "@/components/nosql/database";
import { pageHead } from "@/lib/head";

export const Route = createFileRoute("/dashboard/nosql/$id")({
  head: () => pageHead({ title: "NoSQL" }),
  component: function NosqlDatabasePage() {
    const { id } = Route.useParams();
    return <NosqlDatabase key={id} id={id} />;
  },
});
