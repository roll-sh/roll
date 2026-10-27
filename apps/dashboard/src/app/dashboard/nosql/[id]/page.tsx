import type { Metadata } from "next";
import { Suspense } from "react";
import { NosqlDatabase, NosqlDatabaseLoading } from "@/components/nosql/database";

export const metadata: Metadata = { title: "NoSQL" };

/** The id is only known at request time, so it resolves inside Suspense (Cache Components). */
export default function NosqlDatabasePage({ params }: PageProps<"/dashboard/nosql/[id]">) {
  return (
    <Suspense fallback={<NosqlDatabaseLoading />}>
      {params.then(({ id }) => (
        <NosqlDatabase id={id} />
      ))}
    </Suspense>
  );
}
