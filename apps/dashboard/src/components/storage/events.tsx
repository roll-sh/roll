"use client";

import { Cloud, Inbox, Plus, RefreshCw, Trash2, Waves, Webhook } from "lucide-react";
import { useState } from "react";
import {
  COMMON_EVENTS,
  describeDestination,
  destinationArn,
  destinationError,
  EVENT_TYPES,
  maskSecret,
  notificationInput,
  storageApi,
  type Bucket,
  type Destination,
  type NotificationConfig,
} from "@/lib/storage-api";
import { iconBtn } from "../tasks/ui";
import { btn, ErrorNote, input, Panel, primary, SecretField, useCopy } from "../ui";
import { Chip, Field, InlineConfirm, Segmented, useAction } from "./shared";

type Kind = Destination["kind"];

const KINDS: { label: string; value: Kind }[] = [
  { label: "roll queue", value: "queue" },
  { label: "roll stream", value: "stream" },
  { label: "webhook", value: "webhook" },
  { label: "ARN", value: "arn" },
];

const KIND_ICON = { queue: Inbox, stream: Waves, webhook: Webhook, arn: Cloud } as const;
const KIND_LABEL = { queue: "queue", stream: "stream", webhook: "webhook", arn: "ARN" } as const;

/** `s3:ObjectCreated:*` → `ObjectCreated:*`: the prefix is the same everywhere. */
const short = (e: string) => e.replace(/^s3:/, "");

/** Event notifications: where object changes go, and the webhook signing secret. */
export function EventsPanel({ bucket, onChanged }: { bucket: Bucket; onChanged: () => Promise<unknown> }) {
  const n = bucket.notifications;
  const { busy, error, run } = useAction(onChanged);
  const [adding, setAdding] = useState(false);
  const [removing, setRemoving] = useState<string | null>(null);
  const configs = n?.configurations ?? [];
  const backlog = n?.backlog ?? {};

  const remove = (id: string) =>
    run(`rm-${id}`, () =>
      storageApi.setNotifications(
        bucket.name,
        configs.filter((c) => c.id !== id).map(notificationInput),
        // What stays was validated when it was added.
        { skipValidation: true },
      ),
    );

  return (
    <Panel title="Events" aside={<span className="text-[12px] text-subtle">notifications when objects change</span>}>
      <div className="space-y-4 px-5 py-4">
        <p className="text-[13px] text-muted">
          Send S3 event notifications to a roll queue, a roll stream or a webhook. Delivery is at least once and in order per key;
          each destination gets a test event when it&apos;s added.
        </p>
        <ErrorNote error={error} />
        {configs.length === 0 ? (
          <p className="text-[13px] text-subtle">No destinations.</p>
        ) : (
          <ul className="overflow-hidden rounded-lg border border-line">
            {configs.map((c) => (
              <ConfigRow
                key={c.id}
                c={c}
                backlog={backlog[c.destination]}
                confirming={removing === c.id}
                busy={busy === `rm-${c.id}`}
                onRemove={() => setRemoving(c.id)}
                onCancel={() => setRemoving(null)}
                onConfirm={async () => {
                  await remove(c.id);
                  setRemoving(null);
                }}
              />
            ))}
          </ul>
        )}
        {adding ? (
          <AddDestination
            busy={busy === "add"}
            onCancel={() => setAdding(false)}
            onAdd={async (cfg) => {
              const r = await run("add", () => storageApi.setNotifications(bucket.name, [...configs.map(notificationInput), cfg]));
              if (r) setAdding(false);
            }}
          />
        ) : (
          <button type="button" onClick={() => setAdding(true)} className={btn}>
            <Plus className="size-3.5" /> Add destination
          </button>
        )}
        {n?.webhook_secret && <WebhookSecret secret={n.webhook_secret} busy={busy === "rotate"} onRotate={() => run("rotate", () => storageApi.rotateWebhookSecret(bucket.name))} />}
      </div>
    </Panel>
  );
}

