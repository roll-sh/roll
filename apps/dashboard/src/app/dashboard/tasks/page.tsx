import type { Metadata } from "next";
import { Tasks } from "@/components/tasks/tasks";

export const metadata: Metadata = { title: "Tasks" };

export default function TasksPage() {
  return <Tasks />;
}
