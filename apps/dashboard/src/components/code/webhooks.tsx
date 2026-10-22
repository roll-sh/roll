"use client";

import { ChevronDown, ChevronRight, Plus, Send, Trash2 } from "lucide-react";
import { useEffect, useLayoutEffect, useState } from "react";
import { codeApi, WEBHOOK_EVENTS, type Repo, type Webhook } from "@/lib/code-api";
import {
  CI_SYSTEMS,
  emptyWebhookForm,
  eventsLabel,
  statusScript,
  WEBHOOK_EVENT_HINT,
  webhookBody,
  webhookErrors,
  webhookForm,
  type WebhookForm,
} from "@/lib/ci";
import { btn, Confirm, CopyButton, input, Panel, primary, useCopy } from "../ui";
import { DeliveryLog } from "./deliveries";
import { Field, Segmented, type Run } from "./shared";

type Act = <T>(key: string, fn: () => Promise<T>) => Promise<T | undefined>;

/**
 * Repo webhooks: signed deliveries of the repo's events (the same data as its event stream) to any
 * URL, with a delivery log and redelivery. Writers only; others see why the panel is empty.
 */
export function WebhooksPanel({ repo, writer, busy, run }: { repo: Repo; writer: boolean; busy: string | null; run: Run }) {
  const [hooks, setHooks] = useState<Webhook[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);
  const [created, setCreated] = useState<{ url: string; secret: string | null } | null>(null);
  const [version, setVersion] = useState(0);

  // The secret is shown once: Next keeps visited pages alive but hidden (<Activity>,
  // cacheComponents), so drop it when this page is hidden or left, not only when it unmounts.
  useLayoutEffect(() => () => setCreated(null), []);

  useEffect(() => {
    if (!writer) return;
    let alive = true;
    codeApi.webhooks(repo.full_name).then(
      (h) => alive && (setHooks(h), setError(null)),
      (e) => alive && setError(e instanceof Error ? e.message : String(e)),
    );
    return () => {
      alive = false;
    };
  }, [repo.full_name, writer, version]);

  const act: Act = async (key, fn) => {
    const out = await run(key, fn);
    setVersion((v) => v + 1);
    return out;
  };

  return (
    <section id="webhooks" className="scroll-mt-24">
      <Panel
        title="Webhooks"
        aside={
          writer &&
          !adding && (
            <button onClick={() => (setAdding(true), setCreated(null))} className={btn}>
              <Plus className="size-3.5" /> Add webhook
            </button>
          )
        }
      >
        <div className="space-y-4 px-5 py-4">
          <p className="max-w-2xl text-[12px] text-subtle">
            Every event this repo sends to the dashboard, POSTed to your URL and signed with{" "}
            <span className="font-mono">X-Roll-Signature-256</span> (HMAC-SHA256 of <span className="font-mono">timestamp.body</span>). Anything
            but a 2xx is retried with backoff, eight times.
          </p>
          {!writer && <p className="text-[12px] text-subtle">Only people who can write to {repo.full_name} can see and manage its webhooks.</p>}
          {error && <p className="text-[12px] text-danger">{error}</p>}
          {created && <CreatedNote created={created} onDone={() => setCreated(null)} />}
          {adding && (
            <WebhookEditor
              initial={emptyWebhookForm()}
              busy={busy}
              submitLabel="Add webhook"
              onCancel={() => setAdding(false)}
              onSubmit={async (f) => {
                const out = await act("hook-create", () => codeApi.createWebhook(repo.full_name, { ...webhookBody(f), url: f.url.trim() }));
                if (out) {
                  setAdding(false);
                  setCreated({ url: out.webhook.url, secret: out.secret ?? null });
                }
              }}
            />
          )}
          {writer && hooks && hooks.length === 0 && !adding && <p className="text-[12px] text-subtle">No webhooks yet.</p>}
        </div>
        {writer && hooks && hooks.length > 0 && (
          <ul className="border-t border-line">
            {hooks.map((h) => (
              <WebhookRow key={h.id} repo={repo.full_name} hook={h} busy={busy} act={act} />
            ))}
          </ul>
        )}
      </Panel>
    </section>
  );
}

function CreatedNote({ created, onDone }: { created: { url: string; secret: string | null }; onDone: () => void }) {
  const { copied, copy } = useCopy();
  return (
    <div className="rise space-y-3 rounded-lg border border-accent/40 bg-accent/5 p-4">
      <p className="text-[13px]">
        Added <span className="font-mono">{created.url}</span> and sent it a <span className="font-mono">ping</span>.
      </p>
      {created.secret ? (
        <>
          <p className="text-[12px] text-muted">Shown once. Check every delivery&apos;s signature with this secret.</p>
          <div className="flex items-center gap-2">
            <span className="w-12 shrink-0 text-[11px] text-subtle">secret</span>
            <code className="min-w-0 flex-1 truncate rounded-lg border border-line bg-bg px-2.5 py-1.5 font-mono text-[11px]">{created.secret}</code>
            <CopyButton id="secret" text={created.secret} copied={copied} onCopy={copy} />
          </div>
        </>
      ) : (
        <p className="text-[12px] text-muted">Deliveries are signed with the secret you gave.</p>
      )}
      <button type="button" onClick={onDone} className={btn}>
        {created.secret ? "I've saved it" : "Done"}
      </button>
    </div>
  );
}

