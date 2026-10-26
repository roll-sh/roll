import type { Metadata } from "next";
import { StorageAccounts } from "@/components/storage/accounts";

export const metadata: Metadata = { title: "Accounts · Storage" };

export default function StorageAccountsPage() {
  return <StorageAccounts />;
}
