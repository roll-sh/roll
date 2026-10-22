"use client";

import { Archive, KeyRound, Layers, Trash2 } from "lucide-react";
import { useState, type ReactNode } from "react";
import { formatBytes } from "@/lib/roll-api";
import { formatCount } from "@/lib/redis-api";
import {
  cqlUrl,
  DRIVER_LABEL,
  DRIVERS,
  EXPIRY_PRESETS,
  formatSpan,
  maskCqlUrl,
  maskSecret,
  nosqlApi,
  parseObject,
  settingsPatch,
  snippet,
  validName,
  type Driver,
  type NosqlDb,
  type NosqlInfo,
  type PatchNosqlReq,
} from "@/lib/nosql-api";
import { btn, Confirm, CopyButton, input, Panel, primary, SecretField, Stats, useCopy } from "../ui";
import {
  Dash,
  Expiry,
  Field,
  formatTime,
  fromLocalInput,
  now,
  Segmented,
  SettingsFields,
  settingsOf,
  toLocalInput,
  type NosqlSettings,
  type Run,
} from "./shared";

export function OverviewTab({
  db,
  info,
  busy,
  run,
  onDeleted,
}: {
  db: NosqlDb;
  info: NosqlInfo;
  busy: string | null;
  run: Run;
  onDeleted: () => void;
}) {
  const loaded = db.state === "running";
  const m = db.status.metrics;
  return (
    <div className="space-y-4">
      <Stats
        items={[
          {
            label: "State",
            value: loaded ? "loaded" : "scaled to zero",
            hint: loaded
              ? db.idle_timeout === 0
                ? "always on"
                : `${db.status.users ?? 0} in use · unloads ${formatSpan(db.idle_timeout)} after the last client`
              : "wakes on the first connection",
          },
          {
            label: "Stored",
            value: formatBytes(db.storage_bytes),
            hint: db.parent_id ? "this branch's own SSTables" : "SSTables on object storage",
          },
          { label: "Clients", value: formatCount(db.connections), hint: `${formatCount(m.connections_total)} connections since start` },
          {
            label: "Requests",
            value: formatCount(m.requests),
            hint: m.errors ? `${formatCount(m.errors)} answered with an error` : `${formatBytes(m.bytes_out)} out`,
          },
        ]}
      />
      <div className="grid gap-4 lg:grid-cols-5">
        <Endpoints db={db} info={info} />
        <Snippets db={db} />
      </div>
      <SettingsPanel key={`${db.id}-${db.durability}-${db.idle_timeout}-${db.history_retention}-${db.read_only}-${db.expires_at}-${JSON.stringify(db.settings)}-${JSON.stringify(db.masking)}`} db={db} info={info} busy={busy} run={run} />
      <div className="grid gap-4 lg:grid-cols-3">
        <Maintenance db={db} busy={busy} run={run} />
        <Credentials db={db} busy={busy} run={run} />
        <DeletePanel db={db} busy={busy} run={run} onDeleted={onDeleted} />
      </div>
    </div>
  );
}

function Row({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="grid grid-cols-[7.5rem_minmax(0,1fr)] items-baseline gap-3 py-1.5">
      <dt className="text-[12px] text-subtle">{label}</dt>
      <dd className="min-w-0 truncate font-mono text-[12px]">{children}</dd>
    </div>
  );
}

