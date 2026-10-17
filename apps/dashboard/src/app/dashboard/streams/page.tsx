import type { Metadata } from "next";
import { Streams } from "@/components/streams";

export const metadata: Metadata = { title: "Streams" };

export default function StreamsPage() {
  return <Streams />;
}
