// The console's CQL helpers: statement splitting, quoting, completion, diff classification.

import assert from "node:assert/strict";
import { describe, test } from "node:test";
import type { NosqlSchema, SchemaTable } from "../src/lib/nosql-api";
import {
  applyCompletion,
  complete,
  diffKind,
  qualified,
  quoteIdent,
  referencedTables,
  selectAll,
  shellCommand,
  splitStatements,
  traceEventsQuery,
} from "../src/lib/nosql-cql";

describe("splitStatements", () => {
  test("top-level semicolons", () => {
    assert.deepEqual(splitStatements("USE app; SELECT * FROM t;\n\nINSERT INTO t (k) VALUES (1)"), [
      "USE app",
      "SELECT * FROM t",
      "INSERT INTO t (k) VALUES (1)",
    ]);
  });

  test("quotes, dollar strings and comments keep their semicolons", () => {
    const text = [
      "INSERT INTO t (k, v) VALUES (1, 'a;b''c');",
      'SELECT "we;ird" FROM t;',
      "CREATE FUNCTION f (x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE java AS $$ return x; $$;",
      "-- a comment; with a semicolon",
      "/* block; comment */ SELECT 1 FROM system.local;",
      "// only a comment;",
    ].join("\n");
    assert.deepEqual(splitStatements(text), [
      "INSERT INTO t (k, v) VALUES (1, 'a;b''c')",
      'SELECT "we;ird" FROM t',
      "CREATE FUNCTION f (x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE java AS $$ return x; $$",
      "-- a comment; with a semicolon\n/* block; comment */ SELECT 1 FROM system.local",
    ]);
  });

  test("batches stay whole", () => {
    const text = "BEGIN BATCH\n  INSERT INTO t (k) VALUES (1);\n  INSERT INTO t (k) VALUES (2);\nAPPLY BATCH;\nSELECT * FROM t;";
    assert.deepEqual(splitStatements(text), [
      "BEGIN BATCH\n  INSERT INTO t (k) VALUES (1);\nINSERT INTO t (k) VALUES (2);\nAPPLY BATCH",
      "SELECT * FROM t",
    ]);
    assert.deepEqual(splitStatements("begin unlogged batch insert into t (k) values (1); apply batch"), [
      "begin unlogged batch insert into t (k) values (1);\napply batch",
    ]);
  });

  test("empty input", () => {
    assert.deepEqual(splitStatements(""), []);
    assert.deepEqual(splitStatements(" ;; \n"), []);
  });
});

describe("quoting", () => {
  test("identifiers", () => {
    assert.equal(quoteIdent("users"), "users");
    assert.equal(quoteIdent("Users"), '"Users"');
    assert.equal(quoteIdent("select"), '"select"');
    assert.equal(quoteIdent('we"ird'), '"we""ird"');
    assert.equal(quoteIdent("1st"), '"1st"');
    assert.equal(qualified("app", "Events"), 'app."Events"');
    assert.equal(selectAll("app", "t"), "SELECT * FROM app.t");
  });

  test("trace query validates the session id", () => {
    assert.equal(
      traceEventsQuery("6b1c4c30-8e5a-11ef-9c1b-0242ac120002"),
      "SELECT activity, source, source_elapsed, thread FROM system_traces.events WHERE session_id = 6b1c4c30-8e5a-11ef-9c1b-0242ac120002",
    );
    assert.throws(() => traceEventsQuery("x; DROP KEYSPACE app"));
  });

  test("shell commands", () => {
    assert.deepEqual(shellCommand("TRACING ON"), { cmd: "TRACING", arg: "ON" });
    assert.deepEqual(shellCommand("  clear"), { cmd: "CLEAR", arg: "" });
    assert.deepEqual(shellCommand("serial  consistency SERIAL"), { cmd: "SERIAL CONSISTENCY", arg: "SERIAL" });
    assert.equal(shellCommand("SELECT * FROM t"), null);
    assert.equal(shellCommand("DESCRIBE KEYSPACES"), null, "DESCRIBE runs on the server");
  });

  test("diff kinds", () => {
    assert.equal(diffKind("CREATE TABLE app.t (k int PRIMARY KEY)"), "create");
    assert.equal(diffKind("-- new\nALTER TABLE app.t ADD v text"), "alter");
    assert.equal(diffKind("drop index app.i"), "drop");
    assert.equal(diffKind("GRANT SELECT ON app.t TO r"), "other");
  });
});

function table(name: string, cols: [string, string][]): SchemaTable {
  return {
    name,
    id: "00000000-0000-0000-0000-000000000000",
    columns: cols.map(([n, t], i) => ({ name: n, type: t, kind: i === 0 ? "partition_key" : "regular", position: 0, clustering_order: null })),
    partition_key: [cols[0][0]],
    clustering_key: [],
    indexes: [],
    options: { comment: "", compaction: {}, compression: {}, default_time_to_live: 0, gc_grace_seconds: 864000 },
  };
}

