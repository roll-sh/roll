/**
 * Search from the browser (docs/search/README.md): a cluster's allowed origins (`http.cors.*`) as
 * the Settings tab edits them, and the quickstart snippets a page needs with a browser key.
 * Origins are checked as `search_server::cors` checks them, so the form refuses what the API would.
 */

import type { BrowserKey } from "./search-api";

/**
 * An allowed origin as written (`*`, `/regex/` or `scheme://host[:port]`), kept as a browser sends
 * it (lower case, no default port); `{ error }` when it isn't one.
 */
export function readOrigin(raw: string): { origin: string } | { error: string } {
  const s = raw.trim();
  if (s === "*") return { origin: s };
  if (s.length >= 2 && s.startsWith("/") && s.endsWith("/")) {
    const body = s.slice(1, -1);
    if (!body || body.length > 1024) return { error: `${s}: a /regex/ is 1 to 1024 characters` };
    try {
      new RegExp(body);
    } catch {
      return { error: `${s} isn't a regular expression` };
    }
    return { origin: s };
  }
  const bad = { error: `${s} isn't an origin: write scheme://host[:port], * or /regex/` };
  const m = /^(https?):\/\/([^/?#@\s\\]+?)\/?$/i.exec(s);
  if (!m) return bad;
  const scheme = m[1].toLowerCase();
  const authority = m[2].toLowerCase();
  const v6 = /^(\[[0-9a-f:.]+\])(?::(\d+))?$/.exec(authority);
  const plain = /^([a-z0-9_-]+(?:\.[a-z0-9_-]+)*)(?::(\d+))?$/.exec(authority);
  const hp = v6 ?? plain;
  if (!hp || hp[1].length > 253) return bad;
  const port = hp[2];
  if (port !== undefined) {
    const n = Number(port);
    if (!(n >= 1 && n <= 65535) || port.length > 5) return bad;
    const defaultPort = scheme === "https" ? 443 : 80;
    return { origin: n === defaultPort ? `${scheme}://${hp[1]}` : `${scheme}://${hp[1]}:${n}` };
  }
  return { origin: `${scheme}://${hp[1]}` };
}

/** One origin a line; blank lines are skipped. */
export function parseOrigins(text: string): { origins: string[]; errors: string[] } {
  const origins: string[] = [];
  const errors: string[] = [];
  for (const line of text.split("\n")) {
    if (!line.trim()) continue;
    const r = readOrigin(line);
    if ("error" in r) errors.push(r.error);
    else if (!origins.includes(r.origin)) origins.push(r.origin);
  }
  if (origins.length > 32) errors.push("at most 32 origins");
  return { origins, errors };
}

/** What a browser key reaches, in a few words. */
export function reachOf(k: Pick<BrowserKey, "indices" | "application">): string {
  if (k.application) return `search application ${k.application}`;
  const i = k.indices ?? [];
  return i.length === 1 ? `index ${i[0]}` : `indices ${i.join(", ")}`;
}

/** A key's limit, e.g. "600/min · 60/min per address". */
export function limitOf(k: Pick<BrowserKey, "rate_limit">): string {
  const r = k.rate_limit;
  if (!r) return "no limit";
  return r.per_ip_per_minute ? `${r.per_minute}/min · ${r.per_ip_per_minute}/min per address` : `${r.per_minute}/min`;
}

export type Quickstart = "search-application" | "search-ui" | "instantsearch";

/**
 * The page code for `kind`, against the cluster's `url` with a browser key (`apiKey`: the key
 * just made, else a placeholder). `target`: the search application's name, or the index a key
 * reads (Search UI and Searchkit send whole queries, so they need an indices key; a search
 * application's name is also an alias of its indices).
 */
export function quickstart(kind: Quickstart, url: string, target: string, apiKey?: string): string {
  const key = apiKey ?? "<browser key>";
  const q = (s: string) => JSON.stringify(s);
  switch (kind) {
    case "search-application":
      return `// npm install @elastic/search-application-client
import SearchApplicationClient from "@elastic/search-application-client";

// The key may only run this application's search template; params are checked
// against the template's dictionary.
const client = SearchApplicationClient(${q(target)}, ${q(url)}, ${q(key)});

const results = await client().query("shoes").search();
console.log(results.hits.hits);
`;
    case "search-ui":
      return `// npm install @elastic/react-search-ui @elastic/search-ui-elasticsearch-connector
import ElasticsearchAPIConnector from "@elastic/search-ui-elasticsearch-connector";
import { SearchProvider, SearchBox, Results } from "@elastic/react-search-ui";

const connector = new ElasticsearchAPIConnector({
  host: ${q(url)},
  index: ${q(target)},
  apiKey: ${q(key)},
});

export const App = () => (
  <SearchProvider config={{ apiConnector: connector, searchQuery: { search_fields: { title: {} }, result_fields: { title: { snippet: {} } } } }}>
    <SearchBox />
    <Results />
  </SearchProvider>
);
`;
    case "instantsearch":
      return `// npm install searchkit @searchkit/instantsearch-client instantsearch.js
import Searchkit from "searchkit";
import Client from "@searchkit/instantsearch-client";
import instantsearch from "instantsearch.js";
import { searchBox, hits } from "instantsearch.js/es/widgets";

const sk = new Searchkit({
  connection: { host: ${q(url)}, apiKey: ${q(key)} },
  search_settings: { search_attributes: ["title"], result_attributes: ["title"] },
});

const search = instantsearch({ indexName: ${q(target)}, searchClient: Client(sk) });
search.addWidgets([searchBox({ container: "#q" }), hits({ container: "#hits" })]);
search.start();
`;
  }
}
