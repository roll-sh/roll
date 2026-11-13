import { createFileRoute } from "@tanstack/react-router";
import { SignInPage } from "@/components/signin";
import { pageHead } from "@/lib/head";

export const Route = createFileRoute("/signin")({
  head: () => pageHead({ title: "Sign in" }),
  component: Page,
});

function Page() {
  const s = Route.useSearch() as Record<string, string | undefined>;
  return <SignInPage invite={s.invite} step={s.step} returnTo={s.return_to} error={s.error} />;
}
