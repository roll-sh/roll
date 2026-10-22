"use client";

import { HorizonMark } from "@roll/brand";
import { ChevronDown, ChevronRight, Eye, FileCode, Layers, ListTree, RefreshCw, Rows3, Shapes, SquareTerminal, Table2 } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { formatBytes } from "@/lib/roll-api";
import { formatCount } from "@/lib/redis-api";
import { isCqlError, nosqlApi, type NosqlDb, type SchemaKeyspace, type SchemaTable, type SchemaView } from "@/lib/nosql-api";
import { qualified, quoteIdent, selectAll } from "@/lib/nosql-cql";
import { btn, CopyButton, EmptyPanel, Panel, Stats, useCopy } from "../ui";
import type { SchemaState } from "./database";
import { Dash } from "./shared";

type Sel =
  | { kind: "schema" }
  | { kind: "keyspace"; ks: string }
  | { kind: "table" | "view" | "type" | "index"; ks: string; name: string };

const same = (a: Sel, b: Sel) => JSON.stringify(a) === JSON.stringify(b);

export function SchemaTab({
  db,
  schema: st,
  onBrowse,
  onQuery,
}: {
  db: NosqlDb;
  schema: SchemaState;
  onBrowse: (ks: string, table: string) => void;
  onQuery: (text: string) => void;
}) {
  const { schema, error, loading, reload } = st;
  const [sel, setSel] = useState<Sel | null>(null);
  const [showSystem, setShowSystem] = useState(false);

  const user = schema?.keyspaces.filter((k) => !k.system) ?? [];
  const system = schema?.keyspaces.filter((k) => k.system) ?? [];
  const current: Sel = sel ?? (user[0]?.tables[0] ? { kind: "table", ks: user[0].name, name: user[0].tables[0].name } : { kind: "schema" });

  if (!schema) {
    return error ? (
      <Panel title="Schema">
        <div className="space-y-3 px-5 py-4">
          <p className="text-[13px] text-danger">{error}</p>
          <button onClick={reload} className={btn}>
            <RefreshCw className="size-3.5" /> Retry
          </button>
        </div>
      </Panel>
    ) : (
      <p className="text-[13px] text-subtle">
        <span className="shimmer">{db.state === "running" ? "Reading the schema…" : "Waking the database to read its schema…"}</span>
      </p>
    );
  }

  return (
    <div className="grid gap-4 lg:grid-cols-[18rem_minmax(0,1fr)]">
      <Panel
        title="Keyspaces"
        aside={
          <button onClick={reload} disabled={loading} className="p-1 text-subtle hover:text-fg" title="Reload schema" aria-label="Reload schema">
            {loading ? <HorizonMark state="thinking" size={14} interactive={false} /> : <RefreshCw className="size-3.5" />}
          </button>
        }
        className="self-start"
      >
        <nav className="max-h-[70vh] overflow-y-auto py-2 text-[13px]" aria-label="Schema">
          <TreeItem icon={FileCode} label="Whole schema" active={same(current, { kind: "schema" })} onClick={() => setSel({ kind: "schema" })} depth={0} />
          {user.length === 0 && <p className="px-5 py-2 text-[12px] text-subtle">No keyspaces yet. Create one in the console.</p>}
          {user.map((k) => (
            <KeyspaceNode key={k.name} ks={k} sel={current} onSelect={setSel} defaultOpen={user.length <= 3} />
          ))}
          <button
            onClick={() => setShowSystem((s) => !s)}
            className="mt-2 flex w-full items-center gap-1.5 px-4 py-1.5 text-left text-[11px] text-subtle hover:text-fg"
            aria-expanded={showSystem}
          >
            {showSystem ? <ChevronDown className="size-3" /> : <ChevronRight className="size-3" />}
            System keyspaces <span className="font-mono">{system.length}</span>
          </button>
          {showSystem && system.map((k) => <KeyspaceNode key={k.name} ks={k} sel={current} onSelect={setSel} defaultOpen={false} />)}
        </nav>
      </Panel>
      <Detail
        key={`${JSON.stringify(current)}@${schema.version}`}
        db={db}
        sel={current}
        keyspaces={schema.keyspaces}
        fullDescribe={schema.describe}
        onBrowse={onBrowse}
        onQuery={onQuery}
      />
    </div>
  );
}

function TreeItem({
  icon: Icon,
  label,
  active,
  onClick,
  depth,
  meta,
}: {
  icon: typeof Table2;
  label: string;
  active: boolean;
  onClick: () => void;
  depth: number;
  meta?: string;
}) {
  return (
    <button
      onClick={onClick}
      aria-current={active ? "true" : undefined}
      className={`flex w-full min-w-0 items-center gap-2 py-1.5 pr-4 text-left transition ${
        active ? "bg-panel-2 text-fg" : "text-muted hover:bg-panel-2/60 hover:text-fg"
      }`}
      style={{ paddingLeft: `${16 + depth * 14}px` }}
    >
      <Icon className="size-3.5 shrink-0 text-subtle" />
      <span className="min-w-0 flex-1 truncate font-mono text-[12px]">{label}</span>
      {meta && <span className="shrink-0 font-mono text-[10px] text-subtle">{meta}</span>}
    </button>
  );
}

