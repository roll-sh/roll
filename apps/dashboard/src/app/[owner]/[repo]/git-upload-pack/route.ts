import { proxyGit } from "@/lib/git-proxy";

/** Clone and fetch: the client's wants and haves in, a pack out. */
export async function POST(request: Request, { params }: RouteContext<"/[owner]/[repo]/git-upload-pack">) {
  const { owner, repo } = await params;
  return proxyGit(request, owner, repo, "git-upload-pack");
}
