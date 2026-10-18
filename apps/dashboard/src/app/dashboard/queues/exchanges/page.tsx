import type { Metadata } from "next";
import { Exchanges } from "@/components/queues/exchanges";

export const metadata: Metadata = { title: "Exchanges · Queues" };

export default function QueuesExchangesPage() {
  return <Exchanges />;
}
