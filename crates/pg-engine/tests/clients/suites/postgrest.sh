# PostgREST v16.5 (the static binary of its release) over its own database: the schema cache it
# loads at startup (its catalog queries), then HTTP requests over a Unix socket covering reads
# with filters, embedding through every relationship kind, ordering and paging, counts,
# aggregates, the OpenAPI document, RPC, inserts, upserts, updates and deletes, and errors. The
# fixture is postgrest.setup.sql (postgrest_test, roles pgrst_authenticator / pgrst_anon).
# PostgREST's own spec tests need its Haskell toolchain and a database it starts itself, so a
# pinned request list instead: one id per request, `<n>:<method> <path>#<hash>`, the hash over
# the status, the headers a client reads and the body (JSON as parsed), so a request answered
# differently shows as one id per server; the responses are in $OUT/responses.txt.
# image: python:3.12-slim
# src:
set -eu
B=/cache/src/postgrest/postgrest
if [ ! -x $B ]; then
  mkdir -p /cache/src/postgrest
  python - <<'EOF'
import io, lzma, tarfile, urllib.request
url = "https://github.com/PostgREST/postgrest/releases/download/v16.5/postgrest-v16.5-linux-static-x86-64.tar.xz"
data = urllib.request.urlopen(url).read()
tarfile.open(fileobj=io.BytesIO(lzma.decompress(data))).extractall("/cache/src/postgrest")
EOF
fi
$B --version
S=/tmp/pgrst.sock
export PGRST_DB_URI="postgres://pgrst_authenticator:pw@$PGHOST:$PGPORT/postgrest_test"
export PGRST_DB_SCHEMAS=api PGRST_DB_ANON_ROLE=pgrst_anon PGRST_SERVER_UNIX_SOCKET=$S
export PGRST_DB_AGGREGATES_ENABLED=true PGRST_LOG_LEVEL=info PGRST_DB_POOL=4
$B > "$OUT/postgrest.log" 2>&1 &
P=$!
cat > /tmp/requests.txt <<'REQ_EOF'
GET /
GET /authors
GET /authors?select=name,born&order=born.desc.nullslast
GET /authors?select=id,name,tags&tags=cs.{scifi}
GET /authors?tags=ov.{fantasy,literary}&select=name
GET /authors?mood=eq.happy&select=name,mood
GET /authors?born=is.null
GET /authors?bio->>country=eq.US&select=name,country:bio->>country,awards:bio->awards
GET /authors?bio=not.is.null&select=name,bio->country
GET /authors?name=like.*Le*&select=name
GET /authors?name=ilike.*CALVINO&select=name
GET /authors?name=match.^[SU]&select=name
GET /authors?or=(born.lt.1922-01-01,mood.is.null)&select=name
GET /authors?and=(born.gt.1920-01-01,born.lt.1925-01-01)&select=name&order=name
GET /authors?id=in.(1,3,99)&select=id,name
GET /authors?id=not.in.(1,2)&select=id
GET /authors?select=name,books(title,pages)&order=id
GET /authors?select=name,books(title,reviews(reviewer,stars))&id=eq.1
GET /authors?select=name,books!inner(title)&books.pages=gt.290&order=name
GET /authors?select=name,books(title)&books.order=pages.desc&books.limit=1&order=id
GET /authors?select=*,authors_book_count&order=id
GET /authors?select=name,book_count:authors_book_count&authors_book_count=gt.1
GET /books?select=title,authors(name)&order=id
GET /books?select=title,author:authors(name,born)&id=lte.3&order=id
GET /books?select=title,publishers(name,country)&order=id
GET /books?select=title,book_publishers(edition,publishers(name))&id=eq.6
GET /books?select=title,full_title:books_full_title&id=lte.2&order=id
GET /books?select=title,price&price=gte.10&price=lt.20&order=price
GET /books?select=title&order=rating.desc.nullsfirst,title
GET /books?select=title,meta->>series&meta->>series=eq.Hainish&order=id
GET /books?select=title&meta=cs.{"awards":["Hugo"]}
GET /books?select=title&search=fts(english).wizard
GET /books?select=title&search=plfts.left%20hand
GET /books?select=title&search=wfts(english).cities%20-left
GET /books?select=title&search=phfts(english).winter%20night
GET /books?select=title&published=gte.1970-01-01&order=published
GET /books?select=id,title&limit=2&offset=1&order=id
GET /books?select=id&order=id	Range-Unit: items	Range: 2-4
GET /books?select=id&order=id&limit=3	Prefer: count=exact
GET /books?select=id&pages=gt.200	Prefer: count=exact
GET /books?select=title,price&order=id	Accept: text/csv
GET /books?id=eq.4&select=title,pages	Accept: application/vnd.pgrst.object+json
GET /books?id=gt.100	Accept: application/vnd.pgrst.object+json
GET /books?select=author_id,count(),total:pages.sum(),avg_price:price.avg(),max:rating.max()&order=author_id
GET /books?select=pages.sum()
GET /reviews?select=reviewer,stars,books(title)&order=book_id,reviewer
GET /reviews?select=*,review_votes(voter,up)&book_id=eq.1&order=reviewer
GET /reviews?period=ov.[2024-01-15,2024-01-20)&select=book_id,reviewer
GET /reviews?period=sr.[2023-06-01,2023-07-01)&select=book_id,reviewer
GET /reviews?body=is.null&select=book_id,reviewer
GET /review_votes?select=voter,up,reviews(stars,body)&order=voter
GET /employees?select=name,manager:employees!manager_id(name)&order=id
GET /employees?select=name,reports:employees!manager_id(name)&id=eq.1
GET /employees?select=name,salary&salary=gt.4000&order=salary.desc
GET /events?select=kind,at,ip&order=at
GET /events?ip=eq.10.0.0.1&select=kind
GET /events?select=kind,payload->>user&payload->via=not.is.null
GET /events?id_uuid=eq.6ba7b810-9dad-11d1-80b4-00c04fd430c8&select=kind
GET /events?at=gte.2025-03-01T10:30:00Z&select=kind,at&order=at
GET /book_summary?order=id
GET /book_summary?select=title,author,stars&stars=not.is.null&order=stars.desc,title
GET /cheap_books?select=title,price&order=price
GET /cheap_books?select=title,authors(name)&order=id
GET /author_stats?order=author_id
GET /publishers?select=name,books(title)&order=id
GET /rpc/add_them?a=1&b=2
GET /rpc/add_them?a=5
POST /rpc/add_them	{"a": 40, "b": 2}
GET /rpc/search_books?q=darkness&select=title
GET /rpc/search_books?q=the&select=title,authors(name)&order=title
GET /rpc/get_author?author_id=2
GET /rpc/get_author?author_id=2&select=name,books(title)
GET /rpc/top_rated
GET /rpc/top_rated?min_stars=3&stars=lt.5
POST /rpc/sum_all	{"xs": [1, 2, 3, 4]}
GET /rpc/sum_all?xs=5&xs=6
POST /rpc/echo_json	{"j": {"a": [1, 2, {"b": null}], "c": "d"}}
POST /rpc/bump_price	{"book": 4, "pct": 10}
POST /rpc/bump_price	{"book": 999, "pct": 10}
GET /rpc/bump_price?book=4&pct=1
GET /rpc/nosuch
POST /authors	{"name": "Octavia Butler", "born": "1947-06-22", "tags": ["scifi"]}	Prefer: return=representation
POST /authors	[{"name": "A1"}, {"name": "A2", "mood": "sad"}]	Prefer: return=representation
POST /authors?columns=name,born	[{"name": "B1", "born": "2000-01-01", "extra": 1}, {"name": "B2"}]	Prefer: return=representation
POST /authors	{"name": null}
POST /authors	{"name": "X", "mood": "angry"}
POST /authors	{"name": "X", "nosuch": 1}
POST /books	{"author_id": 1, "title": "Bad", "pages": 0}
POST /books	{"author_id": 999, "title": "Orphan"}
POST /books	{"author_id": 1, "title": "Dup", "isbn": "978-0441478125"}
POST /books	{"author_id": 1, "title": "Bad isbn", "isbn": "x"}
POST /books	{"author_id": 2, "title": "Fiasco", "isbn": "978-0156306393", "pages": 322, "price": 17.5}	Prefer: return=headers-only
POST /publishers	[{"id": 3, "name": "Einaudi", "country": "IT"}, {"id": 4, "name": "Gollancz", "country": "GB"}]	Prefer: resolution=merge-duplicates,return=representation
POST /publishers	[{"id": 4, "name": "Gollancz2", "country": "GB"}]	Prefer: resolution=ignore-duplicates,return=representation
POST /publishers	{"id": 5, "name": "Ace", "country": "US"}
PUT /publishers?id=eq.6	{"id": 6, "name": "Tor", "country": "US"}	Prefer: return=representation
PATCH /books?id=eq.3	{"price": 9.5, "meta": {"series": "Earthsea", "book": 1}}	Prefer: return=representation
PATCH /books?author_id=eq.2&select=id,price	{"price": 13}	Prefer: return=representation
PATCH /employees?id=eq.3	{"salary": 3300}	Prefer: return=representation
PATCH /employees?id=eq.3	{"name": "Senior"}
DELETE /reviews?book_id=eq.6&reviewer=eq.cy	Prefer: return=representation
DELETE /authors?name=in.(A1,A2,B1,B2)&select=name	Prefer: return=representation
DELETE /books?title=eq.Fiasco
GET /authors?select=id,name&order=id
GET /books?select=id,title,price,meta&order=id
GET /publishers?order=id
GET /review_votes?order=book_id,voter
GET /nosuch
GET /authors?select=nosuch
GET /authors?id=eq.notanumber
GET /authors?select=name,nosuch(*)
GET /books?select=title,employees(*)
GET /authors?order=nosuch
HEAD /books?select=id	Prefer: count=exact
OPTIONS /authors
REQ_EOF
python - <<'PY'
import hashlib, http.client, json, os, socket, time

