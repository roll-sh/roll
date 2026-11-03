import type { Metadata } from "next";
import { Suspense } from "react";
import { CodeLoading } from "@/components/code/frame";
import { CodeOwner } from "@/components/code/owner";

export async function generateMetadata({ params }: PageProps<"/dashboard/code/[owner]">): Promise<Metadata> {
  const { owner } = await params;
  return { title: `${owner} · Code` };
}

/** Params are only known at request time, so they resolve inside Suspense (Cache Components). */
export default function CodeOwnerPage({ params }: PageProps<"/dashboard/code/[owner]">) {
  return <Suspense fallback={<CodeLoading />}>{params.then(({ owner }) => <CodeOwner name={owner} />)}</Suspense>;
}
