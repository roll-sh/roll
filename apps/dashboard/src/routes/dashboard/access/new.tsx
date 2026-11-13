import { createFileRoute } from "@tanstack/react-router";
import { NewToken } from "@/components/access/new-token";
import { pageHead } from "@/lib/head";

/** The permission builder: a new scoped token. */
export const Route = createFileRoute("/dashboard/access/new")({
  head: () => pageHead({ title: "New token · MCP & access" }),
  component: NewToken,
});