S, OUT = "/tmp/pgrst.sock", os.environ["OUT"]


class Conn(http.client.HTTPConnection):
    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(S)


def request(method, path, body=None, headers=None):
    c = Conn("localhost", timeout=60)
    c.request(method, path, body=body, headers=headers or {})
    r = c.getresponse()
    return r.status, dict(r.getheaders()), r.read().decode("utf-8", "replace")


ready = False
for _ in range(120):
    try:
        st, _, _ = request("GET", "/publishers?select=id&limit=1")
        if st == 200:
            ready = True
            break
    except OSError:
        pass
    time.sleep(0.5)
KEEP = ("content-range", "content-type", "location", "preference-applied", "content-length")
with open("/tmp/requests.txt") as f, open(f"{OUT}/responses.txt", "w") as rep, open(f"{OUT}/postgrest.lst", "w") as lst:
    lst.write(f"{'PASS' if ready else 'FAIL'} startup::schema_cache\n")
    for n, line in enumerate(l.rstrip("\n") for l in f if l.strip()):
        parts = line.split("\t")
        method, path = parts[0].split(" ", 1)
        body, headers = None, {}
        for p in parts[1:]:
            if p.startswith(("{", "[")):
                body = p
                headers["Content-Type"] = "application/json"
            else:
                k, v = p.split(": ", 1)
                headers[k] = v
        try:
            st, hdr, text = request(method, path, body, headers)
            hdr = {k.lower(): v for k, v in hdr.items()}
            if method == "HEAD" or hdr.get("content-type", "").startswith("text/csv"):
                shown = text
            else:
                try:
                    shown = json.dumps(json.loads(text), sort_keys=False) if text else ""
                except ValueError:
                    shown = text
            kept = {k: hdr[k] for k in KEEP if k in hdr and not (k == "content-length" and method != "HEAD")}
            res = f"{st} {json.dumps(kept, sort_keys=True)}\n{shown}"
        except Exception as e:  # noqa: BLE001
            res = f"EXC {type(e).__name__}: {e}"
        rid = f"{n:03d}:{method} {path}"
        h = hashlib.sha1(res.encode()).hexdigest()[:10]
        rep.write(f"== {rid}\n{res}\n")
        lst.write(f"PASS {rid}#{h}\n")
PY
kill $P 2>/dev/null || true
wait $P 2>/dev/null || true
tail -5 "$OUT/postgrest.log"