const schema: NosqlSchema = {
  dialect: "scylla",
  version: "v",
  describe: null,
  keyspaces: [
    {
      name: "system",
      system: true,
      durable_writes: true,
      replication: {},
      tablets: null,
      tables: [table("local", [["key", "text"]])],
      views: [],
      types: [],
      functions: [],
      aggregates: [],
    },
    {
      name: "app",
      system: false,
      durable_writes: true,
      replication: {},
      tablets: null,
      tables: [
        table("users", [
          ["id", "uuid"],
          ["email", "text"],
          ["Name", "text"],
        ]),
        table("user_events", [
          ["user_id", "uuid"],
          ["at", "timestamp"],
        ]),
      ],
      views: [{ ...table("users_by_email", [["email", "text"]]), base_table: "users", where_clause: "email IS NOT NULL" }],
      types: [{ name: "address", fields: [] }],
      functions: [],
      aggregates: [],
    },
  ],
};

const at = (text: string, ks: string | null = "app") => {
  const pos = text.indexOf("|");
  const clean = text.replace("|", "");
  return { clean, r: complete(clean, pos, schema, ks) };
};
const labels = (text: string, ks: string | null = "app") => at(text, ks).r.items.map((i) => i.label);

describe("completion", () => {
  test("keywords keep the typed case", () => {
    assert.ok(labels("SEL|").includes("SELECT"));
    const r = at("sel|").r;
    assert.equal(r.items.find((i) => i.label === "SELECT")?.insert, "select");
    assert.deepEqual(labels("|"), [], "nothing on an empty prefix outside a context");
  });

  test("tables of the session keyspace and keyspaces after FROM", () => {
    const l = labels("SELECT * FROM us|");
    assert.deepEqual(l, ["users", "user_events", "users_by_email"]);
    const all = labels("SELECT * FROM |");
    assert.ok(all.includes("app.") && all.includes("system."), all.join());
    // Without a session keyspace only keyspaces.
    assert.deepEqual(labels("SELECT * FROM |", null), ["app.", "system."]);
  });

  test("ks. prefix lists that keyspace's tables, views and types", () => {
    const { clean, r } = at("SELECT * FROM app.u|");
    assert.deepEqual(
      r.items.map((i) => i.label),
      ["users", "user_events", "users_by_email"],
    );
    const applied = applyCompletion(clean, r, r.items[0]);
    assert.deepEqual(applied, { text: "SELECT * FROM app.users", pos: "SELECT * FROM app.users".length });
    assert.deepEqual(labels("DESCRIBE TYPE app.a|"), ["address"]);
  });

  test("keyspaces after USE", () => {
    assert.deepEqual(labels("USE a|"), ["app"]);
    assert.deepEqual(labels("USE |"), ["app", "system"]);
  });

  test("columns of the tables the statement names, even after the cursor", () => {
    assert.deepEqual(labels("SELECT | FROM app.users"), ["id", "email", "Name"]);
    assert.ok(labels("SELECT * FROM users WHERE e|").includes("email"));
    const r = at("UPDATE users SET N|").r;
    assert.equal(r.items.find((i) => i.label === "Name")?.insert, '"Name"', "quoted when needed");
  });

  test("column types while defining tables", () => {
    assert.ok(labels("CREATE TABLE t (k ti|").includes("timeuuid"));
    assert.ok(labels("ALTER TABLE users ADD x big|").includes("bigint"));
    assert.ok(!labels("SELECT * FROM users WHERE ti|").includes("timeuuid"));
  });

  test("nothing inside strings and comments", () => {
    assert.deepEqual(labels("SELECT * FROM users WHERE email = 'us|"), []);
    assert.deepEqual(labels("-- SEL|"), []);
  });

  test("the word after the cursor is replaced", () => {
    const { clean, r } = at("SELECT * FROM us|ers");
    assert.equal(r.to, clean.length);
    assert.equal(applyCompletion(clean, r, r.items.find((i) => i.label === "user_events")!).text, "SELECT * FROM user_events");
  });

  test("referenced tables", () => {
    assert.deepEqual(referencedTables('SELECT * FROM app."Events" WHERE x = 1', null), [{ ks: "app", table: "Events" }]);
    assert.deepEqual(referencedTables("INSERT INTO t (k) VALUES (1)", "app"), [{ ks: "app", table: "t" }]);
    assert.deepEqual(referencedTables("INSERT INTO t (k) VALUES (1)", null), []);
  });
});
