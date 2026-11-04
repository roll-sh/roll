import { createFileRoute } from "@tanstack/react-router";
import { methodNotAllowed, proxyGit } from "@/lib/git-proxy";

/** Ref advertisement: the first request of every clone, fetch and push. */
export const Route = createFileRoute("/$owner/$repo/info/refs")({
  server: {
    handlers: {
      GET: ({ request, params }) => proxyGit(request, params.owner, params.repo, "info/refs"),
      ANY: methodNotAllowed("GET, HEAD"),
    },
  },
});
