import { createFileRoute } from "@tanstack/react-router";
import { Dashboard } from "@/components/dashboard";
import { pageHead } from "@/lib/head";

export const Route = createFileRoute("/dashboard/")({
  head: () => pageHead({ title: "Dashboard" }),
  component: Dashboard,
});
