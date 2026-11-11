// Directus against the server (directus.sh): its migrations and system collections, then
// what its API does to the database: collections and fields made, altered and dropped (DDL),
// relations (foreign keys), items read with filters, sorts, deep relational fields, search,
// aggregates and groupings, written and deleted, the errors constraints raise, and the
// schema snapshot it reads from the catalog. One id per step, `<step>#<hash>` of the
// normalized answer, in $OUT/directus.lst; answers in $OUT/responses.txt.
import crypto from "node:crypto";
import fs from "node:fs";

const OUT = process.env.OUT;
const BASE = `http://127.0.0.1:${process.env.APP_PORT || 52454}`;
const lst = fs.createWriteStream(`${OUT}/directus.lst`);
const resp = fs.createWriteStream(`${OUT}/responses.txt`);
const VOL = /^(id|uid|key|date_created|date_updated|user_created|user_updated|timestamp|uuid|token|access_token|refresh_token|expires|last_access|last_page|ip|user_agent|origin|revisions|activity|.*_id|project_id|version|nodeID|directus)$/;
const TS = /\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?(Z|[+-]\d\d:?\d\d)?/g;
const UUID = /[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/g;

function norm(v) {
  if (Array.isArray(v)) return v.map(norm);
  if (v && typeof v === "object") {
    const o = {};
    for (const k of Object.keys(v).sort()) if (!VOL.test(k)) o[k] = norm(v[k]);
    return o;
  }
  if (typeof v === "string") return v.replace(TS, "<ts>").replace(UUID, "<uuid>");
  return v;
}

function record(step, value, ok = true) {
  const text = JSON.stringify(value);
  const h = crypto.createHash("sha256").update(text).digest("hex").slice(0, 12);
  lst.write(`${ok ? "PASS" : "FAIL"} ${step}#${h}\n`);
  resp.write(`== ${step} #${h}\n${text.slice(0, 4000)}\n`);
}

let token = "";
async function call(method, path, body) {
  try {
    const r = await fetch(BASE + path, {
      method,
      headers: { "Content-Type": "application/json", ...(token ? { Authorization: `Bearer ${token}` } : {}) },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    const t = await r.text();
    let j;
    try { j = t ? JSON.parse(t) : null; } catch { j = t.slice(0, 2000); }
    return [r.status, j];
  } catch (e) {
    return [0, { error: String(e.name) }];
  }
}

async function step(name, method, path, body, keep) {
  const [st, j] = await call(method, path, body);
  const shown = j && j.errors ? errs(j) : keep ? keep(j) : j;
  // Errors: their codes and messages (Directus words the database's own messages).
  record(name, { status: st, body: norm(shown) }, st > 0 && st < 500);
  return [st, j];
}

const data = (j) => (j && j.data !== undefined ? j.data : j);
const errs = (j) => (j && j.errors ? j.errors.map((e) => [e.message, e.extensions && e.extensions.code]) : data(j));

async function main() {
  const t0 = Date.now();
  for (let i = 0; i < 900; i++) {
    const [st] = await call("GET", "/server/ping");
    if (st === 200) break;
    await new Promise((r) => setTimeout(r, 1000));
  }
  console.log(`directus up after ${(Date.now() - t0) / 1000}s`);
  const [st, login] = await call("POST", "/auth/login", { email: "admin@example.com", password: "admin-pass-123" });
  token = login && login.data ? login.data.access_token : "";
  record("login", { status: st });
  await step("health", "GET", "/server/health", undefined, (j) => j && j.status);
  await step("collections:system", "GET", "/collections", undefined, (j) => (data(j) || []).map((c) => c.collection).sort());
  await step("snapshot:before", "GET", "/schema/snapshot", undefined, (j) => data(j) && data(j).collections);
  // Collections and fields: CREATE TABLE / ALTER TABLE through Directus's schema layer.
  await step("coll:authors", "POST", "/collections", {
    collection: "authors", schema: {},
    fields: [{ field: "id", type: "integer", schema: { is_primary_key: true, has_auto_increment: true } }],
  }, (j) => (data(j) || {}).collection);
  for (const f of [
    { field: "name", type: "string", schema: { is_nullable: false, is_unique: true, max_length: 120 } },
    { field: "email", type: "string", schema: { is_nullable: true } },
    { field: "born", type: "date" },
    { field: "rating", type: "decimal", schema: { numeric_precision: 4, numeric_scale: 2 } },
    { field: "bio", type: "json" },
    { field: "active", type: "boolean", schema: { default_value: true } },
  ]) {
    await step("field:authors." + f.field, "POST", "/fields/authors", f, (j) => (data(j) || {}).schema);
  }
  await step("coll:books", "POST", "/collections", {
    collection: "books", schema: {},
    fields: [
      { field: "id", type: "uuid", schema: { is_primary_key: true }, meta: { special: ["uuid"] } },
      { field: "title", type: "string", schema: { is_nullable: false } },
      { field: "pages", type: "integer", schema: { default_value: 100 } },
      { field: "price", type: "float" },
      { field: "published", type: "timestamp" },
      { field: "tags", type: "csv", meta: { special: ["cast-csv"] } },
      { field: "author", type: "integer" },
    ],
  }, (j) => (data(j) || {}).collection);
  await step("rel:book_author", "POST", "/relations", {
    collection: "books", field: "author", related_collection: "authors",
    schema: { on_delete: "SET NULL" }, meta: { one_field: "books" },
  }, (j) => data(j) && [data(j).collection, data(j).related_collection, data(j).schema && data(j).schema.on_delete]);
  await step("field:alias", "POST", "/fields/authors", { field: "books", type: "alias", meta: { special: ["o2m"] } },
    (j) => (data(j) || {}).field);
  await step("field:add", "POST", "/fields/books", { field: "isbn", type: "string", schema: { max_length: 20, is_unique: true } },
    (j) => (data(j) || {}).schema);
  await step("field:alter", "PATCH", "/fields/books/pages", { schema: { default_value: 250, is_nullable: false } },
    (j) => (data(j) || {}).schema);
  await step("fields:books", "GET", "/fields/books", undefined,
    (j) => (data(j) || []).map((f) => [f.field, f.type, f.schema && [f.schema.data_type, f.schema.is_nullable, f.schema.default_value, f.schema.is_unique, f.schema.foreign_key_table]]));
  // Items.
  const authors = [];
  for (let i = 1; i <= 8; i++) authors.push({ name: `Author ${i}`, email: i % 3 ? `a${i}@example.com` : null, born: `19${50 + i}-0${1 + (i % 9)}-1${i % 9}`, rating: (i * 0.7).toFixed(2), bio: { country: ["US", "IT", "PL"][i % 3], awards: i % 2 }, active: i % 4 !== 0 });
  await step("items:authors", "POST", "/items/authors", authors, (j) => (data(j) || []).map((a) => a.name));
  const books = [];
  for (let i = 1; i <= 30; i++) books.push({ title: `Book ${i}`, pages: 50 + i * 13, price: i * 1.25, published: `2020-01-${String(1 + (i % 28)).padStart(2, "0")}T10:00:00`, tags: ["sf", i % 2 ? "classic" : "new"], author: 1 + (i % 8), isbn: `isbn-${i}` });
  await step("items:books", "POST", "/items/books", books, (j) => (data(j) || []).length);
  const q = (name, path, keep) => step("query:" + name, "GET", path, undefined, keep || data);
  await q("filter", "/items/books?filter[pages][_gt]=300&sort=-pages&fields=title,pages&limit=5");
  await q("filter_and", "/items/books?filter[_and][0][price][_between]=10,20&filter[_and][1][title][_icontains]=book 1&sort=title&fields=title,price");
  await q("in_null", "/items/authors?filter[_or][0][email][_null]=true&filter[_or][1][id][_in]=1,2&sort=id&fields=name,email");
  await q("deep", "/items/authors?fields=name,books.title&deep[books][_sort]=title&deep[books][_limit]=2&sort=name");
  await q("m2o", "/items/books?fields=title,author.name,author.bio&sort=title&limit=4");
  await q("rel_filter", "/items/books?filter[author][name][_eq]=Author 3&fields=title&sort=title");
  await q("json", "/items/authors?filter[bio][country][_eq]=IT&fields=name&sort=name");
  await q("search", "/items/books?search=Book 2&fields=title&sort=title");
  await q("aggregate", "/items/books?aggregate[count]=*&aggregate[sum]=pages&aggregate[avg]=price&aggregate[max]=published");
  await q("group", "/items/books?aggregate[count]=id&aggregate[avg]=pages&groupBy[]=author&sort=author");
  await q("group_date", "/items/books?aggregate[count]=*&groupBy[]=year(published)&groupBy[]=month(published)");
  await q("meta", "/items/books?meta=*&limit=1&fields=title&sort=title", (j) => j && j.meta);
  await q("page", "/items/books?limit=3&page=4&sort=pages&fields=pages");
  await q("functions", "/items/books?fields=title,day(published),hour(published)&filter[year(published)][_eq]=2020&limit=3&sort=title");
  // Writes and the errors they hit.
  await step("update", "PATCH", "/items/books", { query: { filter: { pages: { _lt: 200 } } }, data: { price: 0.5 } },
    // (In the order of the books' random uuids: sorted.)
    (j) => (data(j) || []).map((b) => b.title).sort());
  await step("update_many", "PATCH", "/items/authors", { keys: [1, 2], data: { active: false } }, (j) => (data(j) || []).map((a) => [a.name, a.active]));
  await step("err_unique", "POST", "/items/authors", { name: "Author 1" });
  await step("err_null", "POST", "/items/books", { title: null });
  await step("err_fk", "POST", "/items/books", { title: "orphan", author: 999 });
  await step("err_length", "POST", "/items/books", { title: "long isbn", isbn: "x".repeat(40) });
  await step("err_type", "POST", "/items/books", { title: "bad pages", pages: "many" });
  await step("delete_author", "DELETE", "/items/authors/3", undefined);
  await q("after_delete", "/items/books?filter[author][_null]=true&fields=title&sort=title");
  await step("field:drop", "DELETE", "/fields/books/price", undefined);
  await step("snapshot:after", "GET", "/schema/snapshot", undefined, (j) => data(j) && { collections: data(j).collections, fields: data(j).fields, relations: data(j).relations });
  await step("activity", "GET", "/activity?aggregate[count]=*&groupBy[]=action&groupBy[]=collection&sort=collection,action");
  await step("coll:drop", "DELETE", "/collections/books", undefined);
  await step("collections:after", "GET", "/collections", undefined, (j) => (data(j) || []).map((c) => c.collection).sort());
  lst.end();
  resp.end();
}

main().catch((e) => { console.error(e); record("driver", String(e), false); lst.end(); resp.end(); });