function WebhookEditor({
  initial,
  busy,
  submitLabel,
  existing,
  onSubmit,
  onCancel,
}: {
  initial: WebhookForm;
  busy: string | null;
  submitLabel: string;
  /** Editing: the secret field rotates it. */
  existing?: boolean;
  onSubmit: (f: WebhookForm) => void;
  onCancel: () => void;
}) {
  const [f, setF] = useState<WebhookForm>(initial);
  const [touched, setTouched] = useState(false);
  const errors = webhookErrors(f);
  const bad = Object.keys(errors).length > 0;
  const set = <K extends keyof WebhookForm>(k: K, v: WebhookForm[K]) => setF((x) => ({ ...x, [k]: v }));
  const toggle = (e: (typeof WEBHOOK_EVENTS)[number]) =>
    set("events", f.events.includes(e) ? f.events.filter((x) => x !== e) : [...f.events, e]);

  return (
    <form
      className="rise space-y-4 rounded-lg border border-line bg-bg/40 p-4"
      onSubmit={(e) => {
        e.preventDefault();
        setTouched(true);
        if (!bad) onSubmit(f);
      }}
    >
      <div className="grid gap-4 sm:grid-cols-2">
        <Field label="Payload URL" hint={touched ? errors.url : undefined}>
          <input
            value={f.url}
            onChange={(e) => set("url", e.target.value)}
            placeholder="https://ci.example.com/hooks/roll"
            spellCheck={false}
            autoFocus={!existing}
            className={`${input} w-full font-mono text-[12px]`}
          />
        </Field>
        <Field
          label={existing ? "New secret" : "Secret"}
          hint={touched && errors.secret ? errors.secret : existing ? "leave empty to keep the current one" : "leave empty and roll generates one"}
        >
          <input
            value={f.secret}
            onChange={(e) => set("secret", e.target.value)}
            type="password"
            autoComplete="off"
            spellCheck={false}
            className={`${input} w-full font-mono text-[12px]`}
          />
        </Field>
      </div>
      <div className="space-y-2">
        <div className="flex items-center gap-3">
          <span className="text-[11px] text-subtle">Events</span>
          <Segmented
            value={f.all ? "all" : "pick"}
            options={[
              { label: "pick", value: "pick" },
              { label: "everything", value: "all" },
            ]}
            onChange={(v) => set("all", v === "all")}
          />
          {touched && errors.events && <span className="text-[11px] text-danger">{errors.events}</span>}
        </div>
        {!f.all && (
          <div className="grid gap-1.5 sm:grid-cols-2">
            {WEBHOOK_EVENTS.map((e) => (
              <label key={e} className="flex cursor-pointer items-start gap-2 text-[12px]">
                <input type="checkbox" checked={f.events.includes(e)} onChange={() => toggle(e)} className="mt-0.5 accent-[var(--accent)]" />
                <span className="min-w-0">
                  <span className="font-mono">{e}</span> <span className="text-subtle">— {WEBHOOK_EVENT_HINT[e]}</span>
                </span>
              </label>
            ))}
          </div>
        )}
      </div>
      <label className="flex cursor-pointer items-center gap-2 text-[12px]">
        <input type="checkbox" checked={f.active} onChange={(e) => set("active", e.target.checked)} className="accent-[var(--accent)]" />
        Active <span className="text-subtle">— paused webhooks keep their settings and get nothing</span>
      </label>
      <div className="flex gap-2">
        <button type="submit" disabled={busy !== null || (touched && bad)} className={primary}>
          {busy?.startsWith("hook-") ? <span className="shimmer">Saving…</span> : submitLabel}
        </button>
        <button type="button" onClick={onCancel} className={btn}>
          Cancel
        </button>
      </div>
    </form>
  );
}

