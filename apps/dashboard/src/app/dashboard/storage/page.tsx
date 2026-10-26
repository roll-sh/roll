import type { Metadata } from "next";
import { StorageHome } from "@/components/storage/home";

export const metadata: Metadata = { title: "Storage" };

export default function StoragePage() {
  return <StorageHome />;
}
