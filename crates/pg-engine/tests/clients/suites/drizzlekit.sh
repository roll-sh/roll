# drizzle-kit 0.31.11 with drizzle-orm 0.45.4 and node-postgres 8.16.3 (TypeScript): its migration
# and introspection flows on a schema with enums, arrays, jsonb, identity and generated columns,
# partial and expression indexes, checks, foreign keys, a view and a second schema:
#   push::*      `push` into a263_dk_push, then `push` again (nothing to change);
#   migrate::*   `generate` (offline), `migrate` into a263_dk_migrate, `check`, a second
#                schema version generated and migrated (ALTERs), `generate` again (no changes);
#   pull::*      `pull` of a263_dk_migrate and of drizzlekit.setup.sql's hand-written
#                a263_dk_intro (partitioned tables, inheritance, more index kinds): one id per
#                generated declaration (`export const` / relation), with a hash of its text,
#                so a declaration introspected differently shows as one id per server.
# image: node:22-bookworm-slim
set -u
export npm_config_cache=/cache/home/npm
N=/cache/drizzlekit-node
if [ ! -x $N/node_modules/.bin/drizzle-kit ]; then
  mkdir -p $N && cd $N
  [ -f package.json ] || echo '{"private": true}' > package.json
  npm install -s --no-audit --no-fund drizzle-kit@0.31.11 drizzle-orm@0.45.4 pg@8.16.3 > /dev/null
