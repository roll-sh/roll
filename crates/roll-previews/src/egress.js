// roll previews' egress proxy (docs/previews.md, "Egress"): the one way out of a preview's own
// network when its project lists hosts (`egress_hosts`). It runs in a container on that network
// (alias `roll-egress`) and on Docker's default one, and passes on only HTTPS tunnels (CONNECT)
// and plain HTTP requests to a listed host on port 443 or 80; anything else is a 403, said on
// stdout as `refused <host>` (the runtime reads it into the preview's activity).
const net = require("node:net");
const http = require("node:http");
const HOSTS = (() => {
  try {
    return JSON.parse(process.env.HOSTS || "[]").filter((h) => typeof h === "string").slice(0, 20);
  } catch {
    return [];
  }
})();
const allowed = (host, port) => {
  const h = String(host || "").toLowerCase().replace(/\.$/, "");
  if (port !== 443 && port !== 80) return false;
  return HOSTS.some((p) => (p.startsWith("*.") ? h.endsWith(p.slice(1)) && h.length > p.length - 1 : h === p));
};
const refused = (host) => process.stdout.write(`refused ${String(host).slice(0, 253).replace(/[^a-z0-9.*:-]/gi, "?")}\n`);
const server = http.createServer((req, res) => {
  // A plain HTTP request through the proxy: an absolute URL.
  let url;
  try {
    url = new URL(req.url);
  } catch {
    res.writeHead(400).end();
    return;
  }
  const port = Number(url.port || 80);
  if (url.protocol !== "http:" || !allowed(url.hostname, port)) {
    refused(url.hostname);
    res.writeHead(403, { "content-type": "text/plain" }).end("roll previews: this host isn't in the project's egress_hosts\n");
    return;
  }
  const up = http.request({ host: url.hostname, port, method: req.method, path: url.pathname + url.search, headers: req.headers }, (r) => {
    res.writeHead(r.statusCode || 502, r.headers);
    r.pipe(res);
  });
  up.on("error", () => res.writeHead(502).end());
  req.pipe(up);
});
server.on("connect", (req, socket, head) => {
  const [host, p] = String(req.url).split(":");
  const port = Number(p || 443);
  if (!allowed(host, port)) {
    refused(host);
    socket.end("HTTP/1.1 403 Forbidden\r\n\r\n");
    return;
  }
  const up = net.connect(port, host, () => {
    socket.write("HTTP/1.1 200 Connection Established\r\n\r\n");
    if (head && head.length) up.write(head);
    up.pipe(socket);
    socket.pipe(up);
  });
  up.on("error", () => socket.end("HTTP/1.1 502 Bad Gateway\r\n\r\n"));
  socket.on("error", () => up.destroy());
});
server.listen(3128, "0.0.0.0");