function Endpoints({ db, info }: { db: NosqlDb; info: NosqlInfo }) {
  const { copied, copy } = useCopy();
  const url = cqlUrl(db);
  const ep = db.endpoints;
  const value = (key: string, v: string | null) =>
    v ? (
      <span className="flex items-center gap-1">
        <span className="truncate">{v}</span>
        <CopyButton id={key} text={v} copied={copied} onCopy={copy} />
      </span>
    ) : (
      <Dash />
    );
  return (
    <Panel title="Connect" className="lg:col-span-2">
      <div className="space-y-4 px-5 py-4">
        <dl>
          <Row label={db.tls ? "CQL (TLS)" : "CQL"}>{value("cql", ep.cql)}</Row>
          {db.dialect === "scylla" && <Row label="Shard-aware">{value("sa", ep.cql_shard_aware)}</Row>}
          {ep.alternator && <Row label="Alternator">{value("alt", ep.alternator)}</Row>}
          <Row label="REST (nodetool)">{value("rest", ep.rest)}</Row>
          {db.server_name && <Row label="SNI">{value("sni", db.server_name)}</Row>}
          <Row label="Datacenter / rack">
            {db.datacenter} / {db.rack}
          </Row>
          <Row label="Cluster">{db.cluster_name}</Row>
          <Row label="Host id">{db.host_id ?? <Dash />}</Row>
          <Row label="Tokens">{db.num_tokens ?? <Dash />}</Row>
        </dl>
        <div className="space-y-2 border-t border-line pt-4">
          <p className="text-[12px] text-subtle">Username</p>
          <div className="flex items-center gap-1 rounded-lg border border-line bg-bg px-2.5 py-2">
            <code className="min-w-0 flex-1 truncate font-mono text-[11px]">{db.credentials.username}</code>
            <CopyButton id="user" text={db.credentials.username} copied={copied} onCopy={copy} />
          </div>
          <p className="text-[12px] text-subtle">Password</p>
          <SecretField id="pw" value={db.credentials.password} masked={maskSecret(db.credentials.password)} copied={copied} onCopy={copy} />
          <p className="text-[12px] text-subtle">HTTP data API token</p>
          <SecretField id="token" value={db.http_token} masked={maskSecret(db.http_token)} copied={copied} onCopy={copy} />
          <p className="text-[12px] text-subtle">Import source URL</p>
          <SecretField id="url" value={url} masked={maskCqlUrl(url)} copied={copied} onCopy={copy} />
          <p className="text-[11px] text-subtle">
            {db.tls
              ? "TLS with SNI: the host name picks the database, the role is the superuser."
              : `Locally the username (${db.credentials.role}@${db.id}) picks the database.`}{" "}
            Branches share their parent&apos;s roles; only the contact point changes.
          </p>
        </div>
        <button
          onClick={() => {
            const [host, port] = ep.cql.split(/:(?=\d+$)/);
            copy(
              "env",
              `CASSANDRA_HOST=${host}\nCASSANDRA_PORT=${port ?? 9042}\nCASSANDRA_DC=${db.datacenter}\nCASSANDRA_USERNAME=${db.credentials.username}\nCASSANDRA_PASSWORD=${db.credentials.password}\n`,
            );
          }}
          className={btn}
        >
          {copied === "env" ? "Copied .env" : "Copy as .env"}
        </button>
        <p className="text-[11px] text-subtle">
          Storage: <span className="font-mono">{info.storage}</span>
        </p>
      </div>
    </Panel>
  );
}

function Snippets({ db }: { db: NosqlDb }) {
  const [driver, setDriver] = useState<Driver>("java");
  const [reveal, setReveal] = useState(false);
  const { copied, copy } = useCopy();
  const full = snippet(db, driver);
  const shown = reveal ? full : snippet(db, driver, maskSecret(db.credentials.password));
  return (
    <Panel
      title="Drivers"
      className="lg:col-span-3"
      aside={
        <div className="flex items-center gap-1">
          <button onClick={() => setReveal((r) => !r)} className="px-1.5 text-[12px] text-subtle hover:text-fg">
            {reveal ? "Hide secrets" : "Show secrets"}
          </button>
          <CopyButton id="snippet" text={full} copied={copied} onCopy={copy} />
        </div>
      }
    >
      <div className="flex gap-1 overflow-x-auto border-b border-line px-4 py-2" role="tablist" aria-label="Driver">
        {DRIVERS.map((d) => (
          <button
            key={d}
            role="tab"
            aria-selected={d === driver}
            onClick={() => setDriver(d)}
            className={`shrink-0 rounded-md px-2 py-1 font-mono text-[11px] transition ${
              d === driver ? "bg-panel-2 text-fg" : "text-subtle hover:text-fg"
            }`}
          >
            {DRIVER_LABEL[d]}
          </button>
        ))}
      </div>
      <pre className="min-h-56 flex-1 overflow-x-auto px-5 py-4 font-mono text-[12px] leading-relaxed text-muted">{shown}</pre>
      <p className="border-t border-line px-5 py-3 text-[11px] text-subtle">
        The same snippets as <span className="font-mono text-muted">roll nosql connect {db.name}</span>, with the local
        datacenter set so token- and DC-aware policies route correctly.
      </p>
    </Panel>
  );
}

