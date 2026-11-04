import type { Metadata } from "next";
import { Suspense } from "react";
import { AgentsPage } from "@/components/agents/agents-page";

export const metadata: Metadata = { title: "Agents" };

const one = (v: string | string[] | undefined) => (Array.isArray(v) ? v[0] : v) || undefined;

/**
 * The agents filling the window: the desktop app's Agents window, and a link straight to them.
 * In the dashboard they open as a popup from the header. Search params are request data, so they
 * resolve inside Suspense.
 */
export default function AgentsRoute({ searchParams }: PageProps<"/dashboard/agents">) {
  return (
    <Suspense fallback={null}>
      {searchParams.then((sp) => (
        <AgentsPage demo={sp.demo !== undefined} owner={one(sp.owner)} focus={one(sp.focus)} fresh={sp.new !== undefined} />
      ))}
    </Suspense>
  );
}
