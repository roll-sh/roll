import type { Metadata } from "next";
import { StorageIam } from "@/components/storage/iam";

export const metadata: Metadata = { title: "IAM · Storage" };

export default function StorageIamPage() {
  return <StorageIam />;
}
