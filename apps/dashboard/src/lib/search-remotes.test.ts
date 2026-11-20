import { describe, expect, it } from "vitest";
import type { SearchCluster } from "./search-api";
import { linkBody, linkTargets, parsePatterns, remoteRows, suggestAlias, validAlias, type RemoteLink } from "./search-remotes";

const link = (alias: string, cluster: string | null, connected = true): RemoteLink => ({ alias, cluster, cluster_id: `id-${cluster}`, key_id: "k", minted: true, created_at: 1, connected });

describe("remoteRows", () => {
  it("joins roll's links with _remote/info", () => {
    const rows = remoteRows([link("logs", "ann-logs"), link("old", null, false)], {
      logs: { connected: true, mode: "proxy", proxy_address: "ann-logs", skip_unavailable: false, cluster_credentials: "::es_redacted::" },
      self: { connected: true, mode: "sniff", seeds: ["127.0.0.1:9300"], skip_unavailable: true },
      bare: { connected: false, mode: "proxy", proxy_address: "ann-x", skip_unavailable: true },
    });
    expect(rows).toEqual([
      { alias: "bare", target: "ann-x", skipUnavailable: true, state: "no-credentials", linked: false, minted: false },
      { alias: "logs", target: "ann-logs", skipUnavailable: false, state: "connected", linked: true, minted: true },
      { alias: "old", target: "deleted cluster", skipUnavailable: null, state: "unreachable", linked: true, minted: true },
      { alias: "self", target: "127.0.0.1:9300", skipUnavailable: true, state: "connected", linked: false, minted: false },
    ]);
  });

  it("doesn't guess what an asleep cluster's settings say", () => {
    expect(remoteRows([link("logs", "ann-logs")], null)).toEqual([{ alias: "logs", target: "ann-logs", skipUnavailable: null, state: "unknown", linked: true, minted: true }]);
  });
});

describe("linking", () => {
  const c = (id: string, name: string, flavor: "elasticsearch" | "opensearch" = "elasticsearch", owner: string | null = "usr_ann") => ({ id, name, flavor, owner }) as SearchCluster;

  it("offers the person's other clusters of the same flavor", () => {
    const self = c("1", "app");
    expect(linkTargets([c("2", "logs"), self, c("3", "os", "opensearch"), c("4", "bobs", "elasticsearch", "usr_bob"), c("5", "audit")], self).map((t) => t.name)).toEqual(["audit", "logs"]);
  });

  it("checks aliases and builds the request", () => {
    expect([validAlias("logs-2"), validAlias("a_b"), validAlias(""), validAlias("a:b"), validAlias("x".repeat(65))]).toEqual([true, true, false, false, false]);
    expect(suggestAlias("logs", ["logs", "logs-2"])).toBe("logs-3");
    expect(parsePatterns(" logs-*, metrics-*  audit ")).toEqual(["logs-*", "metrics-*", "audit"]);
    expect(linkBody("ann-logs", true, [])).toEqual({ cluster: "ann-logs", skip_unavailable: true, access: { search: [{ names: ["*"] }] } });
    expect(linkBody("ann-logs", false, ["logs-*"]).access.search[0].names).toEqual(["logs-*"]);
  });
});
