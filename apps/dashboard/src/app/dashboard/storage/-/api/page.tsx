import type { Metadata } from "next";
import { StorageDevApi } from "@/components/storage/api";

export const metadata: Metadata = { title: "API · Storage" };

export default function StorageApiPage() {
  return <StorageDevApi />;
}
