import { createFileRoute } from "@tanstack/react-router";
import { AgentsPage } from "@/components/agents/agents-page";
import { pageHead } from "@/lib/head";

/**
 * The agents filling the window: the desktop app's Agents window, and a link straight to them.
 * In the dashboard they open as a popup from the header.
 */
export const Route = createFileRoute("/dashboard/agents/")({
  head: () => pageHead({ title: "Agents" }),
  component: function AgentsRoute() {
    const sp = Route.useSearch();
    return <AgentsPage demo={sp.demo !== undefined} owner={sp.owner || undefined} focus={sp.focus || undefined} fresh={sp.new !== undefined} />;
  },
});
