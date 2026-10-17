"use client";

import { Eye, EyeOff, KeyRound, Plus, RotateCw, Trash2 } from "lucide-react";
import { useState } from "react";
import { rel, tasks, TASKS_API, type ApiKey } from "@/lib/tasks-api";
import { btn, CodeBlock, CopyButton, Empty, ICON, iconBtn, InlineError, input, Loading, Panel, primary, Spinner, useActions, useNow, usePoll } from "./ui";

function mask(secret: string) {
  return `${secret.slice(0, 10)}${"•".repeat(16)}`;
}

export function SettingsTab() {
  return (
    <div className="space-y-4">
      <div className="grid gap-4 lg:grid-cols-2">
        <SigningKeys />
        <ApiKeys />
      </div>
      <Connect />
    </div>
  );
}

function Secret({ label, value, hint }: { label: string; value: string; hint: string }) {
  const [reveal, setReveal] = useState(false);
  return (
    <div className="space-y-1">
      <p className="text-[12px] text-subtle">{label}</p>
      <div className="flex items-center gap-1 rounded-lg border border-line bg-bg px-2.5 py-1.5">
        <code className="min-w-0 flex-1 truncate font-mono text-[11px]">{reveal ? value : mask(value)}</code>
        <button onClick={() => setReveal((r) => !r)} className={iconBtn} title={reveal ? "Hide" : "Reveal"}>
          {reveal ? <EyeOff {...ICON} /> : <Eye {...ICON} />}
        </button>
        <CopyButton text={value} id={label} />
      </div>
      <p className="text-[11px] text-subtle">{hint}</p>
    </div>
  );
}

function SigningKeys() {
  const { run, busy } = useActions();
  const keys = usePoll(() => tasks.signingKeys(), 30_000);
  const now = useNow(30_000);
  const k = keys.data;
  return (
    <Panel
      title="Signing keys"
      meta={k ? `rotated ${rel(k.rotated_at, now)}` : undefined}
      actions={
        <button
          disabled={busy !== null || !k}
          onClick={async () => {
            if (
              confirm(
                "Rotate signing keys? The next key becomes current and a new next key is made. Receivers that only trust the current key will start rejecting requests once they stop matching.",
              ) &&
              (await run("rotate", () => tasks.rotateSigningKeys())) !== undefined
            )
              keys.reload();
          }}
          className={btn}
        >
          {busy === "rotate" ? <Spinner /> : <RotateCw {...ICON} />} Rotate
        </button>
      }
    >
      <InlineError error={keys.error} />
      {!k ? (
        <Loading />
      ) : (
        <div className="space-y-4 px-5 py-4">
          <Secret label="Current" value={k.current} hint="Signs every request today." />
          <Secret label="Next" value={k.next} hint="Also signs every request, so receivers can trust it before you rotate." />
        </div>
      )}
      <div className="mt-auto space-y-1.5 border-t border-line px-5 py-3 text-[12px] text-subtle">
        <p>
          Requests follow{" "}
          <a href="https://www.standardwebhooks.com/" target="_blank" rel="noreferrer" className="text-accent hover:underline">
            Standard Webhooks
          </a>
          : <code className="font-mono text-muted">webhook-id</code>, <code className="font-mono text-muted">webhook-timestamp</code> and{" "}
          <code className="font-mono text-muted">webhook-signature</code> (<code className="font-mono">v1,&lt;base64 HMAC-SHA256&gt;</code> of{" "}
          <code className="font-mono">id.timestamp.body</code>, one per key).
        </p>
        <p>Any Standard Webhooks library verifies them; reject timestamps more than 5 minutes old.</p>
      </div>
    </Panel>
  );
}

