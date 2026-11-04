import { createFileRoute } from "@tanstack/react-router";
import { Streams } from "@/components/streams";
import { pageHead } from "@/lib/head";

export const Route = createFileRoute("/dashboard/streams")({
  head: () => pageHead({ title: "Streams" }),
  component: Streams,
});
