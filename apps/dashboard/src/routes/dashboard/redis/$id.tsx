import { createFileRoute } from "@tanstack/react-router";
import { RedisDatabase } from "@/components/redis/database";
import { pageHead } from "@/lib/head";

export const Route = createFileRoute("/dashboard/redis/$id")({
  head: () => pageHead({ title: "Redis" }),
  component: function RedisDatabasePage() {
    const { id } = Route.useParams();
    return <RedisDatabase key={id} id={id} />;
  },
});
