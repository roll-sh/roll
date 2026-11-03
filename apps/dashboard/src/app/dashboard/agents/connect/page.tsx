import type { Metadata } from "next";
import { Suspense } from "react";
import { ConnectAgents } from "@/components/agents/connect";
import { CodeShell } from "@/components/code/shell";

export const metadata: Metadata = { title: "Connect a machine · Agents" };

const one = (v: string | string[] | undefined) => (Array.isArray(v) ? v[0] : v) || undefined;

/** Approve a `roll agent login` (`?code=`), or add a machine. */
export default function ConnectRoute({ searchParams }: PageProps<"/dashboard/agents/connect">) {
  return (
    <CodeShell crumb="Agents · connect">
      <Suspense fallback={null}>{searchParams.then((sp) => <ConnectAgents code={one(sp.code)?.toUpperCase()} />)}</Suspense>
    </CodeShell>
  );
}
