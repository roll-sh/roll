import type { Metadata } from "next";
import { NosqlDatabases } from "@/components/nosql/databases";

export const metadata: Metadata = { title: "NoSQL" };

export default function NosqlPage() {
  return <NosqlDatabases />;
}
