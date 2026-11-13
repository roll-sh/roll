import { createFileRoute } from "@tanstack/react-router";
import { TokenPage } from "@/components/access/token-page";
import { pageHead } from "@/lib/head";

/** One token: what it may do, what it's allowed for a while, its approvals and requests. */
export const Route = createFileRoute("/dashboard/access/tokens/$id")({
  head: () => pageHead({ title: "Token · MCP & access" }),
  component: function TokenRoute() {
    const { id } = Route.useParams();
    return <TokenPage id={id} />;
  },
});
