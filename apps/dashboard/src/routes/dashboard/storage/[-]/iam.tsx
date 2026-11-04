import { createFileRoute } from "@tanstack/react-router";
import { StorageIam } from "@/components/storage/iam";
import { pageHead } from "@/lib/head";

export const Route = createFileRoute("/dashboard/storage/-/iam")({
  head: () => pageHead({ title: "IAM · Storage" }),
  component: StorageIam,
});
