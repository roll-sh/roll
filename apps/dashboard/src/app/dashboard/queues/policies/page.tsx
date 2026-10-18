import type { Metadata } from "next";
import { Policies } from "@/components/queues/policies";

export const metadata: Metadata = { title: "Policies · Queues" };

export default function QueuesPoliciesPage() {
  return <Policies />;
}
