# A web crawler for site search: design

`## Next` item 13 in [README.md](README.md). Status: phases 1 and 2 landed. Phase 1: the crawl,
robots.txt, sitemaps, crawl rules, default fields, the purge, limits and politeness, and
`roll search crawler import` (`search-es` `xpack/entsearch/sync/crawler/`). Phase 2: domain
verification with the verified limits and fleet-wide daily counts (`search-control` `sites.rs`),
`crawl-test` with its MCP tool and CLI, and the dashboard's "crawl a site" box. Phase 3 has begun:
extraction rules landed (`crawler/select.rs`), and verification tokens are derived from the owner
and the site; incremental crawls and the docs preset are to come. It runs on the connector runner of #318, which it reuses for jobs, schedules, slots,
cancelling, counters and sealed credentials (architecture.md, "Connectors" and "Web crawler").
Where phase 1 differs from Open Crawler is listed in compatibility.md ("The web crawler").

## Why

With every product in the category, a site search box starts with "point it at your site". roll
users have to write a script that reads their pages and bulk-indexes them
([guides/site-search.md](guides/site-search.md), "Index your pages"). Connectors (#318) bring in
Postgres tables and S3 buckets. A website has no way in.

| Product | How a site gets in | What roll takes from it |
|---|---|---|
| Elastic Open Crawler | A self-hosted CLI with a YAML config. Elastic's hosted crawler was discontinued in Elasticsearch 9.0, so 9.x has no crawler API. | The config's shape (domains, seed and sitemap URLs, crawl rules, extraction rulesets, limits), the document fields and the id, so an Open Crawler config and index move to roll unchanged |
| Algolia Crawler | Hosted, with a UI, a URL tester, a schedule, and domain verification before a site is crawled | Running it for the user, the URL tester, and verification as the gate to higher rates |
| DocSearch, Meilisearch's scrapers | Hosted or self-hosted crawlers that build records for a docs site's heading hierarchy (`lvl0`…`lvl6`) | A docs preset, in a later slice |

Open Crawler's behaviour is the reference wherever roll does the same thing (crawl rules, robots
directives, canonical URLs, the document schema). roll departs from it only where noted, and each
departure is listed in compatibility.md when it ships.

## The shape: a connector roll runs

A crawler is a native connector whose `service_type` is `roll_web_crawler`. The `roll_` prefix is
deliberate: it can't collide with a service type Elastic adds later. A crawl is a
`_connector/_sync_job`. Everything #318 built for connectors applies without new machinery:

- Jobs move through `pending → in_progress → completed | error | canceling → canceled`, with
  counters written every 10 seconds and `_cancel` honoured at the next page.
- Crawls run on the cluster's owner node, on the cluster's job scheduler, so a sleeping cluster
  wakes only when a crawl is due. They share the node and owner slot caps and the FIFO wait.
- Schedules are `scheduling.full` and `scheduling.incremental` (Quartz cron in UTC).
- Credentials are sealed `sensitive` fields, never returned and scrubbed from errors.
- Branches inherit crawlers waiting for adoption, so a branch never crawls with its parent's
  credentials.
- Writes go through the cluster, as the user who last configured the connector.
- Kibana and the dashboard's connectors panel (#318, `components/search/connectors.tsx`) show
  crawlers with no extra work.

A separate `/v1/search/…/crawlers` API would have duplicated all of this. As a connector, a
crawler costs one new service under `xpack/entsearch/sync/`.

The index follows native connectors' rule, so its name starts with `content-`. When the index
doesn't exist, the first crawl creates it with the mappings in "Documents" below. Existing
mappings are never changed, as with Open Crawler.

## Configuration

Connector configuration is a flat field schema, so the crawler splits its configuration in two.
Simple fields go in `configuration`. Open Crawler's structured config goes in
`filtering.advanced_snippet`, where Elastic's connectors keep their advanced sync rules.

