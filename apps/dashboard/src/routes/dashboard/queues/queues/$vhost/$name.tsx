import { createFileRoute } from "@tanstack/react-router";
import { QueueDetail } from "@/components/queues/queue-detail";
import { pageHead } from "@/lib/head";

export const Route = createFileRoute("/dashboard/queues/queues/$vhost/$name")({
  head: () => pageHead({ title: "Queue · Queues" }),
  component: function QueueDetailPage() {
    const { vhost, name } = Route.useParams();
    return <QueueDetail key={`${vhost}\u0000${name}`} vhost={vhost} name={name} />;
  },
});
