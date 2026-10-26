# Prisma 7.10.1 (TypeScript ORM): its CLI's migrate / introspection against a fixed schema, and
# its client (prisma-client generator, @prisma/adapter-pg) on that schema; Prisma's own engine
# test suites need its Rust workspace and are not run. Checks, one PASS/FAIL line each:
#   push / migrate::*  db push, migrate dev (shadow database a51_prisma_shadow), a second
#                      migration, status, diff against the database (must be empty), reset;
#   pull::*, intro::*  `db pull` of the pushed schema and of prisma.setup.sql's hand-written one
#                      (a51_prisma_intro): one id per model / enum / view, with a hash of its
#                      text, so a block introspected differently shows as engine=MISSING;
#   client::*          CRUD, relations, filters, aggregates, transactions, raw SQL.
# image: node:22-bookworm-slim
set -eu
export npm_config_cache=/cache/home/npm PRISMA_HIDE_UPDATE_MESSAGE=1 CHECKPOINT_DISABLE=1
N=/cache/prisma-node
if [ ! -x $N/node_modules/.bin/prisma ]; then
  mkdir -p $N && cd $N
  [ -f package.json ] || echo '{"private": true}' > package.json
  npm install -s --no-audit --no-fund prisma@7.10.1 @prisma/client@7.10.1 \
    @prisma/adapter-pg@7.10.1 tsx@4.20.6 typescript@5.9.3 > /dev/null
fi
W=/cache/work/prisma
rm -rf $W && mkdir -p $W/prisma && cd $W
ln -s $N/node_modules node_modules
PRISMA=$N/node_modules/.bin/prisma
U="postgresql://postgres:pw@$PGHOST:$PGPORT"
export DATABASE_URL="$U/a51_prisma" SHADOW_DATABASE_URL="$U/a51_prisma_shadow"
cat > prisma.config.ts <<'EOF'
import { defineConfig, env } from "prisma/config";
export default defineConfig({
  schema: "prisma/schema.prisma",
  migrations: { path: "prisma/migrations" },
  datasource: { url: env("DATABASE_URL"), shadowDatabaseUrl: env("SHADOW_DATABASE_URL") },
});
EOF
cat > prisma/schema.prisma <<'EOF'
generator client {
  provider = "prisma-client"
  output   = "../generated/prisma"
}

datasource db {
  provider = "postgresql"
}

enum Role {
  USER
  ADMIN
  @@map("role")
}

model User {
  id        Int       @id @default(autoincrement())
  email     String    @unique @db.VarChar(120)
  name      String?
  role      Role      @default(USER)
  balance   Decimal   @default(0) @db.Decimal(12, 2)
  big       BigInt    @default(0)
  ratio     Float?
  active    Boolean   @default(true)
  blob      Bytes?
  meta      Json?     @db.JsonB
  tags      String[]
  scores    Int[]
  uid       String    @default(dbgenerated("gen_random_uuid()")) @db.Uuid
  ip        String?   @db.Inet
  born      DateTime? @db.Date
  createdAt DateTime  @default(now()) @db.Timestamptz(6)
  updatedAt DateTime  @updatedAt
  posts     Post[]    @relation("author")
  edited    Post[]    @relation("editor")
  profile   Profile?
  @@index([name(sort: Desc)])
  @@index([meta], type: Gin)
  @@map("users")
}

model Profile {
  id     Int    @id @default(autoincrement())
  bio    String @default("")
  userId Int    @unique @map("user_id")
  user   User   @relation(fields: [userId], references: [id], onDelete: Cascade)
}

model Post {
  id        String     @id @default(uuid())
  title     String
  body      String?    @db.Text
  views     Int        @default(0)
  published Boolean    @default(false)
  authorId  Int
  editorId  Int?
  author    User       @relation("author", fields: [authorId], references: [id], onDelete: Cascade)
  editor    User?      @relation("editor", fields: [editorId], references: [id], onDelete: SetNull)
  tags      Tag[]
  category  Category?  @relation(fields: [categoryId], references: [id])
  categoryId Int?
  @@unique([authorId, title])
  @@index([published, views(sort: Desc)])
}

model Tag {
  name  String @id
  posts Post[]
}

model Category {
  id       Int        @id @default(autoincrement())
  name     String
  parentId Int?
  parent   Category?  @relation("tree", fields: [parentId], references: [id])
  children Category[] @relation("tree")
  posts    Post[]
}

