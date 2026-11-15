import { createFileRoute } from "@tanstack/react-router";
import { Device } from "@/components/access/device";
import { pageHead } from "@/lib/head";

/** A device signing in to roll with a code (RFC 8628; docs/permissions.md, "Signing the CLI in"). */
export const Route = createFileRoute("/oauth/device")({
  head: () => pageHead({ title: "Sign a device in" }),
  component: function DeviceRoute() {
    const search = Route.useSearch() as Record<string, unknown>;
    return <Device search={search} />;
  },
});
