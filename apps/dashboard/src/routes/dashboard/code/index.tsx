import { createFileRoute } from "@tanstack/react-router";
import { CodeRepos } from "@/components/code/repos";
import { pageHead } from "@/lib/head";

export const Route = createFileRoute("/dashboard/code/")({
  head: () => pageHead({ title: "Code" }),
  component: CodeRepos,
});