function SettingsPanel({ db, info, busy, run }: { db: NosqlDb; info: NosqlInfo; busy: string | null; run: Run }) {
  const saved = settingsOf(db);
  const [draft, setDraft] = useState<NosqlSettings>(saved);
  const [name, setName] = useState(db.name);
  const [readOnly, setReadOnly] = useState(db.read_only);
  const [expires, setExpires] = useState<number | null>(db.expires_at);
  const [settingsText, setSettingsText] = useState(() => pretty(db.settings));
  const [maskingText, setMaskingText] = useState(() => pretty(db.masking));

  const parsedSettings = parseObject(settingsText);
  const parsedMasking = parseObject(maskingText);
  const settingsErr = "error" in parsedSettings ? parsedSettings.error : null;
  const maskingErr = "error" in parsedMasking ? parsedMasking.error : null;

  const patch: PatchNosqlReq = {};
  for (const k of Object.keys(saved) as (keyof NosqlSettings)[]) if (draft[k] !== saved[k]) Object.assign(patch, { [k]: draft[k] });
  if (name !== db.name) patch.name = name;
  if (readOnly !== db.read_only) patch.read_only = readOnly;
  if (expires !== db.expires_at) patch.expires_at = expires;
  if ("value" in parsedSettings) {
    const s = settingsPatch(db.settings, parsedSettings.value);
    if (Object.keys(s).length) patch.settings = s;
  }
  if ("value" in parsedMasking && JSON.stringify(parsedMasking.value) !== JSON.stringify(db.masking)) {
    patch.masking = parsedMasking.value as Record<string, string>;
  }
  const changes = Object.keys(patch).length;
  const reloads = patch.settings !== undefined || patch.read_only !== undefined;
  const valid = validName(name) && !settingsErr && !maskingErr && (expires == null || expires > now());
  const defaults = db.dialect === "cassandra" ? info.settings.cassandra : info.settings.scylla;

  const reset = () => {
    setDraft(saved);
    setName(db.name);
    setReadOnly(db.read_only);
    setExpires(db.expires_at);
    setSettingsText(pretty(db.settings));
    setMaskingText(pretty(db.masking));
  };

  return (
    <Panel
      title="Settings"
      aside={<span className="text-[12px] text-subtle">durability and history apply live; engine settings reload the database</span>}
    >
      <form
        className="space-y-6 px-5 py-5"
        onSubmit={(e) => {
          e.preventDefault();
          if (valid && changes) run("settings", () => nosqlApi.update(db.id, patch));
        }}
      >
        <div className="grid gap-x-3 gap-y-4 sm:grid-cols-3">
          <Field label="Name" hint="the id and endpoint stay the same">
            <input value={name} onChange={(e) => setName(e.target.value)} spellCheck={false} className={`${input} w-full font-mono text-[12px]`} />
          </Field>
          <Field label="Access" hint={readOnly ? "writes return Unauthorized" : "reads and writes"}>
            <Segmented
              value={readOnly ? "ro" : "rw"}
              options={[
                { label: "read-write", value: "rw" },
                { label: "read-only", value: "ro" },
              ]}
              onChange={(v) => setReadOnly(v === "ro")}
            />
          </Field>
          <ExpiryField value={expires} onChange={setExpires} />
        </div>
        <SettingsFields value={draft} onChange={setDraft} />
        <div className="grid gap-4 lg:grid-cols-2">
          <JsonField
            label="Engine settings"
            hint={settingsErr ?? "cassandra.yaml / scylla.yaml overrides; delete a key to restore its default"}
            bad={!!settingsErr}
            value={settingsText}
            onChange={setSettingsText}
            placeholder={'{\n  "tombstone_warn_threshold": 1000\n}'}
            aside={
              <details className="text-[11px] text-subtle">
                <summary className="cursor-pointer hover:text-fg">defaults</summary>
                <pre className="mt-2 max-h-48 overflow-auto rounded-lg border border-line bg-bg p-2 font-mono text-[11px] text-muted">
                  {JSON.stringify(defaults, null, 2)}
                </pre>
              </details>
            }
          />
          <JsonField
            label="Masking policy"
            hint={maskingErr ?? "keyspace.table.column → mask_default() | mask_null() | mask_inner(1, 1) | mask_hash() …; applied at read time, inherited by branches"}
            bad={!!maskingErr}
            value={maskingText}
            onChange={setMaskingText}
            placeholder={'{\n  "app.users.email": "mask_inner(1, 1)"\n}'}
          />
        </div>
        <div className="flex flex-wrap items-center justify-between gap-3 border-t border-line pt-4">
          <span className="text-[12px] text-subtle">
            {changes ? `${changes} unsaved change${changes === 1 ? "" : "s"}${reloads ? " · reloads the database; drivers reconnect" : ""}` : "Up to date"}
          </span>
          <div className="flex gap-2">
            {changes > 0 && (
              <button type="button" onClick={reset} className={btn}>
                Reset
              </button>
            )}
            <button type="submit" disabled={busy !== null || !changes || !valid} className={primary}>
              {busy === "settings" ? <span className="shimmer">Applying…</span> : "Save"}
            </button>
          </div>
        </div>
      </form>
    </Panel>
  );
}

