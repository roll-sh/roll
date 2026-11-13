import { createFileRoute } from "@tanstack/react-router";
import { AccountPage } from "@/components/account";
import { pageHead } from "@/lib/head";

export const Route = createFileRoute("/dashboard/account")({
  head: () => pageHead({ title: "Account" }),
  component: AccountPage,
});
