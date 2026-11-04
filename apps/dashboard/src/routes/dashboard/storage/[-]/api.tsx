import { createFileRoute } from "@tanstack/react-router";
import { StorageDevApi } from "@/components/storage/api";
import { pageHead } from "@/lib/head";

export const Route = createFileRoute("/dashboard/storage/-/api")({
  head: () => pageHead({ title: "API · Storage" }),
  component: StorageDevApi,
});
