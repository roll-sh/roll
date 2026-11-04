import { createFileRoute } from "@tanstack/react-router";
import { Tasks } from "@/components/tasks/tasks";
import { pageHead } from "@/lib/head";

export const Route = createFileRoute("/dashboard/tasks")({
  head: () => pageHead({ title: "Tasks" }),
  component: Tasks,
});