**`configuration`** (validated like every service's schema):

| Field | Meaning | Default |
|---|---|---|
| `urls` | Seed URLs, comma-separated. Each URL's origin becomes a domain. | required |
| `sitemap_urls` | Extra sitemaps, comma-separated, on the seed domains | none |
| `sitemap_discovery` | Also read the `Sitemap:` lines of robots.txt | `true` |
| `max_crawl_depth` | Link hops from a seed or a sitemap entry | `10` |
| `max_unique_url_count` | URLs one crawl visits at most | `10000` |
| `auth_origin` | The one origin (`https://host[:port]`) the credential is sent to | empty |
| `auth_header` | `sensitive`. A whole `Authorization` value, such as `Bearer …` | empty |
| `auth_username`, `auth_password` | `sensitive` (the password). Basic auth, as an alternative to `auth_header` | empty |

**`filtering.advanced_snippet`** takes a JSON subset of Open Crawler's config:

- `domains[]`, each with `url`, `seed_urls`, `sitemap_urls`, `exclude_tags`, `crawl_rules` and
  `extraction_rulesets`.
- The limit keys from "Limits" below. A value above roll's ceiling is refused when the draft is
  validated, never silently lowered.

A domain named only in the snippet is crawled like a seed domain. A credential in the snippet
(`auth`, or a key roll doesn't know) fails validation, so a secret can't sit in a field that isn't
sealed. One connector holds at most one credential. A site with two authenticated origins needs two
connectors.

`roll search crawler import <cluster> crawler.yml [--connector <id>]` reads an Open Crawler YAML
file. It writes the simple fields and the snippet, moves `domains[].auth` into the sealed fields
(refusing when two domains carry credentials), and lists every key it dropped. Dropped keys include
the output sink, `elasticsearch.*`, logging and the proxy. The CLI parses YAML, so the cluster only
ever sees JSON.

## A crawl

1. **Claim.** The job is claimed as #318's runner claims any job.
2. **Seed the frontier.** Seeds come from `urls` and `domains[].seed_urls`. Sitemaps come from
   `sitemap_urls`, `domains[].sitemap_urls`, and robots.txt's `Sitemap:` lines when discovery is
   on. Sitemap indexes are followed two levels deep. Gzipped sitemaps are decompressed within the
   response cap. Sitemap entries outside the crawl's domains are dropped.
3. **Robots.txt** follows RFC 9309. It is fetched once per origin per crawl, parsed under the
   product token `roll-crawler` (or `*` when there's no group for it), and honoured always: like
   Open Crawler, nothing configures it away. A 4xx means everything is allowed. A 5xx, a timeout or
   a network error means the origin is disallowed for this crawl, as the RFC asks. Redirects are
   followed for robots.txt up to 5 times within the same site (`http` to `https`, the apex to
   `www`), each hop's address checked and the credential not sent after a hop leaves its origin;
   more, or a hop to another site, is unreachable. A leading byte order mark is ignored. A rule is
   matched by finding the literal text between its `*`s left to right, which is exact for `*`
   patterns and linear in the URL per rule; a group of more than 5,000 rules disallows its origin,
   and the files are charged to the request breaker. `Crawl-delay` is honoured, up to the crawl's
   deadline: a request whose turn would come after it isn't made, and an origin whose next turn
   comes after the deadline is left for this crawl rather than holding up the others.

   Deciding whether URLs may be fetched (crawl rules and robots.txt) has a budget of 30 seconds a
   crawl; past it the crawl ends `completed` with `limit_reached: rule_matching_time`. The cheap
   checks (depth, seen, a full frontier) come first, a sitemap stops being read once the frontier
   is full, and long lists of links yield to the runtime as they're queued (the job's
   `metadata.checked_url_count` counts the checks). Crawl rules are fully built DFAs over a URL's
   bytes with Unicode off, one step per byte whatever the pattern; a rule whose DFA doesn't fit its
   equal share of a 10 MiB budget (or whose NFA passes 64 KiB) fails validation. They're built off
   the async workers, both when a draft is validated and when a crawl starts, and a deny rule
   whose search gives up denies.

   Unlike Open Crawler, a site's `Elastic-Crawler` group doesn't apply to roll, whose token is
   `roll-crawler`. A crawl that finds such a group and no `roll-crawler` group names the origin in
   `metadata.robots_elastic_crawler_group_ignored`.
4. **Visit, breadth first by depth.** The frontier is a FIFO per depth. Each URL is normalised (see
   "Documents"), checked against the limits, the domains, the crawl rules and robots.txt, and then
   fetched (see "Fetching").
   - Crawl rules use Open Crawler's semantics exactly. `begins`, `ends`, `contains` and `regex` act
     on the path. The first match decides, and a URL that matches no rule is allowed. `*` is a
     wildcard in the three literal types. A `regex` must match from the start of the path. Regexes
     use the `regex` crate, which runs in linear time, so a tenant's pattern can't backtrack. A
     pattern Ruby accepts but the crate doesn't (look-around, backreferences) fails validation and
     names the rule.
5. **Extract.** An HTML page becomes a document, and its links feed the frontier unless the page
   says `nofollow`. Directives follow Open Crawler's crawler directives:
   - `<meta name="robots">` with `noindex` and `nofollow`, and `rel="nofollow"` on links.
   - `<link rel="canonical">`, honoured only for a canonical URL the crawl fetched and indexed as
     itself: the page naming it then goes in that document's `additional_urls` and gets no
     document of its own. Until the canonical URL is fetched (it is queued), the page is stored
     under its own URL, so no page can take over another's document.
   - `data-elastic-include` and `data-elastic-exclude`, nestable, which choose the text that is
     indexed.
   - `<meta class="elastic" name=… content=…>`, which becomes a field.
   - `exclude_tags`.
6. **Index.** Documents are appended to the page's `_bulk` body as #318's `sync::Page` builds it.
   The body is charged to the request breaker as it grows, and a page is flushed at 500 documents
   or 20 MiB, through the connector's `pipeline` when it has one.
7. **Purge.** Only a full crawl that ends `completed` purges (see "Purge").

## Fetching

Every URL is the tenant's choice, so fetching follows AGENTS.md's rule for such URLs and goes
further:

- **Public addresses only.** Every request goes through roll's one egress check
  (`search-es` `egress.rs`), as every other outbound call of roll search does: names resolve
  through its resolver to public addresses that are neither the node's own nor in the operator's
  deny lists, addresses written out and `localhost` are checked before connecting, and nothing
  goes out until the node has resolved its own names. No proxy is used.
  `roll.connectors.allow_private_hosts` lifts the private-address rule for local development
  only, as it does for other connectors.
- **Redirects are followed by hand.** The client follows none itself. The crawler follows up to
  `max_redirects` (10), and each hop is a new URL checked like a link: public address, domains,
  crawl rules, robots.txt. A redirect leaving the crawl's domains ends there, and the source URL is
  counted as redirected away.
- **The credential stays on its origin.** `auth_header` or basic auth is sent only on requests
  whose origin is exactly `auth_origin` (scheme, host and port), never after a redirect to another
  origin, and never over plain `http`. Open Crawler's `http_auth_allowed` has no roll equivalent.
  Changing `auth_origin` without giving the credential again clears it, by #318's rule for fields
  that change where a credential goes.
- **Bounded reads.** A response is read up to `max_response_size` (10 MiB), counting decompressed
  bytes when the server compresses, so a compression bomb stops at the cap. Headers are capped at
  64 KiB. Only `text/html` and `application/xhtml+xml` pages are parsed. Sitemaps are read as XML
  with `quick-xml`, which builds no tree and resolves no entities. Other types are indexed as
  metadata only when `binary_content_extraction_mime_types` names them. roll has no `attachment`
  processor, so there is no PDF or Office text yet.
- **Timeouts.** Open Crawler's defaults apply: `connect_timeout` 10 s, `socket_timeout` 10 s and
  `request_timeout` 60 s. Every wait races the job's cancel, as connector syncs do.
- **Politeness.** One request at a time per origin, spaced by the larger of robots.txt's
  `Crawl-delay` and `roll.crawler.min_request_interval` (500 ms, a node setting). A 429 or 503
  with `Retry-After` pauses that origin for the time given, up to 10 minutes; beyond that, the
  origin's remaining URLs are skipped and counted. Requests carry the user agent
  `roll-crawler/<version> (+https://roll.sh/docs/crawler)`. A site can't be made to see another
  user agent.
- **Rate.** The crawl's bytes count against `roll.connectors.max_bytes_per_second`, like every
  connector sync.

### Domain verification

A crawler fetches someone's website from roll's addresses, so roll is answerable for what it
fetches. Algolia verifies a domain before crawling it, and so does roll. The difference is that an
unverified domain can still be crawled, within small limits, so the first crawl in the dashboard
works in a minute:

| | Unverified origin | Verified origin |
|---|---|---|
| URLs per crawl | 1,000 | `max_unique_url_count`, up to the operator's ceiling |
| Interval between requests | at least 1 s | at least `roll.crawler.min_request_interval` |
| Crawls per day | 4, per owner and site | by schedule |

What a verification covers depends on how it is proven, as with Google Search Console's domain
and URL-prefix properties:

- **A domain** (a registrable domain by the public suffix list: `docs.example.com` is
  `example.com`'s, each `*.github.io` is its own) is proven by a DNS TXT record
  `roll-site-verification=<token>` on it, and covers every origin on it, any host, scheme or port.
- **An origin** (`https://host[:port]`, exactly) is proven by the file
  `/.well-known/roll-site-verification.txt` holding the token, served by that origin itself: read
  through the crawler's own fetch path and egress check, with redirects followed only within the
  origin. It covers that origin alone. Controlling one host or port of a domain (a taken-over
  `blog.example.com`, a service on `example.com:8443`) proves nothing about its others.

A verification belongs to one cluster: its status, the limits it gives, its re-checks and its
lapse are that cluster's, and a crawl gets the verified limits when every origin it covers is
verified in its own cluster, by the origin's file or its domain's record; otherwise the
unverified ones. The token is the cluster's tenant's for the site (`SearchCluster::tenant`: the
workspace project it's at home in, else the person who owns it, else the cluster alone until its
project is recorded; the operator's for the operator's own clusters), the same in each of their
clusters, so once it is published, verifying the site in another of their clusters is one check
that passes at once. Another tenant gets a token of its own: two projects never share one. The
"owner" below is that tenant. Verifications kept from before tenants under the operator's key
were dropped once (every project's clusters shared it), and their clusters verify again. Every connector of a cluster shares its verifications. The daily count of unverified
crawls is the owner's, per site, so neither another cluster nor another host of the same domain
buys more. Deleting a cluster deletes its verifications (when it is tombstoned, purged or deleted
outright); a restored cluster starts without them and checks again.

The token isn't kept anywhere but the rows: it is derived, HMAC-SHA256 of the identity's length
(8 bytes), the identity and the site, the identity being the cluster's tenant (`sites::owner_of`: its home project, else its person
owner, else the cluster itself, or the operator's for the operator's own clusters), under a key
used for nothing else, derived (HKDF, its own label) from the deployment key, or on a
single node without one from the node's own key (`<home>/key`), and cut to 22 base64url characters
(132 bits). So starting a verification again, after every row that named the site is deleted or
expired, gives the same token, and a published record keeps working. Every check still reads the
token from its row:

- Rows made before tokens were derived keep their random token, which is published. A new row in
  another of the tenant's clusters reuses a token one of their clusters has verified with, so that
  record proves the site there in one check; otherwise it takes the derived token.
- Rotating the deployment key changes every derived token. New starts get the new one; a verified
  row keeps the token it was proven with, re-checked against it, until it lapses or is deleted,
  and then the tenant publishes the new one.
- A cluster whose tenant changes (handed to a project, its home recorded, or moved between
  projects) keeps its rows under the tenant that made them, so its verifications hold, but the new
  tenant's other clusters get the new tenant's token: a proof never moves from one tenant to
  another.

A verification is re-checked every 30 days (claimed in the catalogue first, so one node of a fleet
checks it; the claim is kept apart from the result, so a node that dies mid-check only delays it).
A failed re-check is retried after 1 hour, 6 hours and a day before the verification lapses to the
unverified limits, and a lapsed one is re-checked daily for a week, so a short outage costs
nothing. A check that couldn't be made (the site's request turn too far off: 10 s for a check
someone asked for, which then answers 429; 60 s for a re-check) is neither a pass nor a failure.
A re-check is tried again in 5 minutes, until its last check is a day past the 30-day interval;
from then on, one that can't be made counts as failed, so a site that never gives a turn can't
keep a verification. Pending verifications (never proven) are dropped after 7 days; a cluster
can have at most 20 pending, and an owner 100 across their clusters (409 past either). A cluster
that is deleted or being deleted can't start one. A host the crawler never fetches
(`blocked_hosts`, the fleet's names) can't be verified, and a check says only that the file is
`unreachable` (a timeout, a network failure and a refused address alike), isn't the file, or
doesn't hold the token, so it tells nothing about how a port answers or which names lead to a
private or fleet address.

Verifications, like each owner's crawls of unverified sites (the daily limit), are kept in roll's
catalogue (`search-control`: SQLite, or the fleet's shared Postgres), not in a cluster's state,
so a tenant can't write one through the cluster's API and the count holds across the fleet. The
owner node reads them when a crawl starts (`ClusterConfig::sites`). The routes sit under the
cluster (routed to its owner) and are classified in roll-access on that cluster, `read` to list
and `write` for the rest, so a grant on one cluster (a project member's, a token's) touches that
cluster's verifications alone:

- `GET /v1/search/clusters/{id}/site-verifications`: the cluster's domains and origins, each with
  the DNS record or file that proves it and when it is checked next;
- `POST /v1/search/clusters/{id}/site-verifications` `{"domain": …}` or `{"origin": …}`: starts
  one, answering what to publish (the same token again for one already started);
- `POST /v1/search/clusters/{id}/site-verifications/{site}/check`: checks now;
- `DELETE /v1/search/clusters/{id}/site-verifications/{site}`.

They're MCP tools (`search_list_site_verifications`, `search_verify_site`,
`search_check_site_verification`, `search_delete_site_verification`) and
`roll search crawler verify <cluster> <domain-or-url> [--check]` (a URL verifies its origin) and
`roll search crawler sites <cluster>`.

Checks and `crawl-test` make roll send requests, so they're held like a crawl's: refused while
crawlers are off on the node, each request in the site's turn at the unverified interval (shared
with every crawl on the node), and at most 10 a minute per owner on a node (429 past that).

`roll.crawler.enabled` (a node setting, on by default) lets an operator stop every crawler on a
node. `roll.crawler.blocked_hosts` refuses hosts outright, so an abuse report can be acted on in
minutes. The fleet's own names are blocked the same way, so a crawl never fetches roll itself, by
name as well as by the egress check's addresses: rolld puts every product's public, edge and
internal names in `roll.crawler.fleet_names` (the edge's domains, the dashboard, roll apps', roll
code's and its previews', storage's, NoSQL's and Alternator's, search's, and the advertised and
internal hosts; `served_names.rs`, where a test fails on a name argument left out). That list is
the crawler's alone, so other outbound calls' deny lists don't grow; `roll.egress.deny_names` (and
its old name, `roll.reindex.remote.deny_names`) is honoured too.

## Documents

The fields and the id are Open Crawler's, so an index Open Crawler filled can be taken over by
roll's crawler. The next full crawl then replaces each document under the same id:

- `id` and `_id` are the hex SHA-1 of the normalised URL, as in Open Crawler's
  `URL#normalized_hash`. Normalising drops the fragment and applies RFC 3986 normalisation: the
  scheme and host are lowercased, the default port is dropped, percent-encoding gets uppercase hex,
  and needlessly encoded unreserved characters are decoded. Open Crawler uses Ruby's
  `Addressable::URI#normalize!`. Its test vectors (`spec/lib/crawler/data/url_spec.rb`, Apache-2.0)
  are ported as roll's tests. A URL where the two normalisers still differ is listed in
  compatibility.md.
- HTML pages get `title`, `body`, `meta_keywords`, `meta_description`, `links`, `headings`,
  `last_crawled_at` (the crawl's start time), `url`, `additional_urls`, `url_scheme`, `url_host`,
  `url_port`, `url_path`, and `url_path_dir1` to `url_path_dir3`, each cut to Open Crawler's
  `max_*_size` and `max_*_count` limits. `full_html` is added when `full_html_extraction_enabled`
  is set. Empty values are left out.
- Binary files get `file_name`, `content_length` and `content_type`. They get no `_attachment`
  until roll has an attachment processor.
- Extraction rules add fields and can never overwrite the fields above, as in Open Crawler.

A new index is created with these mappings, which Open Crawler leaves to the user:

- `title`, `body`, `headings` and `meta_description` are `text`.
- `url`, `url_host`, `url_path`, `url_scheme` and `additional_urls` are `keyword`.
- `url_path_dir1` to `url_path_dir3` are `keyword`, so they work as facets.
- `last_crawled_at` is `date` and `url_port` is `long`.
- `links` is `keyword` with `index: false`.
- Other fields are dynamic.

The site-search guide then has no mapping step.

### Parsing

HTML is parsed with `html5ever`, the WHATWG parser used by Servo, so malformed pages parse as
browsers parse them. Parsing builds a small arena DOM because extraction rules need selectors. By
the charge-what-you-build rule, each node and text run is charged to the request breaker as the tree
builder makes it, and the charge is held until the document is serialised into the page body. A
page of a million empty `<p>` tags trips `circuit_breaking_exception` for that URL, which is counted
as a failed document, rather than growing the node. html5ever puts no limit on nesting and scans its
stack of open elements for every element it opens, so the tree builder fails a page as soon as it
nests more than 512 elements deep (a failed document), and a parse runs off the async workers, fed
in 4 KiB chunks against a 10-second deadline. Reading the DOM never recurses. Nothing in the page
runs: no scripts, no styles, no external fetches.

Extraction rules use Open Crawler's ruleset shape, under each domain's `extraction_rulesets` in the
advanced snippet (and carried over by `roll search crawler import`):

- `url_filters`: a ruleset applies to a page whose path one filter takes (`begins`, a path
  starting with `/`; `ends`; `contains`; `regex`), or to every page when it has none.
- `rules`, each with `action` (`extract` or `set`), `field_name`, `selector`, `join_as` (`array`
  or `string`, the values joined with spaces), `source` (`html` or `url`) and, for `set`, `value`:
  - `extract` from `html`: the text of each element the selector picks (as the body's text is
    gathered); from `url`: each match of the regex over the URL, its first group when it has one.
  - `set`: `value` when the selector picks anything (or the regex matches).
- A field name is lowercase letters, digits and `_`, and never one of the fields every page has,
  so a rule can't overwrite them; a rule's field wins over the page's own `meta.elastic` and
  `data-elastic-name` fields, and a later rule's over an earlier one's.
- Per page, a rule takes at most 100 values of 32 KiB each, and all its rules together 1 MiB,
  charged to the request breaker as they're taken. The text of the elements rules pick shares one
  budget of node visits per page (four per node of the page; a node's text costs one more per KiB
  and each child queued one, and text is taken word by word only as far as there's room), so rules
  picking nested elements can't walk a subtree again and again, nor copy a huge text node. Past the 1 MiB or the visits, the page's remaining rules
  are dropped: the page is indexed with what was extracted, and the job's metadata counts the
  pages (`extraction_rules_dropped_page_count`; `crawl-test` answers `extraction_rules_dropped`).
- A crawl has at most 100 rulesets and 100 rules, a ruleset at most 20 URL filters, and the crawl
  at most 200 regexes in its rules (`regex` filters and `url` rules). They share the crawl rules'
  10 MiB: every regex, a crawl rule's or an extraction rule's, gets an equal share, and one that
  needs more fails validation. An extraction regex's engines are all held inside its share: the
  NFA and the one-pass table a quarter each (a larger one-pass table isn't built), the lazy DFA's
  caches an eighth, no backtracker and no full DFA, and what was built is measured against it.
  Regexes are ASCII (Unicode off), as Open Crawler's Ruby `\w` is and URLs are.

`html` selectors are CSS Selectors Level 3 without what needs more than an element's ancestors and
earlier siblings (`crawler/select.rs`, roll's own): types, `#id`, `.class`, attribute selectors
(`=`, `~=`, `|=`, `^=`, `$=`, `*=`, and `i`), the structural pseudo-classes (`:root`, `:empty`,
`:first-child`, `:nth-child()`, `:only-of-type` and the rest), `:not()` of one simple selector, and
the four combinators. A selector is at most 1 KiB, 16 selectors in a list, 32 compound parts across
it and 16 conditions on one element; `:has()`, states (`:hover`), pseudo-elements, escapes and
XPath (which Open Crawler also accepts) fail validation, each with its reason. A page's elements are
indexed once (charged to the request breaker, like its nodes, with their attributes), and a
selector list is matched in one pass: each element inherits from its parent which parts matched an
ancestor and from its previous sibling which matched an earlier sibling, so every combinator is a
bit test and nothing backtracks. What a condition costs depends on the attribute it reads, so the
match counts the bytes it touches; attribute values are compared in place, and ` i` folds ASCII
only (Selectors Level 4's definition), allocating nothing. The deadline is checked between rules,
at the start of a match and after every 64 KiB of its work (an element counts as 64 bytes, and
each attribute byte touched as one, so one huge attribute can't hold a match and the clock isn't
read per element), and as an element's text is gathered: a page whose rules can't
finish by its parse deadline fails, `crawl-test`'s too. `url` selectors and `regex` filters are regexes with the same linear-time engine
as crawl rules (no look-around or backreferences).

## Purge

Open Crawler's purge re-fetches every document the primary crawl didn't see and deletes any that
don't answer 200. A site that is down during the purge, or a CDN serving 503s for a minute, wipes
the index. roll keeps Open Crawler's re-check and deletes a document only for something that proves
the page is gone:

- **Deleted:** a 404 or a 410, a `noindex` page, a URL now excluded by the crawl's domains, crawl
  rules or robots.txt, and a page naming as canonical a URL the crawl indexed.
- **Kept:** 5xx responses, other 4xx (401, 403), timeouts, network errors, an origin whose
  robots.txt couldn't be read or disallows the whole site, and signs that may only be an outage: a
  redirect off the crawl (a login), onto a URL the crawl already has (a maintenance page) or onto
  a new one (the page moved), a body that isn't HTML, or a page with no title and no text (a soft
  404). The document's `last_crawled_at` stays as it was. A document this crawl kept, which also
  wasn't refreshed by the two full crawls before it (they kept it or didn't get to it) and is at
  least a week old (`roll.crawler.strike_min_age`), is deleted, and the job's metadata names the
  count.
- **Purge skipped:** when more than half of the index's documents weren't refreshed by the crawl
  (gone, kept or not re-checked; documents the crawl's own configuration now leaves out, by its
  domains, rules or URL limits, don't count and are decided as they're listed, before any budget
  or deadline, so removing a domain purges it even from a crawl that used its whole budget), or when ten redirects or more of re-checked documents mostly led
  to one URL. And per origin: nothing of an origin more than half of whose documents weren't
  refreshed is deleted (`metadata.purge_skipped_origins`), so one domain of a crawl that ships
  `Disallow: /` or 404s everywhere keeps its documents. A site that is down, behind an expired
  login, in maintenance or wrongly deployed then loses nothing, however many crawls it lasts. The
  job ends `completed` with `metadata.purge_skipped: true` and its reason. Pages that name an
  indexed canonical URL lose their own documents only when the purge runs.
- **Forced:** the connector's `force_purge` field lifts the guards for the next full crawl, then
  turns itself off: for a site that really removed more than half its pages.

Unseen documents are found with a `last_crawled_at` range query over the crawler's own index, and
re-fetched with the same politeness. The re-check counts against `max_unique_url_count` and
`max_duration`.

## Incremental crawls

An incremental crawl fetches three sets of URLs: the seeds, sitemap entries whose `<lastmod>` is
newer than the last crawl's start, and links found on the pages it fetches. Each request carries
`If-Modified-Since: <last_crawled_at>` when the document exists. A 304 counts as seen and leaves
the document alone. Incremental crawls never purge. Open Crawler has no incremental mode.

## Limits

Open Crawler's limits keep their names and defaults. roll adds a node-level ceiling to each, which
an operator can raise, and the tenant's value must be at or under the ceiling.

| Key | Default | roll's ceiling |
|---|---|---|
| `max_unique_url_count` | 10,000 (Open Crawler: 100,000) | 1,000,000 |
| `max_crawl_depth` | 10 | 50 |
| `max_duration` | 6 h for the whole crawl (Open Crawler: 24 h) | `roll.connectors.max_sync_duration` |
| `max_response_size` | 10 MiB | 10 MiB |
| `max_url_length`, `max_url_segments`, `max_url_params` | 2048, 16, 32 | the same |
| `max_redirects` | 10 | 10 |
| `max_extracted_links_count` | 1,000 per page | 1,000 |
| `url_queue_size_limit` | 10,000 | 100,000 |
| `max_title_size`, `max_body_size`, `max_keywords_size`, `max_description_size`, `max_elastic_tag_size` | Open Crawler's (1 KiB, 5 MiB, 512, 1 KiB, 512) | the same |
| `max_indexed_links_count`, `max_headings_count` | Open Crawler's (25, 25) | 100, 100 |

An operator moves a ceiling with the node setting `roll.crawler.limits.<key>`; `max_duration`'s is
`roll.connectors.max_sync_duration`. The unverified limits are node settings too
(`roll.crawler.unverified.max_unique_url_count`, `.min_request_interval` and
`.max_crawls_per_day`), and `roll.crawler.min_request_interval` is 500 ms. A crawl that reaches
`max_unique_url_count` or `max_duration` ends `completed`, with `metadata.limit_reached` naming the
limit.

The frontier holds URLs as text and is capped by `url_queue_size_limit`. A link that arrives when
the queue is full is dropped and counted in `metadata.dropped_link_count`. The visited set holds
64-bit hashes, 8 bytes per URL, as full syncs keep seen ids. So a crawl's memory is bounded by its
limits, not by the site. Crawler traps such as endless calendars and session ids in URLs end at
`max_unique_url_count`, `max_url_params` and `max_crawl_depth`, and the job's metadata shows which
limit stopped the crawl.

Unlike Open Crawler's `threads_per_crawl`, a crawl fetches from at most 4 origins at once and makes
one request per origin. Crawls share the node's connector slots.

## Checking a URL

Algolia's URL tester is how people debug extraction. roll's version is
`POST /v1/search/clusters/{id}/crawl-test` with `{"url": …, "connector"?: …}`. It is routed
to the cluster's owner, and classified in roll-access as `write` on the cluster: it stores
nothing, but it makes roll send requests (held as checks are; see above). It fetches one URL with the connector's configuration (or, with none, as a
crawl of the URL's site would) through the crawler's own path (the same plan, egress check,
robots.txt reading, verdicts, redirects, credential rules and parsing) and answers four things:

- the robots.txt verdict and the crawl rule that decided;
- the redirect chain;
- the document that would be indexed, with fields cut as configured;
- the links that would be queued, each with its verdict.

robots.txt is read for the URL's origin and each redirect's, and for at most 5 more of the
crawl's origins its links lead to; a link on another is shown as `robots_not_read`. One test takes
75 s at most, waits and requests included, and the page's memory is let go before the links are
decided; a link whose origin's robots.txt can't be read in time is `robots_not_read` too, and the
page's parse is held to the test's deadline. A failed fetch says `unreachable` for a timeout, a
network failure and a refused address alike (`too_large` stays: the page answered). The
credential is used only by the fetch's rules above, and never appears in the answer. The answer
is capped at 1 MiB (the body shown is cut at 64 KiB). It is a tool in roll's MCP
(`search_crawl_test`) and `roll search crawler test <cluster> <url> [--connector <id>]`. The
dashboard's Connectors tab has a URL box that calls it: its "crawl a site" empty state tests a
URL, then makes it a `roll_web_crawler` connector under an id no connector has and starts the
first crawl, and offers to verify the URL's domain (DNS) or just its origin (the file).

## Metering and usage

A crawl is a connector sync: its documents and bytes count as #318's syncs do. The job's metadata
adds `fetched_page_count`, `fetched_bytes`, `robots_disallowed_count`, `redirected_away_count`,
`error_count` by class, and the limit that stopped the crawl, if any. These are what a person needs
to see why pages are missing.

## Not in the first slice

- **JavaScript rendering** (Algolia's `renderJavaScript`). It would run third-party scripts in a
  headless browser. Under the rule that customer code never runs on a fleet node, it would run in
  roll apps' `Runtime` (a container), never in the node's process. That needs its own design.
- **XPath selectors.**
- **PDF and Office text,** which waits for an attachment processor.
- **Duplicate content handling** (Elastic's hosted crawler had it; Open Crawler doesn't).
- **A docs preset** in the DocSearch style: `lvl0`…`lvl6` records per heading, with a mapping and
  a search-as-you-type template.

## Tests

| What | Where |
|---|---|
| Crawl rules, robots.txt (RFC 9309's examples), URL normalisation and ids (Open Crawler's vectors), and directive parsing | unit tests in `xpack/entsearch/sync/crawler/` |
| A crawl end to end against a fixture site served by the test on loopback (`allow_private_hosts`): sitemaps, canonical URLs, nofollow, redirects off-domain, a credential not sent after a cross-origin redirect, purge with a 503 window, and incremental with 304 | `crates/search-es/tests/` beside #318's sync tests |
| Hostile pages: a compression bomb, a million-node page under a small breaker, an endless calendar, and a sitemap with entities | the same file, one test each |
| `crawl-test` answering; an origin verified by its file (a redirect to another port refused), the higher limits for that origin only | `search-control/tests/control.rs` (`crawler_sites_verified_for_higher_limits`), against a fixture site on loopback |
| Verifications are each cluster's: the owner's other cluster checks once with the same token; another owner gets nothing and a token of their own; past 10 probes a minute, 429 | the same file (`crawler_verifications_are_each_clusters`; the 429 in `crawler_sites_verified_for_higher_limits`) |
| The daily limit of unverified crawls is the owner's, across connectors, in the catalogue, and a refusal spends nothing | the same file (`crawler_daily_limit_is_the_owners_across_connectors`) |
| What a domain and an origin cover | unit test in `crawler/verify.rs` |
| A project's grant verifies on its own cluster and gets 403 on another project's | `rolld/tests/auth.rs` (`mcp_search_with_a_project_token`) |
| Re-checks retried, then lapsed, then re-checked for a week | unit test in `search-control/src/store.rs` |
| Every rolld argument naming a host roll serves reaches `roll.crawler.fleet_names` | unit test in `rolld/src/served_names.rs` |
| The derived token is the same after a delete and a new start | `search-control/tests/control.rs` (`crawler_verifications_are_each_clusters`) |
| Extraction rules end to end: a URL filter, `extract` as a string and an array, `set`, a URL group | `crates/search-es/tests/crawler.rs` (`extraction_rules_fill_fields`) |
| Selector parsing bounds and one-pass matching; a huge attribute on a page of a few elements meets the deadline | unit tests in `crawler/select.rs` |
| Extraction regexes share the crawl rules' budget (too many, or too big for their share, are refused; a capture group's one-pass table stays within it; ASCII `\w`) | unit test in `crawler/config.rs` |
| A rule whose hits nest spends a page's text visits: later rules are dropped and counted, earlier fields stay | unit test in `crawler/html.rs` |
| A tenant change carries no proof across owners | `search-control/tests/control.rs` (`crawler_site_tokens_follow_the_clusters_project`) |
| The routes' catalogue rows; the MCP tools | `rolld/tests/auth.rs`; docs/mcp.md and `pnpm test:mcp-sweep` |
| The dashboard's crawl box | `apps/dashboard/src/components/search/connectors.test.tsx` |
| `roll search crawler import` | `roll-cli/tests/search.rs` |

## Phases

1. **The crawl:** `roll_web_crawler`, robots.txt, sitemaps, crawl rules, default fields, purge,
   the limits and politeness. Unverified limits only, with the CLI import. It lands after #318.
2. **Domain verification, `crawl-test` and the dashboard:** the URL box, and a "crawl a site"
   path in the empty state for `## Next` item 12. The site-search guide's "Index your pages"
   becomes three commands.
3. **Extraction rules, incremental crawls and the docs preset.** Extraction rules landed, with the
   derived verification token; incremental crawls and the docs preset are left.
