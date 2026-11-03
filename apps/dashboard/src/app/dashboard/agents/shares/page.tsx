import type { Metadata } from "next";
import { Suspense } from "react";
import { SharesList } from "@/components/agents/share";
import { CodeShell } from "@/components/code/shell";

export const metadata: Metadata = { title: "Shared threads · Agents" };

const one = (v: string | string[] | undefined) => (Array.isArray(v) ? v[0] : v) || undefined;

/** An owner's public share links, to copy or revoke. */
export default function SharesRoute({ searchParams }: PageProps<"/dashboard/agents/shares">) {
  return (
    <CodeShell crumb="Agents · shares">
      <Suspense fallback={null}>{searchParams.then((sp) => <SharesList owner={one(sp.owner)} />)}</Suspense>
    </CodeShell>
  );
}