function WebhookRow({ repo, hook, busy, act }: { repo: string; hook: Webhook; busy: string | null; act: Act }) {
  const [open, setOpen] = useState(false);
  const [editing, setEditing] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [log, setLog] = useState(0);
  return (
    <li className="border-b border-line last:border-0">
      <div className="flex flex-wrap items-center gap-3 px-5 py-3">
        <button type="button" onClick={() => setOpen((o) => !o)} className="text-subtle hover:text-fg" aria-label={open ? "Hide deliveries" : "Show deliveries"}>
          {open ? <ChevronDown className="size-3.5" /> : <ChevronRight className="size-3.5" />}
        </button>
        <span className={`inline-block size-1.5 shrink-0 rounded-full ${hook.active ? "bg-ok" : "bg-line-2"}`} title={hook.active ? "active" : "paused"} />
        <div className="min-w-0 flex-1">
          <p className="truncate font-mono text-[12px]">{hook.url}</p>
          <p className="truncate text-[11px] text-subtle">
            {eventsLabel(hook.events)} · added by {hook.created_by}
            {!hook.active && " · paused"}
          </p>
        </div>
        <span className="flex items-center gap-1.5">
          <button
            type="button"
            onClick={async () => {
              await act(`hook-ping-${hook.id}`, () => codeApi.pingWebhook(repo, hook.id));
              setOpen(true);
              setLog((v) => v + 1);
            }}
            disabled={busy !== null}
            className={btn}
            title="Send a ping delivery"
          >
            <Send className="size-3.5" /> {busy === `hook-ping-${hook.id}` ? <span className="shimmer">Pinging…</span> : "Ping"}
          </button>
          <button type="button" onClick={() => setEditing((e) => !e)} disabled={busy !== null} className={btn}>
            Edit
          </button>
          <button type="button" onClick={() => setDeleting(true)} disabled={busy !== null} className={btn} aria-label="Delete webhook">
            <Trash2 className="size-3.5" />
          </button>
        </span>
      </div>
      {editing && (
        <div className="px-5 pb-4">
          <WebhookEditor
            initial={webhookForm(hook)}
            existing
            busy={busy}
            submitLabel="Save"
            onCancel={() => setEditing(false)}
            onSubmit={async (f) => {
              const body = webhookBody(f, hook);
              if (Object.keys(body).length === 0) return setEditing(false);
              const out = await act(`hook-save-${hook.id}`, () => codeApi.updateWebhook(repo, hook.id, body));
              if (out) setEditing(false);
            }}
          />
        </div>
      )}
      {deleting && (
        <div className="px-5 pb-4">
          <Confirm
            word="delete"
            action="Delete webhook"
            busy={busy === `hook-delete-${hook.id}`}
            onCancel={() => setDeleting(false)}
            onConfirm={() => act(`hook-delete-${hook.id}`, () => codeApi.deleteWebhook(repo, hook.id))}
          >
            Stops deliveries to <span className="font-mono text-fg">{hook.url}</span>. Pending retries are dropped.
          </Confirm>
        </div>
      )}
      {open && (
        <DeliveryLog
          load={() => codeApi.webhookDeliveries(repo, hook.id)}
          redeliver={(d) => codeApi.redeliverWebhook(repo, hook.id, d)}
          deps={`${hook.id}:${log}`}
          busy={busy}
          act={act}
        />
      )}
    </li>
  );
}

/**
 * How external CI reports to roll: a commit status per run, posted for the commit it built. On a
 * queue repo that's what `required_checks` look at, so TeamCity or Jenkins gate the merge queue.
 */
export function CiStatusesPanel({ repo }: { repo: Repo }) {
  const [system, setSystem] = useState(CI_SYSTEMS[0].name);
  const [context, setContext] = useState("ci/build");
  const { copied, copy } = useCopy();
  const ci = CI_SYSTEMS.find((c) => c.name === system) ?? CI_SYSTEMS[0];
  const script = statusScript(codeApi.url(repo.full_name), ci, context.trim() || "ci");
  return (
    <Panel title="CI statuses">
      <div className="space-y-4 px-5 py-4">
        <p className="max-w-2xl text-[12px] text-subtle">
          External CI posts a status for the commit it built; roll turns it into a check named after its context on every open change at
          that commit
          {repo.land_policy === "queue" ? ", which is what the merge queue's required checks wait for" : ""}. Give the job an owner token (
          <span className="font-mono">roll code token create {repo.owner} ci</span>) as <span className="font-mono">ROLL_TOKEN</span>. To
          start builds from roll, add a webhook for <span className="font-mono">push</span> and <span className="font-mono">landed</span>.
        </p>
        <div className="flex flex-wrap items-end gap-4">
          <Field label="CI">
            <Segmented value={system} options={CI_SYSTEMS.map((c) => ({ label: c.name, value: c.name }))} onChange={setSystem} />
          </Field>
          <Field label="Context" hint="the check's name">
            <input value={context} onChange={(e) => setContext(e.target.value)} spellCheck={false} className={`${input} w-44 font-mono text-[12px]`} />
          </Field>
        </div>
        <div className="relative rounded-lg border border-line bg-bg">
          <pre className="overflow-x-auto px-3 py-2 pr-9 font-mono text-[11px] leading-5">{script}</pre>
          <span className="absolute top-1.5 right-1.5">
            <CopyButton id="ci" text={script} copied={copied} onCopy={copy} />
          </span>
        </div>
        <p className="text-[11px] text-subtle">{ci.note}.</p>
      </div>
    </Panel>
  );
}