function pretty(o: Record<string, unknown>) {
  return Object.keys(o).length ? JSON.stringify(o, null, 2) : "";
}

function JsonField({
  label,
  hint,
  bad,
  value,
  onChange,
  placeholder,
  aside,
}: {
  label: string;
  hint: string;
  bad: boolean;
  value: string;
  onChange: (v: string) => void;
  placeholder: string;
  aside?: ReactNode;
}) {
  return (
    <div className="min-w-0 space-y-1">
      <div className="flex items-center justify-between gap-2">
        <span className="text-[11px] text-subtle">{label}</span>
        {aside}
      </div>
      <textarea
        value={value}
        onChange={(e) => onChange(e.target.value)}
        placeholder={placeholder}
        spellCheck={false}
        rows={5}
        aria-label={label}
        className={`${input} block w-full resize-y font-mono text-[12px] leading-relaxed ${bad ? "border-danger/60 focus:border-danger" : ""}`}
      />
      <p className={`text-[11px] ${bad ? "text-danger" : "text-subtle"}`}>{hint}</p>
    </div>
  );
}

/** No expiry, a preset from now, or a date. */
export function ExpiryField({ value, onChange }: { value: number | null; onChange: (v: number | null) => void }) {
  return (
    <Field label="Expires" hint={value ? `deleted ${formatTime(value)}` : "kept until deleted"}>
      <div className="space-y-2">
        <div className="flex items-center gap-2">
          <input
            type="datetime-local"
            step={60}
            value={value ? toLocalInput(value) : ""}
            min={toLocalInput(now())}
            onChange={(e) => onChange(e.target.value ? fromLocalInput(e.target.value) : null)}
            aria-label="Expiry date"
            className={`${input} min-w-0 flex-1 font-mono text-[12px]`}
          />
          {value != null && (
            <button type="button" onClick={() => onChange(null)} className="text-[12px] text-subtle hover:text-fg">
              Clear
            </button>
          )}
        </div>
        <div className="flex flex-wrap items-center gap-1">
          {EXPIRY_PRESETS.map((p) => (
            <button
              key={p.s}
              type="button"
              onClick={() => onChange(now() + p.s)}
              className="rounded-full border border-line px-2.5 py-0.5 text-[11px] text-muted transition hover:border-line-2 hover:text-fg"
            >
              {p.label}
            </button>
          ))}
          {value != null && value > now() && <Expiry at={value} compact />}
        </div>
      </div>
    </Field>
  );
}

