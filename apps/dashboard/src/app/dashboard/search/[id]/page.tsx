import type { Metadata } from "next";
import { Suspense } from "react";
import { SearchClusterLoading, SearchClusterPage } from "@/components/search/cluster";

export const metadata: Metadata = { title: "Search" };

/** The id is only known at request time, so it resolves inside Suspense (Cache Components). */
export default function SearchClusterRoute({ params }: PageProps<"/dashboard/search/[id]">) {
  return (
    <Suspense fallback={<SearchClusterLoading />}>
      {params.then(({ id }) => (
        <SearchClusterPage id={id} />
      ))}
    </Suspense>
  );
}
