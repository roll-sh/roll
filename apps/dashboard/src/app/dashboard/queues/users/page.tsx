import type { Metadata } from "next";
import { Users } from "@/components/queues/users";

export const metadata: Metadata = { title: "Users · Queues" };

export default function QueuesUsersPage() {
  return <Users />;
}