function ConfigRow({
  c,
  backlog,
  confirming,
  busy,
  onRemove,
  onCancel,
  onConfirm,
}: {
  c: NotificationConfig;
  backlog?: { pending: number; last_error: string | null };
  confirming: boolean;
  busy: boolean;
  onRemove: () => void;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  const d = describeDestination(c.destination);
  const Icon = KIND_ICON[d.kind];
  const filter = [c.prefix && `prefix ${c.prefix}`, c.suffix && `suffix ${c.suffix}`].filter(Boolean).join(" · ");
  return (
    <li className="rise flex items-start gap-3 border-b border-line px-4 py-3 last:border-0">
      <Icon className="mt-0.5 size-4 shrink-0 text-subtle" />
      <div className="min-w-0 flex-1 space-y-0.5">
        <p className="flex min-w-0 items-center gap-2 text-[13px]">
          <span className="shrink-0 text-subtle">{KIND_LABEL[d.kind]}</span>
          <span className="truncate font-mono text-[12px]" title={c.destination}>
            {d.label}
          </span>
        </p>
        <p className="truncate font-mono text-[11px] text-subtle" title={c.events.join(", ")}>
          {c.events.map(short).join(", ")}
          {filter && ` · ${filter}`}
        </p>
        {backlog && backlog.pending > 0 && (
          <p className="flex items-center gap-1.5 text-[12px] text-muted">
            <span className="size-1.5 rounded-full bg-accent" aria-hidden />
            {backlog.pending >= 1000 ? "1000+" : backlog.pending} waiting
            {backlog.last_error && <span className="truncate text-danger"> · {backlog.last_error}</span>}
          </p>
        )}
      </div>
      {confirming ? (
        <InlineConfirm label="Remove" busy={busy} onConfirm={onConfirm} onCancel={onCancel} />
      ) : (
        <button type="button" onClick={onRemove} className={`${iconBtn} hover:text-danger`} aria-label={`Remove ${d.label}`} title="Remove">
          <Trash2 className="size-3.5" />
        </button>
      )}
    </li>
  );
}

function AddDestination({
  busy,
  onAdd,
  onCancel,
}: {
  busy: boolean;
  onAdd: (cfg: { destination: string; events: string[]; prefix: string | null; suffix: string | null }) => void;
  onCancel: () => void;
}) {
  const [kind, setKind] = useState<Kind>("queue");
  const [vhost, setVhost] = useState("/");
  const [queue, setQueue] = useState("");
  const [stream, setStream] = useState("");
  const [topic, setTopic] = useState("");
  const [url, setUrl] = useState("https://");
  const [arn, setArn] = useState("");
  const [events, setEvents] = useState<string[]>(["s3:ObjectCreated:*"]);
  const [more, setMore] = useState(false);
  const [prefix, setPrefix] = useState("");
  const [suffix, setSuffix] = useState("");
  const [touched, setTouched] = useState(false);

  const dest: Destination =
    kind === "queue"
      ? { kind, vhost, queue }
      : kind === "stream"
        ? { kind, stream, topic }
        : kind === "webhook"
          ? { kind, url }
          : { kind, arn };
  const err = destinationError(dest) ?? (events.length === 0 ? "Pick at least one event." : null);
  const toggle = (e: string) => setEvents((x) => (x.includes(e) ? x.filter((y) => y !== e) : [...x, e]));
  const others = EVENT_TYPES.filter((e) => !COMMON_EVENTS.includes(e));
  const text = (value: string, set: (v: string) => void, placeholder: string, label: string) => (
    <input
      value={value}
      onChange={(e) => set(e.target.value)}
      placeholder={placeholder}
      aria-label={label}
      spellCheck={false}
      className={`${input} w-full font-mono text-[12px]`}
    />
  );

  return (
    <form
      className="rise space-y-4 rounded-lg border border-line bg-bg p-4"
      onSubmit={(e) => {
        e.preventDefault();
        setTouched(true);
        if (!err) onAdd({ destination: destinationArn(dest), events, prefix: prefix || null, suffix: suffix || null });
      }}
    >
      <Field label="Destination">
        <Segmented value={kind} options={KINDS} onChange={setKind} />
      </Field>
      {kind === "queue" && (
        <div className="grid gap-3 sm:grid-cols-[8rem_minmax(0,1fr)]">
          <Field label="Vhost">{text(vhost, setVhost, "/", "Vhost")}</Field>
          <Field label="Queue" hint="published to the queue, confirmed">
            {text(queue, setQueue, "uploads", "Queue")}
          </Field>
        </div>
      )}
      {kind === "stream" && (
        <div className="grid gap-3 sm:grid-cols-2">
          <Field label="Stream">{text(stream, setStream, "events", "Stream")}</Field>
          <Field label="Topic" hint="produced keyed by object key, acks=all">
            {text(topic, setTopic, "uploads", "Topic")}
          </Field>
        </div>
      )}
      {kind === "webhook" && (
        <Field label="URL" hint="POST of the event JSON, signed with the bucket's webhook secret">
          {text(url, setUrl, "https://example.com/hooks/s3", "Webhook URL")}
        </Field>
      )}
      {kind === "arn" && (
        <Field label="ARN" hint="arn:roll:… or arn:aws:sqs|sns|lambda:… (AWS needs delivery to AWS enabled)">
          {text(arn, setArn, "arn:aws:sqs:us-east-1:123456789012:jobs", "Destination ARN")}
        </Field>
      )}
      <Field label="Events">
        <div className="flex flex-wrap gap-1">
          {COMMON_EVENTS.map((e) => (
            <Chip key={e} on={events.includes(e)} onClick={() => toggle(e)}>
              {short(e)}
            </Chip>
          ))}
          <button type="button" onClick={() => setMore((m) => !m)} className="px-1.5 text-[11px] text-subtle hover:text-fg">
            {more ? "fewer" : "more…"}
          </button>
        </div>
        {more && (
          <div className="rise mt-1 flex flex-wrap gap-1">
            {others.map((e) => (
              <Chip key={e} on={events.includes(e)} onClick={() => toggle(e)}>
                {short(e)}
              </Chip>
            ))}
          </div>
        )}
      </Field>
      <div className="grid gap-3 sm:grid-cols-2">
        <Field label="Key prefix" hint={prefix ? undefined : "every key"}>
          {text(prefix, setPrefix, "uploads/", "Key prefix")}
        </Field>
        <Field label="Key suffix">{text(suffix, setSuffix, ".jpg", "Key suffix")}</Field>
      </div>
      {touched && err && <p className="text-[12px] text-danger">{err}</p>}
      <div className="flex gap-2">
        <button type="submit" disabled={busy} className={primary}>
          {busy ? <span className="shimmer">Sending a test event…</span> : "Add destination"}
        </button>
        <button type="button" onClick={onCancel} className={btn}>
          Cancel
        </button>
      </div>
    </form>
  );
}

function WebhookSecret({ secret, busy, onRotate }: { secret: string; busy: boolean; onRotate: () => void }) {
  const { copied, copy } = useCopy();
  const [confirming, setConfirming] = useState(false);
  return (
    <div className="space-y-2 border-t border-line pt-4">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div>
          <p className="text-[13px]">Webhook signing secret</p>
          <p className="text-[12px] text-subtle">
            Verify <span className="font-mono">X-Roll-Signature: t=…,v1=…</span>, an HMAC-SHA256 of{" "}
            <span className="font-mono">&quot;&lt;t&gt;.&lt;body&gt;&quot;</span>.
          </p>
        </div>
        {confirming ? (
          <InlineConfirm
            label="Rotate"
            busy={busy}
            danger={false}
            onConfirm={async () => {
              await onRotate();
              setConfirming(false);
            }}
            onCancel={() => setConfirming(false)}
          />
        ) : (
          <button type="button" onClick={() => setConfirming(true)} className={btn}>
            <RefreshCw className="size-3.5" /> Rotate
          </button>
        )}
      </div>
      {confirming && <p className="rise text-[12px] text-muted">Deliveries are signed with the new secret right away: update your receivers.</p>}
      <SecretField id="whsec" value={secret} masked={maskSecret(secret)} copied={copied} onCopy={copy} />
    </div>
  );
}
