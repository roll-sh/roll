import { createFileRoute } from "@tanstack/react-router";
import { StorageHome } from "@/components/storage/home";
import { pageHead } from "@/lib/head";

export const Route = createFileRoute("/dashboard/storage/")({
  head: () => pageHead({ title: "Storage" }),
  component: StorageHome,
});
