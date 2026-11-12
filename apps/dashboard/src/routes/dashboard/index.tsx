import { createFileRoute } from "@tanstack/react-router";
import { Home } from "@/components/workspace/home";
import { pageHead } from "@/lib/head";

export const Route = createFileRoute("/dashboard/")({
  head: () => pageHead({ title: "Dashboard" }),
  component: Home,
});