function Maintenance({ db, busy, run }: { db: NosqlDb; busy: string | null; run: Run }) {
  const [note, setNote] = useState<string | null>(null);
  return (
    <Panel title="Storage">
      <div className="space-y-3 px-5 py-4">
        <p className="text-[13px] text-muted">
          Flush writes memtables to SSTables on object storage; compact merges each table&apos;s SSTables (and loads the
          database to do it).
        </p>
        {note && <p className="rise text-[12px] text-muted">{note}</p>}
        <div className="flex flex-wrap gap-2">
          <button
            disabled={busy !== null}
            onClick={async () => {
              const r = await run("flush", () => nosqlApi.flush(db.id));
              if (r) setNote(r.flushed ? "Flushed." : "Nothing to flush: the database isn't loaded.");
            }}
            className={btn}
          >
            <Archive className="size-3.5" />
            {busy === "flush" ? <span className="shimmer">Flushing…</span> : "Flush"}
          </button>
          <button
            disabled={busy !== null}
            onClick={async () => {
              const r = await run("compact", () => nosqlApi.compact(db.id));
              if (r) setNote(`Compacted ${r.compacted_tables} table${r.compacted_tables === 1 ? "" : "s"}.`);
            }}
            className={btn}
          >
            <Layers className="size-3.5" />
            {busy === "compact" ? <span className="shimmer">Compacting…</span> : "Compact"}
          </button>
        </div>
      </div>
    </Panel>
  );
}

function Credentials({ db, busy, run }: { db: NosqlDb; busy: string | null; run: Run }) {
  const [rotating, setRotating] = useState(false);
  const [rotated, setRotated] = useState(false);
  return (
    <Panel title="Credentials">
      <div className="space-y-3 px-5 py-4">
        <p className="text-[13px] text-muted">
          A new password for <span className="font-mono text-fg">{db.credentials.role}</span>. Connected clients stay up;
          the old password stops working at once for new connections.
        </p>
        {rotated && <p className="rise text-[12px] text-ok">Rotated. Copy the new password from Connect.</p>}
        {rotating ? (
          <div className="flex gap-2">
            <button
              disabled={busy !== null}
              onClick={async () => {
                const r = await run("rotate", () => nosqlApi.rotatePassword(db.id));
                setRotating(false);
                setRotated(!!r);
              }}
              className={primary}
            >
              {busy === "rotate" ? <span className="shimmer">Rotating…</span> : "Rotate now"}
            </button>
            <button onClick={() => setRotating(false)} className={btn}>
              Cancel
            </button>
          </div>
        ) : (
          <button
            onClick={() => {
              setRotated(false);
              setRotating(true);
            }}
            className={btn}
          >
            <KeyRound className="size-3.5" /> Rotate password
          </button>
        )}
      </div>
    </Panel>
  );
}

function DeletePanel({ db, busy, run, onDeleted }: { db: NosqlDb; busy: string | null; run: Run; onDeleted: () => void }) {
  const [deleting, setDeleting] = useState(false);
  return (
    <Panel title="Delete database">
      <div className="space-y-3 px-5 py-4">
        <p className="text-[13px] text-muted">
          Disconnects every client and removes the database and its restore history. Branches of it keep their data.
        </p>
        {deleting ? (
          <Confirm
            word={db.name}
            action="Delete database"
            busy={busy === "delete"}
            onCancel={() => setDeleting(false)}
            onConfirm={async () => {
              const r = await run("delete", () => nosqlApi.remove(db.id));
              if (r) onDeleted();
            }}
          >
            This can&apos;t be undone.
          </Confirm>
        ) : (
          <button onClick={() => setDeleting(true)} className={`${btn} hover:border-danger/50 hover:text-danger`}>
            <Trash2 className="size-3.5" /> Delete {db.name}
          </button>
        )}
      </div>
    </Panel>
  );
}
