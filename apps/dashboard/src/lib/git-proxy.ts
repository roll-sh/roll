/**
 * Git smart HTTP on roll.sh: `git clone https://roll.sh/owner/repo(.git)` is passed straight
 * through to rolld's `/git/owner/repo.git/…`. Bodies stream both ways — packs can be large — so
 * this is a route handler rather than a `next.config` rewrite (external rewrites buffer request
 * bodies in memory, capped at `proxyClientMaxBodySize`). In production, route `*.git/*` at the
 * load balancer straight to rolld and this never runs.
 */

const ROLL_API_URL = process.env.ROLL_API_URL ?? process.env.NEXT_PUBLIC_ROLL_API ?? "http://127.0.0.1:7070";

const REQUEST_HEADERS = ["authorization", "content-type", "content-encoding", "accept", "accept-encoding", "git-protocol", "user-agent"];
/** Hop-by-hop, plus what undici rewrites when it decodes the body for us. */
const DROP_RESPONSE_HEADERS = new Set(["connection", "keep-alive", "transfer-encoding", "content-encoding", "content-length"]);

export async function proxyGit(request: Request, owner: string, repo: string, tail: string) {
  const name = repo.replace(/\.git$/i, "");
  const url = new URL(request.url);
  const target = `${ROLL_API_URL}/git/${encodeURIComponent(owner)}/${encodeURIComponent(name)}.git/${tail}${url.search}`;

  const headers = new Headers();
  for (const h of REQUEST_HEADERS) {
    const v = request.headers.get(h);
    if (v) headers.set(h, v);
  }
  const forwarded = request.headers.get("x-forwarded-for");
  headers.set("x-forwarded-for", forwarded ?? "");
  headers.set("x-forwarded-host", url.host);
  headers.set("x-forwarded-proto", url.protocol.replace(":", ""));

  const hasBody = request.method !== "GET" && request.method !== "HEAD";
  let upstream: Response;
  try {
    upstream = await fetch(target, {
      method: request.method,
      headers,
      body: hasBody ? request.body : undefined,
      // Stream the request body instead of buffering it.
      ...(hasBody ? { duplex: "half" } : {}),
      redirect: "manual",
      cache: "no-store",
    } as RequestInit);
  } catch {
    return new Response("roll: git backend unavailable\n", { status: 502, headers: { "content-type": "text/plain" } });
  }

  const out = new Headers();
  upstream.headers.forEach((v, k) => {
    if (!DROP_RESPONSE_HEADERS.has(k)) out.set(k, v);
  });
  return new Response(upstream.body, { status: upstream.status, statusText: upstream.statusText, headers: out });
}