function KeyspaceNode({ ks, sel, onSelect, defaultOpen }: { ks: SchemaKeyspace; sel: Sel; onSelect: (s: Sel) => void; defaultOpen: boolean }) {
  const [open, setOpen] = useState(defaultOpen || ("ks" in sel && sel.ks === ks.name));
  const indexes = ks.tables.flatMap((t) => t.indexes.map((i) => ({ ...i, table: t.name })));
  const n = ks.tables.length + ks.views.length;
  return (
    <div>
      <div className="flex items-center">
        <button
          onClick={() => setOpen((o) => !o)}
          className="py-1.5 pl-3 text-subtle hover:text-fg"
          aria-label={open ? `Collapse ${ks.name}` : `Expand ${ks.name}`}
          aria-expanded={open}
        >
          {open ? <ChevronDown className="size-3" /> : <ChevronRight className="size-3" />}
        </button>
        <div className="min-w-0 flex-1">
          <TreeItem
            icon={Layers}
            label={ks.name}
            meta={String(n)}
            active={same(sel, { kind: "keyspace", ks: ks.name })}
            onClick={() => {
              setOpen(true);
              onSelect({ kind: "keyspace", ks: ks.name });
            }}
            depth={-0.5}
          />
        </div>
      </div>
      {open && (
        <div className="pb-1">
          {ks.tables.map((t) => (
            <TreeItem
              key={`t-${t.name}`}
              icon={Table2}
              label={t.name}
              meta={t.stats ? formatCount(t.stats.partitions) : undefined}
              active={same(sel, { kind: "table", ks: ks.name, name: t.name })}
              onClick={() => onSelect({ kind: "table", ks: ks.name, name: t.name })}
              depth={1.5}
            />
          ))}
          {ks.views.map((v) => (
            <TreeItem
              key={`v-${v.name}`}
              icon={Eye}
              label={v.name}
              meta="view"
              active={same(sel, { kind: "view", ks: ks.name, name: v.name })}
              onClick={() => onSelect({ kind: "view", ks: ks.name, name: v.name })}
              depth={1.5}
            />
          ))}
          {ks.types.map((u) => (
            <TreeItem
              key={`u-${u.name}`}
              icon={Shapes}
              label={u.name}
              meta="type"
              active={same(sel, { kind: "type", ks: ks.name, name: u.name })}
              onClick={() => onSelect({ kind: "type", ks: ks.name, name: u.name })}
              depth={1.5}
            />
          ))}
          {indexes.map((i) => (
            <TreeItem
              key={`i-${i.name}`}
              icon={ListTree}
              label={i.name}
              meta="index"
              active={same(sel, { kind: "index", ks: ks.name, name: i.name })}
              onClick={() => onSelect({ kind: "index", ks: ks.name, name: i.name })}
              depth={1.5}
            />
          ))}
        </div>
      )}
    </div>
  );
}

function describeQuery(sel: Sel) {
  switch (sel.kind) {
    case "schema":
      return null;
    case "keyspace":
      return `DESCRIBE KEYSPACE ${quoteIdent(sel.ks)}`;
    case "table":
      return `DESCRIBE TABLE ${qualified(sel.ks, sel.name)}`;
    case "view":
      return `DESCRIBE MATERIALIZED VIEW ${qualified(sel.ks, sel.name)}`;
    case "type":
      return `DESCRIBE TYPE ${qualified(sel.ks, sel.name)}`;
    case "index":
      return `DESCRIBE INDEX ${qualified(sel.ks, sel.name)}`;
  }
}

/** DESCRIBE output of one object, through the console route (create statements joined). */
function useDescribe(db: NosqlDb, sel: Sel, full: string | null) {
  const q = describeQuery(sel);
  const [text, setText] = useState<string | null>(q ? null : full);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    if (!q) return;
    let alive = true;
    nosqlApi.cql(db.id, { query: q }).then(
      (r) => {
        if (!alive) return;
        if (isCqlError(r)) setError(r.error);
        else if (r.kind === "rows") {
          const i = r.columns.findIndex((c) => c.name === "create_statement");
          setText(r.rows.map((row) => row[i >= 0 ? i : row.length - 1] ?? "").join("\n\n"));
        } else setText("");
      },
      (e) => alive && setError(e instanceof Error ? e.message : String(e)),
    );
    return () => {
      alive = false;
    };
  }, [db.id, q]);
  return { text, error, query: q ?? "DESCRIBE SCHEMA" };
}

