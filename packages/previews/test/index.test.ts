import { strict as assert } from "node:assert";
import { createServer, type IncomingMessage } from "node:http";
import { test } from "node:test";
import { BYPASS_HEADER, bypass, snapshot, type PageLike, type RouteLike } from "../src/index.js";

/** A preview's host that keeps what it was sent. */
async function preview(status = 201) {
  const got: { url: string; headers: IncomingMessage["headers"]; body: Buffer }[] = [];
  const server = createServer((req, res) => {
    const chunks: Buffer[] = [];
    req.on("data", (c: Buffer) => chunks.push(c));
    req.on("end", () => {
      got.push({ url: req.url ?? "", headers: req.headers, body: Buffer.concat(chunks) });
      res.writeHead(status, { "content-type": "application/json" });
      res.end(JSON.stringify({ page: "snap:checkout", head: "cs_1" }));
    });
  });
  await new Promise<void>((ok) => server.listen(0, "127.0.0.1", ok));
  const port = (server.address() as { port: number }).port;
  return { base: `http://127.0.0.1:${port}`, got, close: () => server.close() };
}

test("a snapshot is the page's picture, sent with the secret and its head to its own origin", async () => {
  const p = await preview();
  const png = new Uint8Array([0x89, 0x50, 0x4e, 0x47]);
  const page: PageLike = { url: () => `${p.base}/cart?x=1`, route: async () => {}, screenshot: async (o) => (assert.deepEqual(o, { fullPage: true, animations: "disabled" }), png) };
  const made = await snapshot(page, "checkout/empty", { secret: "rpb_s", head: "abc123" });
  p.close();
  assert.deepEqual(made, { page: "snap:checkout", head: "cs_1" });
  assert.equal(p.got[0].url, "/__roll/snapshot?name=checkout%2Fempty&head=abc123");
  assert.equal(p.got[0].headers[BYPASS_HEADER], "rpb_s");
  assert.deepEqual([...p.got[0].body], [...png]);
});

test("a refused snapshot fails the test, saying why", async () => {
  const p = await preview(409);
  const page: PageLike = { url: () => p.base, route: async () => {}, screenshot: async () => new Uint8Array([1]) };
  await assert.rejects(snapshot(page, "x", { secret: "rpb_s" }), /409/);
  p.close();
});

test("the bypass is set on the preview's own origin only, keeping the request's headers", async () => {
  let pattern = "";
  let handler: ((r: RouteLike) => Promise<void> | void) | undefined;
  const page: PageLike = { url: () => "", route: async (u, h) => { pattern = u; handler = h; }, screenshot: async () => new Uint8Array() };
  await bypass(page, "https://web-pr7.preview.roll.site/cart", "rpb_s");
  assert.equal(pattern, "https://web-pr7.preview.roll.site/**");
  let sent: Record<string, string> = {};
  await handler!({ request: () => ({ headers: () => ({ accept: "text/html" }) }), continue: async (o) => { sent = o.headers; } });
  assert.deepEqual(sent, { accept: "text/html", [BYPASS_HEADER]: "rpb_s" });
  await assert.rejects(bypass(page, "https://x.test", ""), /no secret/);
});
