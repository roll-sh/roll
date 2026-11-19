import { createFileRoute } from "@tanstack/react-router";
import { Invites } from "@/components/workspace/invites";
import { pageHead } from "@/lib/head";

/** Invites to projects waiting for the signed-in person; `?token=` (an invite's link) comes first. */
export const Route = createFileRoute("/dashboard/invites")({
  head: () => pageHead({ title: "Invites" }),
  component: function InvitesRoute() {
    const { token } = Route.useSearch() as { token?: string };
    return <Invites token={typeof token === "string" ? token : undefined} />;
  },
});
