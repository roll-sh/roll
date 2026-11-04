import { createFileRoute } from "@tanstack/react-router";
import { methodNotAllowed, proxyGit } from "@/lib/git-proxy";

/** Clone and fetch: the client's wants and haves in, a pack out. */
export const Route = createFileRoute("/$owner/$repo/git-upload-pack")({
  server: {
    handlers: {
      POST: ({ request, params }) => proxyGit(request, params.owner, params.repo, "git-upload-pack"),
      ANY: methodNotAllowed("POST"),
    },
  },
});
