// roll previews' egress proxy (docs/previews.md, "Egress"): the one way out of a preview's own
// network when its project lists hosts (`egress_hosts`). It runs in a container on that network
// (alias `roll-egress`) and on Docker's default one, and passes on only HTTPS tunnels (CONNECT)
// and plain HTTP requests to a listed host on port 443 or 80; anything else is a 403, said on
// stdout as `refused <host>` (the runtime reads it into the preview's activity). A listed name
// is a tenant's choice, so it may point anywhere: it's resolved here and only a public address
// is used (roll-net's `private_ip`, AGENTS.md), connected to as resolved, so a name pointed at the
// cloud's metadata service, the host or the fleet's network (or re-pointed between the check and
// the connection) reaches nothing.
const net = require("node:net");
const http = require("node:http");
const dns = require("node:dns");
// Loopback, private, link-local, CGNAT, multicast, unspecified, documentation and reserved
// ranges, and IPv4 inside IPv6 (mapped, NAT64, 6to4): roll-net's `private_ip`.
const private4 = (a) =>
  a[0] === 127 || a[0] === 10 || (a[0] === 172 && (a[1] & 0xf0) === 16) || (a[0] === 192 && a[1] === 168) ||
  (a[0] === 169 && a[1] === 254) || a[0] === 0 || (a[0] === 100 && (a[1] & 0xc0) === 64) || (a[0] === 192 && a[1] === 0 && a[2] === 0) ||
  (a[0] === 192 && a[1] === 0 && a[2] === 2) || (a[0] === 198 && a[1] === 51 && a[2] === 100) || (a[0] === 203 && a[1] === 0 && a[2] === 113) ||
  (a[0] === 198 && (a[1] & 0xfe) === 18) || (a[0] === 192 && a[1] === 88 && a[2] === 99) ||
  (a[0] >= 224);
const privateIp = (ip) => {
  if (net.isIPv4(ip)) return private4(ip.split(".").map(Number));
  if (!net.isIPv6(ip)) return true;
  const s = expand6(ip);
  const v4 = (hi, lo) => [hi >> 8, hi & 255, lo >> 8, lo & 255];
  if (s.slice(0, 5).every((x) => x === 0) && s[5] === 0xffff) return private4(v4(s[6], s[7]));
  if (s[0] === 0x64 && s[1] === 0xff9b && ((s[2] === 0 && s[3] === 0 && s[4] === 0 && s[5] === 0) || s[2] === 1)) return private4(v4(s[6], s[7]));
  if (s[0] === 0x2002) return private4(v4(s[1], s[2]));
  // Teredo (2001::/32) carries an IPv4 address of its own: never a destination.
  if (s[0] === 0x2001 && s[1] === 0) return true;
  if (s.every((x) => x === 0) || (s.slice(0, 7).every((x) => x === 0) && s[7] === 1)) return true;
  return (s[0] & 0xff00) === 0xff00 || (s[0] & 0xfe00) === 0xfc00 || (s[0] & 0xffc0) === 0xfe80 || (s[0] === 0x2001 && s[1] === 0xdb8);
};
// An IPv6 address's eight groups (`::` filled in, a dotted IPv4 tail as two groups).
const expand6 = (ip) => {
  let s = ip.split("%")[0];
  const dotted = s.match(/(\d+\.\d+\.\d+\.\d+)$/);
  if (dotted) {
    const a = dotted[1].split(".").map(Number);
    s = s.slice(0, -dotted[1].length) + ((a[0] << 8) | a[1]).toString(16) + ":" + ((a[2] << 8) | a[3]).toString(16);
  }
  const [head, tail] = s.split("::");
  const h = head ? head.split(":") : [];
  const t = tail !== undefined && tail ? tail.split(":") : [];
  const fill = tail !== undefined ? Array(8 - h.length - t.length).fill("0") : [];
  return [...h, ...fill, ...t].map((x) => parseInt(x || "0", 16));
};
// The name's first public address, or null (none, or it doesn't resolve).
const publicAddress = (host) =>
  new Promise((done) =>
    dns.lookup(host, { all: true }, (err, addrs) => done(err ? null : (addrs || []).map((a) => a.address).find((a) => !privateIp(a)) || null)),
  );
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
// `refused <host>`, and ` private` when the host is listed but resolves only to private addresses.
const refused = (host, why) => process.stdout.write(`refused ${String(host).slice(0, 253).replace(/[^a-z0-9.*:-]/gi, "?")}${why ? " private" : ""}\n`);
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
  publicAddress(url.hostname).then((addr) => {
    if (!addr) {
      refused(url.hostname, true);
      res.writeHead(403, { "content-type": "text/plain" }).end("roll previews: this host has no public address\n");
      return;
    }
    // To the address checked, with the name as the Host header.
    const up = http.request({ host: addr, port, method: req.method, path: url.pathname + url.search, headers: { ...req.headers, host: url.host } }, (r) => {
      res.writeHead(r.statusCode || 502, r.headers);
      r.pipe(res);
    });
    up.on("error", () => res.writeHead(502).end());
    req.pipe(up);
  });
});
server.on("connect", (req, socket, head) => {
  const [host, p] = String(req.url).split(":");
  const port = Number(p || 443);
  if (!allowed(host, port)) {
    refused(host);
    socket.end("HTTP/1.1 403 Forbidden\r\n\r\n");
    return;
  }
  publicAddress(host).then((addr) => {
    if (!addr) {
      refused(host, true);
      socket.end("HTTP/1.1 403 Forbidden\r\n\r\n");
      return;
    }
    // To the address checked: the tunnel's TLS still names the host (SNI), as the client sends it.
    const up = net.connect(port, addr, () => {
      socket.write("HTTP/1.1 200 Connection Established\r\n\r\n");
      if (head && head.length) up.write(head);
      up.pipe(socket);
      socket.pipe(up);
    });
    up.on("error", () => socket.end("HTTP/1.1 502 Bad Gateway\r\n\r\n"));
    socket.on("error", () => up.destroy());
  });
});
server.listen(3128, "0.0.0.0");
