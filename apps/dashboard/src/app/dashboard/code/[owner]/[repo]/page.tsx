import type { Metadata } from "next";
import { Suspense } from "react";
import { CodeLoading } from "@/components/code/frame";
import { CodeRepo } from "@/components/code/repo";

export async function generateMetadata({ params }: PageProps<"/dashboard/code/[owner]/[repo]">): Promise<Metadata> {
  const { owner, repo } = await params;
  return { title: `${owner}/${repo}` };
}

/** The repo is only known at request time, so it resolves inside Suspense (Cache Components). */
export default function CodeRepoPage({ params }: PageProps<"/dashboard/code/[owner]/[repo]">) {
  return (
    <Suspense fallback={<CodeLoading />}>
      {params.then(({ owner, repo }) => (
        <CodeRepo fullName={`${owner}/${repo}`} />
      ))}
    </Suspense>
  );
}
