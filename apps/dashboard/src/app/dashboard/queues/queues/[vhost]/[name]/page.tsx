import type { Metadata } from "next";
import { Suspense } from "react";
import { QueueDetail } from "@/components/queues/queue-detail";

export const metadata: Metadata = { title: "Queue · Queues" };

/** The vhost and queue name are request-time params, so the detail view renders inside Suspense. */
export default function QueueDetailPage() {
  return (
    <Suspense fallback={<div className="min-h-svh" />}>
      <QueueDetail />
    </Suspense>
  );
}
