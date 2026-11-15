import { createFileRoute } from "@tanstack/react-router";
import { SecretsPage } from "@/components/secrets/secrets-page";
import { pageHead } from "@/lib/head";

export const Route = createFileRoute("/dashboard/projects/$id_/secrets")({
  head: () => pageHead({ title: "Secrets" }),
  component: Page,
});

function Page() {
  const { id } = Route.useParams();
  const search = Route.useSearch();
  return <SecretsPage id={id} search={search} />;
}