model Pair {
  a     Int
  b     Int
  label String  @db.Char(4)
  at    DateTime? @db.Time(3)
  @@id([a, b])
  @@index([label], type: Hash)
}
EOF
L="$OUT/prisma.lst"
: > "$L"
# check <id> <command...>: PASS/FAIL <id>, output in $OUT/<id>.log
check() {
  id=$1; shift
  if "$@" > "$OUT/$(echo "$id" | tr '/: ' '___').log" 2>&1; then echo "PASS $id" >> "$L"
  else echo "FAIL $id" >> "$L"; echo "== $id"; tail -15 "$OUT/$(echo "$id" | tr '/: ' '___').log"; fi
}
# blocks <prefix> <file>: one PASS line per model / enum / view, keyed by a hash of its text
blocks() {
  d="$OUT/$1.blocks"
  rm -rf "$d" && mkdir -p "$d"
  awk -v d="$d" '/^(model|enum|view|type) / { f = d "/" $1 " " $2; inb = 1 }
    inb { print > f } inb && /^}/ { inb = 0; close(f) }' "$2"
  for f in "$d"/*; do
    [ -f "$f" ] && echo "PASS $1::$(basename "$f") $(sha1sum < "$f" | cut -c1-10)" >> "$L"
  done
}
check push $PRISMA db push --force-reset --accept-data-loss
check pull $PRISMA db pull --print
[ -s "$OUT/pull.log" ] && blocks pull "$OUT/pull.log"
check migrate::reset0 $PRISMA migrate reset --force
check migrate::dev_init $PRISMA migrate dev --name init
check migrate::status $PRISMA migrate status
check migrate::diff_empty $PRISMA migrate diff --from-config-datasource --to-schema prisma/schema.prisma --exit-code
cp prisma/schema.prisma "$OUT/schema1.prisma"
# a second migration: a column, an index, an enum value, a dropped index
sed -i -e 's/^  ADMIN$/  ADMIN\n  GUEST/' -e 's/^  views     Int        @default(0)$/  views     Int        @default(0)\n  rating    Float      @default(1.5) @db.Real/' \
  -e 's/^  @@index(\[meta\], type: Gin)$//' prisma/schema.prisma
check migrate::dev_second $PRISMA migrate dev --name second
check migrate::diff_empty2 $PRISMA migrate diff --from-config-datasource --to-schema prisma/schema.prisma --exit-code
check migrate::reset $PRISMA migrate reset --force
check migrate::status2 $PRISMA migrate status
cp -r prisma/migrations "$OUT/migrations"
check generate $PRISMA generate
cat > client.ts <<'EOF'
import { PrismaClient, Prisma } from "./generated/prisma/client";
import { PrismaPg } from "@prisma/adapter-pg";
const prisma = new PrismaClient({ adapter: new PrismaPg({ connectionString: process.env.DATABASE_URL! }) });
function eq(a: unknown, b: unknown) {
  const s = (x: unknown) => JSON.stringify(x, (_, v) => (typeof v === "bigint" ? v.toString() : v));
  if (s(a) !== s(b)) throw new Error(`expected ${s(b)}, got ${s(a)}`);
}
const tests: [string, () => Promise<void>][] = [
  ["create_nested", async () => {
    const u = await prisma.user.create({
      data: { email: "a@x.io", name: "Ann", tags: ["x", "y"], scores: [1, 2], meta: { k: [1, "two"] },
        balance: new Prisma.Decimal("12.34"), big: 9007199254740993n, blob: Buffer.from([0, 1, 255]),
        ip: "10.0.0.1", born: new Date("2000-02-29"),
        profile: { create: { bio: "hi" } },
        posts: { create: [{ title: "p1", tags: { create: [{ name: "t1" }, { name: "t2" }] } }, { title: "p2", published: true, views: 5 }] } },
      include: { profile: true, posts: { include: { tags: true }, orderBy: { title: "asc" } } } });
    eq(u.posts.length, 2); eq(u.posts[0].tags.length, 2); eq(u.profile!.bio, "hi");
    eq(u.big, 9007199254740993n); eq(u.balance.toString(), "12.34"); eq([...u.blob!], [0, 1, 255]);
    eq(u.born!.toISOString().slice(0, 10), "2000-02-29");
  }],
  ["find_unique", async () => {
    const u = await prisma.user.findUnique({ where: { email: "a@x.io" }, select: { name: true, tags: true, meta: true } });
    eq(u, { name: "Ann", tags: ["x", "y"], meta: { k: [1, "two"] } });
  }],
  ["filters", async () => {
    await prisma.user.createMany({ data: [{ email: "b@x.io", name: "bob", role: "ADMIN" }, { email: "c@x.io", name: null }] });
    eq(await prisma.user.count({ where: { name: { contains: "B", mode: "insensitive" } } }), 1);
    eq(await prisma.user.count({ where: { name: null } }), 1);
    eq(await prisma.user.count({ where: { OR: [{ role: "ADMIN" }, { tags: { has: "x" } }] } }), 2);
    eq(await prisma.user.count({ where: { scores: { hasEvery: [1, 2] } } }), 1);
    eq(await prisma.user.count({ where: { meta: { path: ["k"], array_contains: ["two"] } } }), 1);
    eq(await prisma.user.count({ where: { email: { startsWith: "b", endsWith: ".io" } } }), 1);
    eq(await prisma.user.count({ where: { email: { in: ["a@x.io", "zz"] } } }), 1);
  }],
  ["relation_filters", async () => {
    eq(await prisma.user.count({ where: { posts: { some: { published: true } } } }), 1);
    eq(await prisma.user.count({ where: { posts: { none: {} } } }), 2);
    eq(await prisma.post.count({ where: { tags: { every: { name: { startsWith: "t" } } } } }), 2);
    eq(await prisma.post.count({ where: { author: { is: { email: "a@x.io" } } } }), 2);
  }],
  ["order_paginate", async () => {
    const r = await prisma.user.findMany({ orderBy: [{ name: { sort: "asc", nulls: "last" } }], skip: 1, take: 2, select: { email: true } });
    eq(r.map((x) => x.email), ["b@x.io", "c@x.io"]);
    const c = await prisma.post.findMany({ cursor: { authorId_title: { authorId: 1, title: "p1" } }, take: 5, orderBy: { title: "asc" }, select: { title: true } });
    eq(c.map((x) => x.title), ["p1", "p2"]);
    const d = await prisma.user.findMany({ distinct: ["role"], orderBy: { role: "asc" }, select: { role: true } });
    eq(d.map((x) => x.role), ["USER", "ADMIN"]);
  }],
  ["update_upsert_delete", async () => {
    const u = await prisma.user.update({ where: { email: "a@x.io" }, data: { scores: { push: 3 }, big: { increment: 1 }, balance: { multiply: 2 } } });
    eq(u.scores, [1, 2, 3]); eq(u.big, 9007199254740994n); eq(u.balance.toString(), "24.68");
    const up = await prisma.user.upsert({ where: { email: "d@x.io" }, create: { email: "d@x.io" }, update: { name: "no" } });
    eq(up.name, null);
    const up2 = await prisma.user.upsert({ where: { email: "d@x.io" }, create: { email: "d@x.io" }, update: { name: "yes" } });
    eq(up2.name, "yes");
    eq((await prisma.post.updateMany({ where: { published: false }, data: { views: { increment: 10 } } })).count, 1);
    eq((await prisma.user.deleteMany({ where: { email: "d@x.io" } })).count, 1);
  }],
  ["cascade_setnull", async () => {
    const u = await prisma.user.create({ data: { email: "e@x.io", posts: { create: { title: "e1" } } } });
    const p = await prisma.post.update({ where: { authorId_title: { authorId: 1, title: "p2" } }, data: { editorId: u.id } });
    await prisma.user.delete({ where: { id: u.id } });
    eq(await prisma.post.count({ where: { authorId: u.id } }), 0);
    eq((await prisma.post.findUnique({ where: { id: p.id } }))!.editorId, null);
  }],
  ["aggregate_groupby", async () => {
    const a = await prisma.post.aggregate({ _sum: { views: true }, _avg: { views: true }, _max: { title: true }, _count: { _all: true } });
    eq([a._sum.views, a._avg.views, a._max.title, a._count._all], [15, 7.5, "p2", 2]);
    const g = await prisma.user.groupBy({ by: ["role"], _count: { _all: true }, orderBy: { role: "asc" }, having: { id: { _count: { gt: 0 } } } });
    eq(g.map((x) => [x.role, x._count._all]), [["USER", 2], ["ADMIN", 1]]);
    const rc = await prisma.user.findMany({ select: { email: true, _count: { select: { posts: true } } }, orderBy: { email: "asc" } });
    eq(rc.map((x) => x._count.posts), [2, 0, 0]);
  }],
  ["self_relation", async () => {
    const root = await prisma.category.create({ data: { name: "root", children: { create: [{ name: "a" }, { name: "b" }] } }, include: { children: true } });
    eq(root.children.length, 2);
    const c = await prisma.category.findFirst({ where: { name: "a" }, include: { parent: true } });
    eq(c!.parent!.name, "root");
  }],
  ["composite_id", async () => {
    await prisma.pair.create({ data: { a: 1, b: 2, label: "ab", at: new Date("1970-01-01T12:34:56.789Z") } });
    const p = await prisma.pair.findUnique({ where: { a_b: { a: 1, b: 2 } } });
    eq(p!.label, "ab  "); eq(p!.at!.toISOString(), "1970-01-01T12:34:56.789Z");
  }],
  ["transaction_batch", async () => {
    const [n, u] = await prisma.$transaction([prisma.user.count(), prisma.user.findFirst({ where: { email: "b@x.io" } })]);
    eq([n, u!.name], [3, "bob"]);
  }],
  ["transaction_interactive_rollback", async () => {
    try {
      await prisma.$transaction(async (tx) => {
        await tx.user.create({ data: { email: "f@x.io" } });
        await tx.user.create({ data: { email: "f@x.io" } });
      });
      throw new Error("no unique violation");
    } catch (e) {
      if (!(e instanceof Prisma.PrismaClientKnownRequestError) || e.code !== "P2002") throw e;
    }
    eq(await prisma.user.count({ where: { email: "f@x.io" } }), 0);
  }],
  ["transaction_isolation", async () => {
    await prisma.$transaction(async (tx) => { await tx.user.count(); }, { isolationLevel: "Serializable" });
  }],
  ["errors", async () => {
    try { await prisma.user.create({ data: { email: "a@x.io" } }); throw new Error("no error"); }
    catch (e) { if (!(e instanceof Prisma.PrismaClientKnownRequestError) || e.code !== "P2002") throw e; }
    try { await prisma.post.create({ data: { title: "x", authorId: 999 } }); throw new Error("no error"); }
    catch (e) { if (!(e instanceof Prisma.PrismaClientKnownRequestError) || e.code !== "P2003") throw e; }
    try { await prisma.user.update({ where: { email: "zz" }, data: { name: "q" } }); throw new Error("no error"); }
    catch (e) { if (!(e instanceof Prisma.PrismaClientKnownRequestError) || e.code !== "P2025") throw e; }
  }],
  ["raw", async () => {
    const r = await prisma.$queryRaw<{ n: bigint; s: string; d: Prisma.Decimal; j: unknown; a: number[] }[]>`SELECT count(*) AS n, ${"x"}::text AS s, 1.50::numeric AS d, '{"a":1}'::jsonb AS j, ARRAY[1,2] AS a FROM users WHERE id > ${0}`;
    eq([r[0].n, r[0].s, r[0].d.toString(), r[0].j, r[0].a], [3n, "x", "1.5", { a: 1 }, [1, 2]]);
    eq(await prisma.$executeRaw`UPDATE users SET ratio = ${0.5} WHERE email = ${"a@x.io"}`, 1);
    const u = await prisma.$queryRawUnsafe<{ uid: string; created: Date }[]>("SELECT uid, \"createdAt\" AS created FROM users WHERE email = $1", "a@x.io");
    if (!/^[0-9a-f-]{36}$/.test(u[0].uid) || !(u[0].created instanceof Date)) throw new Error(JSON.stringify(u));
  }],
  ["json_null", async () => {
    await prisma.user.update({ where: { email: "b@x.io" }, data: { meta: Prisma.JsonNull } });
    eq(await prisma.user.count({ where: { meta: { equals: Prisma.JsonNull } } }), 1);
    eq(await prisma.user.count({ where: { meta: { equals: Prisma.DbNull } } }), 1);
  }],
];
(async () => {
  for (const [name, fn] of tests) {
    try { await fn(); console.log(`PASS client::${name}`); }
    catch (e) { console.log(`FAIL client::${name}`); console.error(`== ${name}:`, e); }
  }
  await prisma.$disconnect();
})();
EOF
if [ -d generated ]; then
  $N/node_modules/.bin/tsx client.ts >> "$L" 2> "$OUT/client.err" || true
  grep -A12 '^== ' "$OUT/client.err" | head -150 || true
fi
export DATABASE_URL="$U/a51_prisma_intro"
check intro $PRISMA db pull --print
[ -s "$OUT/intro.log" ] && blocks intro "$OUT/intro.log"
grep -c '^PASS' "$L"; grep '^FAIL' "$L" || true
