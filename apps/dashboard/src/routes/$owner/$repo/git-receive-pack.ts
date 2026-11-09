import { createFileRoute } from "@tanstack/react-router";
import { methodNotAllowed, proxyGit } from "@/lib/git-proxy";

/** Push: ref updates and a pack in; landing on the default branch happens here. */
export const Route = createFileRoute("/$owner/$repo/git-receive-pack")({
  server: {
    handlers: {
      POST: ({ request, params }) => proxyGit(request, params.owner, params.repo, "git-receive-pack"),
      ANY: methodNotAllowed("POST"),
    },
  },
});