function ApiKeys() {
  const { run, busy } = useActions();
  const keys = usePoll(() => tasks.apiKeys(), 15_000);
  const [name, setName] = useState("");
  const [created, setCreated] = useState<(ApiKey & { token: string }) | null>(null);
  const now = useNow(30_000);

  return (
    <Panel title="API keys" meta={keys.data ? keys.data.length : undefined}>
      {created && (
        <div className="rise space-y-2 border-b border-line bg-panel-2/40 px-5 py-4">
          <p className="text-[12px]">
            Token for <span className="font-mono">{created.name}</span> — copy it now, it isn&apos;t shown again.
          </p>
          <div className="flex items-center gap-1 rounded-lg border border-accent/40 bg-bg px-2.5 py-1.5">
            <code className="min-w-0 flex-1 truncate font-mono text-[12px]">{created.token}</code>
            <CopyButton text={created.token} />
          </div>
          <button onClick={() => setCreated(null)} className="text-[11px] text-subtle hover:text-fg">
            Done
          </button>
        </div>
      )}
      <InlineError error={keys.error} />
      {!keys.data ? (
        <Loading />
      ) : keys.data.length === 0 ? (
        <Empty title="No API keys">Keys authenticate publishers and SDKs when rolld runs with a token.</Empty>
      ) : (
        <ul>
          {keys.data.map((k) => (
            <li key={k.id} className="flex items-center gap-3 border-b border-line px-5 py-3 last:border-0">
              <KeyRound className="size-4 shrink-0 text-subtle" strokeWidth={1.5} />
              <div className="min-w-0 flex-1">
                <p className="truncate text-[13px]">{k.name}</p>
                <p className="truncate font-mono text-[11px] text-subtle">
                  {k.prefix}… · created {rel(k.created_at, now)}
                  {k.last_used_at ? ` · used ${rel(k.last_used_at, now)}` : ""}
                </p>
              </div>
              <button
                title="Delete key"
                disabled={busy !== null}
                onClick={async () => {
                  if (confirm(`Delete ${k.name}? Anything using it stops working.`) && (await run(`del-${k.id}`, () => tasks.deleteApiKey(k.id))) !== undefined)
                    keys.reload();
                }}
                className={`${iconBtn} hover:text-danger`}
              >
                <Trash2 {...ICON} />
              </button>
            </li>
          ))}
        </ul>
      )}
      <form
        className="mt-auto flex items-center gap-2 border-t border-line px-5 py-3"
        onSubmit={async (e) => {
          e.preventDefault();
          const out = await run("key-new", () => tasks.createApiKey(name.trim()));
          if (out) {
            setCreated(out);
            setName("");
            keys.reload();
          }
        }}
      >
        <input value={name} onChange={(e) => setName(e.target.value)} placeholder="Key name, e.g. production" className={`${input} min-w-0 flex-1 text-[12px]`} />
        <button type="submit" disabled={busy !== null || !name.trim()} className={primary}>
          <Plus {...ICON} /> Create
        </button>
      </form>
    </Panel>
  );
}

function Connect() {
  const curl = `curl -X POST ${TASKS_API}/publish \\
  -H "Authorization: Bearer $ROLL_TASKS_TOKEN" \\
  -H "Content-Type: application/json" \\
  -d '{
    "url": "https://example.com/api/webhook",
    "body": { "hello": "world" },
    "delay": "10s",
    "retries": 3
  }'`;
  const raw = `curl -X POST ${TASKS_API}/publish/https://example.com/api/webhook \\
  -H "Authorization: Bearer $ROLL_TASKS_TOKEN" \\
  -H "Roll-Delay: 10s" -H "Roll-Retries: 3" \\
  -d '{"hello":"world"}'`;
  return (
    <Panel title="Connect" meta={<span className="font-mono">{TASKS_API}</span>}>
      <div className="grid gap-4 px-5 py-4 lg:grid-cols-2">
        <div className="space-y-1.5">
          <p className="text-[12px] text-subtle">Publish with JSON options</p>
          <CodeBlock raw={curl} />
        </div>
        <div className="space-y-1.5">
          <p className="text-[12px] text-subtle">Or forward a raw body, options in Roll-* headers</p>
          <CodeBlock raw={raw} />
        </div>
      </div>
      <p className="border-t border-line px-5 py-3 text-[12px] text-subtle">
        Without <code className="font-mono">ROLL_TASKS_TOKEN</code> on rolld the local API is open and the Authorization header is ignored.
      </p>
    </Panel>
  );
}