function Detail({
  db,
  sel,
  keyspaces,
  fullDescribe,
  onBrowse,
  onQuery,
}: {
  db: NosqlDb;
  sel: Sel;
  keyspaces: SchemaKeyspace[];
  fullDescribe: string | null;
  onBrowse: (ks: string, table: string) => void;
  onQuery: (text: string) => void;
}) {
  const desc = useDescribe(db, sel, fullDescribe);
  const ks = "ks" in sel ? keyspaces.find((k) => k.name === sel.ks) : undefined;
  const table = useMemo<SchemaTable | SchemaView | undefined>(() => {
    if (sel.kind === "table") return ks?.tables.find((t) => t.name === sel.name);
    if (sel.kind === "view") return ks?.views.find((v) => v.name === sel.name);
    return undefined;
  }, [ks, sel]);

  if ("ks" in sel && !ks) {
    return (
      <EmptyPanel>
        <p className="text-[13px] text-muted">{sel.ks} no longer exists. Reload the schema.</p>
      </EmptyPanel>
    );
  }

  return (
    <div className="min-w-0 space-y-4">
      {sel.kind === "keyspace" && ks && <KeyspaceInfo ks={ks} />}
      {table && "name" in sel && (
        <>
          <div className="flex flex-wrap items-center justify-between gap-3">
            <h2 className="font-mono text-[15px] font-medium">
              {sel.ks}.{table.name}
            </h2>
            <div className="flex gap-2">
              <button onClick={() => onBrowse(sel.ks, table.name)} className={btn}>
                <Rows3 className="size-3.5" /> Browse data
              </button>
              <button onClick={() => onQuery(`${selectAll(sel.ks, table.name)} LIMIT 100;`)} className={btn}>
                <SquareTerminal className="size-3.5" /> Query
              </button>
            </div>
          </div>
          {table.stats && <TableStatsTiles s={table.stats} />}
          <ColumnsPanel t={table} />
        </>
      )}
      {sel.kind === "type" && ks && <TypeInfo ks={ks} name={sel.name} />}
      {sel.kind === "index" && ks && <IndexInfo ks={ks} name={sel.name} />}
      <DescribePanel title={desc.query} text={desc.text} error={desc.error} />
    </div>
  );
}

function KeyspaceInfo({ ks }: { ks: SchemaKeyspace }) {
  const repl = Object.entries(ks.replication);
  return (
    <Panel title={<span className="font-mono">{ks.name}</span>} aside={ks.system && <span className="text-[12px] text-subtle">system</span>}>
      <dl className="grid gap-x-6 gap-y-3 px-5 py-4 sm:grid-cols-2">
        <div>
          <dt className="text-[12px] text-subtle">Replication</dt>
          <dd className="mt-1 space-y-0.5 font-mono text-[12px]">
            {repl.length ? repl.map(([k, v]) => <p key={k}>{`${k}: ${v}`}</p>) : <Dash />}
          </dd>
        </div>
        <div className="space-y-3">
          <div>
            <dt className="text-[12px] text-subtle">Contents</dt>
            <dd className="mt-1 font-mono text-[12px]">
              {ks.tables.length} tables · {ks.views.length} views · {ks.types.length} types · {ks.functions.length + ks.aggregates.length}{" "}
              functions
            </dd>
          </div>
          <div>
            <dt className="text-[12px] text-subtle">Durable writes{ks.tablets != null ? " · tablets" : ""}</dt>
            <dd className="mt-1 font-mono text-[12px]">
              {String(ks.durable_writes)}
              {ks.tablets != null && ` · ${ks.tablets}`}
            </dd>
          </div>
        </div>
      </dl>
    </Panel>
  );
}

function TableStatsTiles({ s }: { s: NonNullable<SchemaTable["stats"]> }) {
  return (
    <Stats
      items={[
        { label: "Partitions", value: formatCount(s.partitions), hint: "estimate" },
        { label: "Size", value: formatBytes(s.total_bytes), hint: `${formatBytes(s.memtable_bytes)} in memtables` },
        { label: "SSTables", value: s.sstable_count, hint: `${formatCount(s.tombstones)} tombstones` },
        { label: "Reads / writes", value: `${formatCount(s.reads)} / ${formatCount(s.writes)}`, hint: "since loaded" },
      ]}
    />
  );
}

const KIND_LABEL: Record<string, string> = {
  partition_key: "partition key",
  clustering: "clustering",
  static: "static",
  regular: "",
};

