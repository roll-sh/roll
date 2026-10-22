import type { Metadata } from "next";
import { CodeRepos } from "@/components/code/repos";

export const metadata: Metadata = { title: "Code" };

export default function CodePage() {
  return <CodeRepos />;
}
