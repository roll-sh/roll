import type { Metadata } from "next";
import { StorageSignIn } from "@/components/storage/console";

export const metadata: Metadata = { title: "Sign in · Storage" };

/**
 * IAM user sign-in. The storage pages that aren't buckets live under `/-/` (as GitLab does) so they
 * can't shadow a bucket: `signin`, `iam` and `accounts` are valid bucket names, `-` isn't.
 */
export default function StorageSignInPage() {
  return <StorageSignIn />;
}
