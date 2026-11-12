import { createFileRoute } from "@tanstack/react-router";
import { StorageAccounts } from "@/components/storage/accounts";
import { pageHead } from "@/lib/head";

export const Route = createFileRoute("/dashboard/storage/-/accounts")({
  head: () => pageHead({ title: "Accounts · Storage" }),
  component: StorageAccounts,
});
