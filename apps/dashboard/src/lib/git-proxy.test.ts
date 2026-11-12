import { afterEach, describe, expect, it, vi } from "vitest";

afterEach(() => {
  vi.unstubAllGlobals();
  vi.unstubAllEnvs();
  vi.resetModules();
});

async function load(api = "http://rolld:7070") {
  vi.stubEnv("ROLL_API_URL", api);
  return (await import("./git-proxy")).proxyGit;
}

describe("proxyGit", () => {
  it("forwards git requests to rolld with only the headers git needs", async () => {
    const calls: { url: string; init: RequestInit & { duplex?: string } }[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string, init: RequestInit) => {
        calls.push({ url, init });
        return new Response("PACK", {
          status: 200,
          headers: {
            "content-type": "application/x-git-upload-pack-result",
            "content-encoding": "gzip",
            "content-length": "4",
            connection: "keep-alive",
            "cache-control": "no-cache",
          },
        });
      }),
    );
    const proxyGit = await load();
    const body = new ReadableStream({ start: (c) => (c.enqueue(new TextEncoder().encode("0000")), c.close()) });
    const req = new Request("https://roll.sh/acme/web.git/git-upload-pack?x=1", {
      method: "POST",
      body,
      headers: {
        authorization: "Basic eDp5",
        "content-type": "application/x-git-upload-pack-request",
        "content-encoding": "gzip",
        "git-protocol": "version=2",
        "user-agent": "git/2.56",
        cookie: "session=secret",
        "x-forwarded-for": "1.2.3.4",
      },
      duplex: "half",
    } as RequestInit);
    const res = await proxyGit(req, "acme", "web.git", "git-upload-pack");

    expect(calls[0].url).toBe("http://rolld:7070/git/acme/web.git/git-upload-pack?x=1");
    const h = calls[0].init.headers as Headers;
    expect(h.get("authorization")).toBe("Basic eDp5");
    expect(h.get("git-protocol")).toBe("version=2");
    expect(h.get("content-encoding")).toBe("gzip");
    expect(h.get("cookie")).toBeNull();
    expect(h.get("x-forwarded-for")).toBe("1.2.3.4");
    expect(h.get("x-forwarded-host")).toBe("roll.sh");
    expect(h.get("x-forwarded-proto")).toBe("https");
    expect(calls[0].init.duplex).toBe("half");
    expect(calls[0].init.redirect).toBe("manual");
    expect(calls[0].init.body).toBeInstanceOf(ReadableStream);

    expect(res.status).toBe(200);
    expect(await res.text()).toBe("PACK");
    expect(res.headers.get("content-type")).toBe("application/x-git-upload-pack-result");
    expect(res.headers.get("cache-control")).toBe("no-cache");
    for (const dropped of ["content-encoding", "content-length", "connection"]) expect(res.headers.get(dropped)).toBeNull();
  });

  it("adds .git when the URL has none, sends no body on GET, and passes auth challenges through", async () => {
    const calls: { url: string; init: RequestInit }[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string, init: RequestInit) => {
        calls.push({ url, init });
        return new Response("authentication required\n", { status: 401, headers: { "www-authenticate": 'Basic realm="roll code"' } });
      }),
    );
    const proxyGit = await load();
    const res = await proxyGit(new Request("https://roll.sh/acme/my%20repo/info/refs?service=git-receive-pack"), "acme", "my repo", "info/refs");
    expect(calls[0].url).toBe("http://rolld:7070/git/acme/my%20repo.git/info/refs?service=git-receive-pack");
    expect(calls[0].init.body).toBeUndefined();
    expect((calls[0].init as { duplex?: string }).duplex).toBeUndefined();
    expect(res.status).toBe(401);
    expect(res.headers.get("www-authenticate")).toBe('Basic realm="roll code"');
  });

  it("answers 502 when rolld is unreachable", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => {
        throw new TypeError("fetch failed");
      }),
    );
    const proxyGit = await load();
    const res = await proxyGit(new Request("https://roll.sh/acme/web/info/refs?service=git-upload-pack"), "acme", "web", "info/refs");
    expect(res.status).toBe(502);
    expect(await res.text()).toContain("git backend unavailable");
  });
});
