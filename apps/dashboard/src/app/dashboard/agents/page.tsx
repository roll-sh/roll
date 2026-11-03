import type { Metadata } from "next";
import { Suspense } from "react";
import { AgentsPage } from "@/components/agents/agents-page";
import { CodeShell } from "@/components/code/shell";

export const metadata: Metadata = { title: "Agents" };

const one = (v: string | string[] | undefined) => (Array.isArray(v) ? v[0] : v) || undefined;

/** The agents canvas, full-bleed. Search params are request data, so they resolve inside Suspense. */
export default function AgentsRoute({ searchParams }: PageProps<"/dashboard/agents">) {
  return (
    <CodeShell crumb="Agents" bleed>
      <Suspense fallback={null}>
        {searchParams.then((sp) => (
          <AgentsPage demo={sp.demo !== undefined} owner={one(sp.owner)} focus={one(sp.focus)} />
        ))}
      </Suspense>
    </CodeShell>
  );
}
