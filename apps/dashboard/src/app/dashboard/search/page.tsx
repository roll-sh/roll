import type { Metadata } from "next";
import { SearchClusters } from "@/components/search/clusters";

export const metadata: Metadata = { title: "Search" };

export default function SearchPage() {
  return <SearchClusters />;
}
