import type { Metadata } from "next";
import { Suspense } from "react";
import { CodeChange } from "@/components/code/changes";
import { CodeLoading } from "@/components/code/frame";

export async function generateMetadata({
  params,
}: PageProps<"/dashboard/code/[owner]/[repo]/changes/[number]">): Promise<Metadata> {
  const { owner, repo, number } = await params;
  return { title: `${number.startsWith("chg_") ? number : `#${number}`} · ${owner}/${repo}` };
}

/** Params are only known at request time, so they resolve inside Suspense (Cache Components). */
export default function CodeChangePage({ params }: PageProps<"/dashboard/code/[owner]/[repo]/changes/[number]">) {
  return (
    <Suspense fallback={<CodeLoading />}>
      {params.then(({ owner, repo, number }) => (
        <CodeChange fullName={`${owner}/${repo}`} id={number} />
      ))}
    </Suspense>
  );
}
