import { proxyGit } from "@/lib/git-proxy";

/** Push: ref updates and a pack in; landing on the default branch happens here. */
export async function POST(request: Request, { params }: RouteContext<"/[owner]/[repo]/git-receive-pack">) {
  const { owner, repo } = await params;
  return proxyGit(request, owner, repo, "git-receive-pack");
}