function ColumnsPanel({ t }: { t: SchemaTable | SchemaView }) {
  const order = (k: string) => ({ partition_key: 0, clustering: 1, static: 2 })[k] ?? 3;
  const cols = [...t.columns].sort((a, b) => order(a.kind) - order(b.kind) || a.position - b.position);
  const pk = t.partition_key.length > 1 ? `(${t.partition_key.join(", ")})` : t.partition_key[0];
  return (
    <Panel
      title="Columns"
      aside={
        <span className="truncate font-mono text-[11px] text-subtle">
          PRIMARY KEY ({[pk, ...t.clustering_key].join(", ")})
        </span>
      }
    >
      <div className="overflow-x-auto">
        <table className="w-full text-[13px]">
          <thead>
            <tr className="border-b border-line text-left text-[11px] text-subtle">
              <th className="px-5 py-2 font-normal">Column</th>
              <th className="px-3 py-2 font-normal">Type</th>
              <th className="px-3 py-2 font-normal">Key</th>
              <th className="px-5 py-2 font-normal">Order</th>
            </tr>
          </thead>
          <tbody className="font-mono text-[12px]">
            {cols.map((c) => (
              <tr key={c.name} className="border-b border-line last:border-0">
                <td className="px-5 py-2">{c.name}</td>
                <td className="px-3 py-2 text-muted">{c.type}</td>
                <td className="px-3 py-2 text-[11px] text-subtle">{KIND_LABEL[c.kind] ?? c.kind}</td>
                <td className="px-5 py-2 text-[11px] text-subtle">{c.kind === "clustering" ? (c.clustering_order ?? "asc") : ""}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <div className="grid gap-x-6 gap-y-2 border-t border-line px-5 py-3 text-[11px] text-subtle sm:grid-cols-2">
        {"base_table" in t && (
          <p className="sm:col-span-2">
            View of <span className="font-mono text-muted">{t.base_table}</span> where{" "}
            <span className="font-mono text-muted">{t.where_clause}</span>
          </p>
        )}
        <p>
          TTL <span className="font-mono text-muted">{t.options.default_time_to_live}s</span> · gc_grace{" "}
          <span className="font-mono text-muted">{t.options.gc_grace_seconds}s</span>
        </p>
        <p className="truncate">
          compaction <span className="font-mono text-muted">{t.options.compaction.class ?? "—"}</span>
        </p>
        {t.indexes.length > 0 && (
          <p className="sm:col-span-2">
            Indexes: <span className="font-mono text-muted">{t.indexes.map((i) => `${i.name} (${i.options.target ?? i.kind})`).join(", ")}</span>
          </p>
        )}
        {t.options.comment && <p className="sm:col-span-2">“{t.options.comment}”</p>}
      </div>
    </Panel>
  );
}

function TypeInfo({ ks, name }: { ks: SchemaKeyspace; name: string }) {
  const u = ks.types.find((x) => x.name === name);
  return (
    <Panel title={<span className="font-mono">{`${ks.name}.${name}`}</span>} aside={<span className="text-[12px] text-subtle">user-defined type</span>}>
      <ul className="px-5 py-3 font-mono text-[12px]">
        {(u?.fields ?? []).map((f) => (
          <li key={f.name} className="flex gap-4 py-1">
            <span className="w-40 truncate">{f.name}</span>
            <span className="text-muted">{f.type}</span>
          </li>
        ))}
      </ul>
    </Panel>
  );
}

function IndexInfo({ ks, name }: { ks: SchemaKeyspace; name: string }) {
  const t = ks.tables.find((x) => x.indexes.some((i) => i.name === name));
  const i = t?.indexes.find((x) => x.name === name);
  return (
    <Panel title={<span className="font-mono">{`${ks.name}.${name}`}</span>} aside={<span className="text-[12px] text-subtle">{i?.kind ?? "index"}</span>}>
      <dl className="space-y-1 px-5 py-3 font-mono text-[12px]">
        <p>
          <span className="text-subtle">on </span>
          {t?.name}
        </p>
        {Object.entries(i?.options ?? {}).map(([k, v]) => (
          <p key={k}>
            <span className="text-subtle">{k} </span>
            {v}
          </p>
        ))}
      </dl>
    </Panel>
  );
}

function DescribePanel({ title, text, error }: { title: string; text: string | null; error: string | null }) {
  const { copied, copy } = useCopy();
  return (
    <Panel
      title={<span className="font-mono text-[13px]">{title}</span>}
      aside={text ? <CopyButton id="describe" text={text} copied={copied} onCopy={copy} /> : null}
    >
      {error ? (
        <p className="px-5 py-4 text-[13px] text-danger">{error}</p>
      ) : text == null ? (
        <p className="px-5 py-4 text-[13px] text-subtle">
          <span className="shimmer">Describing…</span>
        </p>
      ) : (
        <pre className="max-h-[32rem] overflow-auto px-5 py-4 font-mono text-[12px] leading-relaxed text-muted">
          {text || "-- nothing to describe"}
        </pre>
      )}
    </Panel>
  );
}
