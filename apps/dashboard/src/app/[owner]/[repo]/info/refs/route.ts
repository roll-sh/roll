import { proxyGit } from "@/lib/git-proxy";

/** Ref advertisement: the first request of every clone, fetch and push. */
export async function GET(request: Request, { params }: RouteContext<"/[owner]/[repo]/info/refs">) {
  const { owner, repo } = await params;
  return proxyGit(request, owner, repo, "info/refs");
}
