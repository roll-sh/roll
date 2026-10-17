import type { Metadata } from "next";
import { Databases } from "@/components/databases";

export const metadata: Metadata = { title: "Databases" };

export default function DatabasesPage() {
  return <Databases />;
}
