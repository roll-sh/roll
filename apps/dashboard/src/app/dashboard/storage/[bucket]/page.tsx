import type { Metadata } from "next";
import { Suspense } from "react";
import { StorageBucket, StorageBucketLoading } from "@/components/storage/bucket";

export const metadata: Metadata = { title: "Storage" };

/** The bucket is only known at request time, so it resolves inside Suspense (Cache Components). */
export default function StorageBucketPage({ params }: PageProps<"/dashboard/storage/[bucket]">) {
  return (
    <Suspense fallback={<StorageBucketLoading />}>
      {params.then(({ bucket }) => (
        <StorageBucket name={bucket} />
      ))}
    </Suspense>
  );
}
