import { createFileRoute } from "@tanstack/react-router";
import { StorageSignIn } from "@/components/storage/console";
import { pageHead } from "@/lib/head";

/**
 * IAM user sign-in. The storage pages that aren't buckets live under `/-/` (as GitLab does) so they
 * can't shadow a bucket: `signin`, `iam` and `accounts` are valid bucket names, `-` isn't. (`[-]`
 * escapes the dash, which would otherwise keep the folder out of the route tree.)
 */
export const Route = createFileRoute("/dashboard/storage/-/signin")({
  head: () => pageHead({ title: "Sign in · Storage" }),
  component: StorageSignIn,
});
