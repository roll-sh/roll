import type { Metadata } from "next";
import { Suspense } from "react";
import { RedisDatabase, RedisDatabaseLoading } from "@/components/redis/database";

export const metadata: Metadata = { title: "Redis" };

/** The id is only known at request time, so it resolves inside Suspense (Cache Components). */
export default function RedisDatabasePage({ params }: PageProps<"/dashboard/redis/[id]">) {
  return (
    <Suspense fallback={<RedisDatabaseLoading />}>
      {params.then(({ id }) => (
        <RedisDatabase id={id} />
      ))}
    </Suspense>
  );
}