fi
W=/cache/work/drizzlekit-$TARGET
rm -rf $W && mkdir -p $W/src && cd $W
ln -s $N/node_modules node_modules
DK=$N/node_modules/.bin/drizzle-kit
U="postgres://postgres:pw@$PGHOST:$PGPORT"
L=$OUT/checks.lst
: > $L
# check <id> <command...>: PASS when it exits 0 and prints no error.
check() {
  id=$1; shift
  if "$@" > "$W/$id.log" 2>&1 && ! grep -qiE 'error|exception' "$W/$id.log"; then echo "PASS $id" >> $L
  else echo "FAIL $id" >> $L; sed "s/^/$id: /" "$W/$id.log" | tail -30 >> "$OUT/errors.txt"; fi
}
config() {  # config <file> <database> <schema file> <out dir>
  cat > "$1" <<EOF
import { defineConfig } from "drizzle-kit";
export default defineConfig({
  dialect: "postgresql",
  schema: "$3",
  out: "$4",
  dbCredentials: { url: "$U/$2" },
  schemaFilter: ["public", "shop"],
  strict: false,
  verbose: false,
});
EOF
}
schema() {  # schema <version 1|2>
  cat > src/schema.ts <<'EOF'
import { sql } from "drizzle-orm";
import {
  pgTable, pgSchema, pgEnum, pgView, serial, integer, bigint, text, varchar, boolean, jsonb, json,
  timestamp, date, numeric, real, doublePrecision, uuid, smallint, inet, interval, char, index,
  uniqueIndex, primaryKey, foreignKey, check, unique,
} from "drizzle-orm/pg-core";
export const shop = pgSchema("shop");
export const mood = pgEnum("mood", ["sad", "ok", "happy"]);
export const tier = shop.enum("tier", ["free", "pro"]);
export const users = pgTable("users", {
  id: integer("id").primaryKey().generatedAlwaysAsIdentity({ startWith: 100 }),
  email: varchar("email", { length: 200 }).notNull(),
  name: text("name").notNull().default("anon"),
  mood: mood("mood").default("ok").notNull(),
  tags: text("tags").array().notNull().default(sql`'{}'::text[]`),
  scores: integer("scores").array(),
  prefs: jsonb("prefs").$type<Record<string, unknown>>().default({}),
  raw: json("raw"),
  balance: numeric("balance", { precision: 12, scale: 2 }).default("0"),
  ratio: doublePrecision("ratio"),
  small: smallint("small").default(1),
  active: boolean("active").default(true),
  uid: uuid("uid").defaultRandom().notNull(),
  ip: inet("ip"),
  born: date("born"),
  wait: interval("wait"),
  code: char("code", { length: 3 }),
  nameLen: integer("name_len").generatedAlwaysAs(sql`length(name)`),
  createdAt: timestamp("created_at", { withTimezone: true, precision: 3 }).defaultNow().notNull(),
}, (t) => [
  uniqueIndex("users_email_lower").on(sql`lower(${t.email})`),
  index("users_active_born").on(t.born).where(sql`${t.active}`),
  index("users_prefs_gin").using("gin", t.prefs),
  index("users_name_desc").on(t.name.desc().nullsLast(), t.id),
  check("users_small_pos", sql`${t.small} > 0`),
]);
export const plans = shop.table("plans", {
  id: serial("id").primaryKey(),
  tier: tier("tier").notNull(),
  price: real("price"),
  owner: integer("owner").references(() => users.id, { onDelete: "cascade", onUpdate: "restrict" }),
}, (t) => [unique("plans_tier_owner").on(t.tier, t.owner)]);
export const memberships = pgTable("memberships", {
  userId: integer("user_id").notNull(),
  planId: integer("plan_id").notNull(),
  since: timestamp("since").defaultNow(),
}, (t) => [
  primaryKey({ columns: [t.userId, t.planId] }),
  foreignKey({ columns: [t.userId], foreignColumns: [users.id], name: "memberships_user_fk" }).onDelete("set null"),
  foreignKey({ columns: [t.planId], foreignColumns: [plans.id] }),
]);
export const activeUsers = pgView("active_users").as((qb) =>
  qb.select({ id: users.id, email: users.email }).from(users).where(sql`${users.active}`));
EOF
  if [ "$1" = 2 ]; then
    # Version 2: a new column, a changed default, a dropped index, a new table with a bigint
    # identity and a self reference, an enum value added.
    sed -i 's/\["sad", "ok", "happy"\]/["sad", "ok", "happy", "ecstatic"]/;
      s/default("anon")/default("nobody")/;
      /users_name_desc/d;
      s/^  code: char("code", { length: 3 }),/  code: char("code", { length: 3 }),\n  nick: varchar("nick", { length: 40 }),/' src/schema.ts
    cat >> src/schema.ts <<'EOF'
export const nodes = pgTable("nodes", {
  id: bigint("id", { mode: "number" }).primaryKey().generatedByDefaultAsIdentity(),
  parent: bigint("parent", { mode: "number" }),
  label: text("label"),
}, (t) => [foreignKey({ columns: [t.parent], foreignColumns: [t.id] })]);
EOF
  fi
}
# ids for a pulled schema.ts / relations.ts: one per top-level declaration, with a hash of its text.
pulled() {  # pulled <prefix> <dir>
  for f in "$2"/schema.ts "$2"/relations.ts; do
    [ -f "$f" ] || { echo "FAIL $1::$(basename "$f")" >> $L; continue; }
    node -e '
      const fs = require("fs"), crypto = require("crypto");
      const [p, file] = [process.argv[1], process.argv[2]];
      let name = null, body = [];
      const out = [];
      const flush = () => { if (name) out.push(`PASS ${p}::${name}#` + crypto.createHash("md5").update(body.join("\n")).digest("hex").slice(0, 10)); };
      for (const line of fs.readFileSync(file, "utf8").split("\n")) {
        const m = line.match(/^export const (\w+)/);
        if (m) { flush(); name = m[1]; body = []; }
        if (name) body.push(line);
      }
      flush();
      if (out.length) console.log(out.join("\n"));' "$1::$(basename "$f" .ts)" "$f" >> $L
  done
}
# redo <id> <push output>: a push over a database already pushed should change nothing; PASS when
# it says so, else FAIL and one id per statement it ran again (drizzle-kit itself re-runs some on
# PostgreSQL: the statements are what compares).
redo() {
  if grep -q 'No changes detected' "$2"; then echo "PASS $1" >> $L; else echo "FAIL $1" >> $L; fi
  grep -E '^\s*(ALTER|CREATE|DROP|COMMENT|DO) ' "$2" | sed 's/^ *//' | sort -u | while read -r stmt; do
    echo "PASS $1::$(printf '%s' "$stmt" | cut -c1-120 | tr ' ' '_')#$(printf '%s' "$stmt" | md5sum | cut -c1-8)" >> $L
  done
}
dropdb() { node -e '
  const { Client } = require("pg");
  (async () => { const c = new Client({ connectionString: process.argv[1] }); await c.connect();
    for (const d of process.argv.slice(2)) { await c.query(`DROP DATABASE IF EXISTS ${d} WITH (FORCE)`); await c.query(`CREATE DATABASE ${d}`); }
    await c.end(); })().catch((e) => { console.error(e.message); process.exit(1); });' "$U/postgres" "$@"; }

dropdb a263_dk_push a263_dk_migrate
schema 1
config push.config.ts a263_dk_push ./src/schema.ts ./drizzle-push
check push::first $DK push --config push.config.ts --force
$DK push --config push.config.ts --force --verbose > $W/push2.out 2>&1
redo push::again $W/push2.out
config mig.config.ts a263_dk_migrate ./src/schema.ts ./drizzle
check migrate::generate1 $DK generate --config mig.config.ts --name v1
check migrate::migrate1 $DK migrate --config mig.config.ts
check migrate::check $DK check --config mig.config.ts
schema 2
check migrate::generate2 $DK generate --config mig.config.ts --name v2
check migrate::migrate2 $DK migrate --config mig.config.ts
$DK generate --config mig.config.ts --name v3 > $W/gen3.out 2>&1
if grep -q 'No schema changes' $W/gen3.out; then echo "PASS migrate::nochange" >> $L
else echo "FAIL migrate::nochange" >> $L; fi
# push of version 2 over the version 1 database: ALTERs through introspection.
check push::alter $DK push --config push.config.ts --force
$DK push --config push.config.ts --force --verbose > $W/push3.out 2>&1
redo push::alter-again $W/push3.out
config pull1.config.ts a263_dk_migrate ./src/none.ts ./pull-migrate
check pull::migrate $DK pull --config pull1.config.ts
pulled pull::migrate ./pull-migrate
config pull2.config.ts a263_dk_intro ./src/none.ts ./pull-intro
sed -i 's/schemaFilter: \["public", "shop"\]/schemaFilter: ["public", "other"]/' pull2.config.ts
check pull::intro $DK pull --config pull2.config.ts
pulled pull::intro ./pull-intro
grep -c '^PASS' $L; grep '^FAIL' $L
