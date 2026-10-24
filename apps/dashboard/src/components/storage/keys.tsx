"use client";

import { KeyRound, Plus, Trash2 } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import {
  agoMs,
  PERMISSIONS,
  storageApi,
  type AccessKey,
  type Bucket,
  type CreatedKey,
  type Permission,
  type StorageInfo,
} from "@/lib/storage-api";
import { btn, CopyButton, ErrorNote, input, Panel, primary, useCopy } from "../ui";
import { Field, Segmented, useAction } from "./shared";

const SDKS = ["aws cli", "JS (v3)", "boto3", "rclone"] as const;
type Sdk = (typeof SDKS)[number];

export function snippet(sdk: Sdk, endpoint: string, region: string, id: string, secret: string, bucket = "my-bucket") {
  switch (sdk) {
    case "aws cli":
      return `export AWS_ACCESS_KEY_ID=${id}\nexport AWS_SECRET_ACCESS_KEY=${secret}\nexport AWS_ENDPOINT_URL=${endpoint}\nexport AWS_REGION=${region}\n\naws s3 cp ./photo.jpg s3://${bucket}/photos/photo.jpg\naws s3 ls s3://${bucket}/photos/`;
    case "JS (v3)":
      return `// npm i @aws-sdk/client-s3\nimport { S3Client, PutObjectCommand } from "@aws-sdk/client-s3";\n\nconst s3 = new S3Client({\n  endpoint: "${endpoint}",\n  region: "${region}",\n  forcePathStyle: true,\n  credentials: {\n    accessKeyId: "${id}",\n    secretAccessKey: "${secret}",\n  },\n});\n\nawait s3.send(new PutObjectCommand({\n  Bucket: "${bucket}",\n  Key: "hello.txt",\n  Body: "hello from roll",\n}));`;
    case "boto3":
      return `# pip install boto3\nimport boto3\n\ns3 = boto3.client(\n    "s3",\n    endpoint_url="${endpoint}",\n    region_name="${region}",\n    aws_access_key_id="${id}",\n    aws_secret_access_key="${secret}",\n)\n\ns3.put_object(Bucket="${bucket}", Key="hello.txt", Body=b"hello from roll")`;
    case "rclone":
      return `# ~/.config/rclone/rclone.conf\n[roll]\ntype = s3\nprovider = Other\nendpoint = ${endpoint}\nregion = ${region}\naccess_key_id = ${id}\nsecret_access_key = ${secret}\nforce_path_style = true\n\n# rclone sync ./site roll:${bucket}/site`;
  }
}

function mask(secret: string) {
  return `${secret.slice(0, 4)}${"•".repeat(12)}`;
}

/** Code sample for a set of credentials, with a client switcher. */
export function SdkSnippet({
  info,
  id,
  secret,
  bucket,
  reveal = true,
}: {
  info: StorageInfo;
  id: string;
  secret: string;
  bucket?: string;
  reveal?: boolean;
}) {
  const [sdk, setSdk] = useState<Sdk>("aws cli");
  const { copied, copy } = useCopy();
  const full = snippet(sdk, info.endpoint, info.region, id, secret, bucket);
  return (
    <div className="overflow-hidden rounded-lg border border-line bg-bg">
      <div className="flex items-center gap-1 overflow-x-auto border-b border-line px-2 py-1.5">
        {SDKS.map((s) => (
          <button
            key={s}
            type="button"
            onClick={() => setSdk(s)}
            className={`shrink-0 rounded-md px-2 py-1 font-mono text-[11px] transition ${
              s === sdk ? "bg-panel-2 text-fg" : "text-subtle hover:text-fg"
            }`}
          >
            {s}
          </button>
        ))}
        <span className="ml-auto">
          <CopyButton id="snippet" text={full} copied={copied} onCopy={copy} />
        </span>
      </div>
      <pre className="overflow-x-auto px-4 py-3 font-mono text-[12px] leading-relaxed text-muted">
        {reveal ? full : snippet(sdk, info.endpoint, info.region, id, mask(secret), bucket)}
      </pre>
    </div>
  );
}

export function KeysTab({ info, buckets }: { info: StorageInfo; buckets: Bucket[] }) {
  const [keys, setKeys] = useState<AccessKey[] | null>(null);
  const [loadErr, setLoadErr] = useState<string | null>(null);
  const refresh = useCallback(async () => {
    try {
      setKeys(await storageApi.keys());
      setLoadErr(null);
    } catch (e) {
      setLoadErr(e instanceof Error ? e.message : String(e));
    }
  }, []);
  useEffect(() => {
    refresh();
  }, [refresh]);
  const { busy, error, run } = useAction(refresh);
  const [created, setCreated] = useState<CreatedKey | null>(null);
  const [confirming, setConfirming] = useState<string | null>(null);

  return (
    <div className="space-y-4">
      <ErrorNote error={error ?? loadErr} />
      {created && <NewKeySecret info={info} k={created} onDone={() => setCreated(null)} />}
      <div className="grid gap-4 lg:grid-cols-5">
        <Panel
          title="Access keys"
          aside={<span className="font-mono text-[12px] text-subtle">{keys?.length ?? "—"}</span>}
          className="lg:col-span-3"
        >
          {keys === null ? (
            <p className="px-5 py-6 text-[13px] text-subtle">
              <span className="shimmer">Loading keys…</span>
            </p>
          ) : keys.length === 0 ? (
            <p className="px-5 py-6 text-[13px] text-muted">
              No access keys yet. S3 clients sign every request with an access key id and secret (SigV4); create one to
              connect.
            </p>
          ) : (
            <ul>
              {keys.map((k) => (
                <li key={k.id} className="rise border-b border-line px-5 py-3 last:border-0">
                  <div className="flex items-center gap-3">
                    <KeyRound className="size-4 shrink-0 text-subtle" />
                    <div className="min-w-0 flex-1">
                      <p className="flex items-center gap-2 truncate text-[13px]">
                        {k.name}
                        <span className="rounded-md border border-line px-1.5 py-px font-mono text-[10px] text-muted">
                          {k.permission}
                        </span>
                      </p>
                      <p className="truncate font-mono text-[11px] text-subtle">
                        {k.id} · {k.buckets ? k.buckets.join(", ") : "all buckets"} ·{" "}
                        {k.last_used_at ? `used ${agoMs(k.last_used_at)}` : "never used"}
                      </p>
                    </div>
                    {confirming === k.id ? (
                      <span className="flex shrink-0 gap-1.5">
                        <button
                          disabled={busy !== null}
                          onClick={async () => {
                            await run(`del-${k.id}`, () => storageApi.deleteKey(k.id));
                            setConfirming(null);
                          }}
                          className="inline-flex items-center gap-1.5 rounded-lg bg-danger px-2.5 py-1.5 text-[12px] font-medium text-white transition hover:opacity-85 disabled:opacity-40"
                        >
                          {busy === `del-${k.id}` ? <span className="shimmer">Revoking…</span> : "Revoke"}
                        </button>
                        <button onClick={() => setConfirming(null)} className={btn}>
                          Cancel
                        </button>
                      </span>
                    ) : (
                      <button
                        onClick={() => setConfirming(k.id)}
                        className="rounded-md p-1.5 text-subtle transition hover:bg-panel-2 hover:text-danger"
                        title="Revoke key"
                        aria-label={`Revoke ${k.name}`}
                      >
                        <Trash2 className="size-3.5" />
                      </button>
                    )}
                  </div>
                </li>
              ))}
            </ul>
          )}
        </Panel>
        <CreateKey
          buckets={buckets}
          busy={busy === "create"}
          onCreate={async (name, permission, scope) => {
            const k = await run("create", () => storageApi.createKey(name, permission, scope));
            if (k) setCreated(k);
            return !!k;
          }}
        />
      </div>
    </div>
  );
}

function CreateKey({
  buckets,
  busy,
  onCreate,
}: {
  buckets: Bucket[];
  busy: boolean;
  onCreate: (name: string, permission: Permission, buckets: string[] | null) => Promise<boolean>;
}) {
  const [name, setName] = useState("");
  const [permission, setPermission] = useState<Permission>("write");
  const [scoped, setScoped] = useState(false);
  const [picked, setPicked] = useState<string[]>([]);
  const valid = name.trim().length > 0 && name.length <= 64 && (!scoped || picked.length > 0);
  return (
    <Panel title="New key" className="lg:col-span-2">
      <form
        className="space-y-4 px-5 py-4"
        onSubmit={async (e) => {
          e.preventDefault();
          if (!valid) return;
          if (await onCreate(name.trim(), permission, scoped ? picked : null)) {
            setName("");
            setPicked([]);
            setScoped(false);
          }
        }}
      >
        <Field label="Name">
          <input
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="web-app"
            spellCheck={false}
            className={`${input} w-full font-mono text-[12px]`}
          />
        </Field>
        <Field label="Permission" hint={PERMISSIONS.find((p) => p.value === permission)?.hint}>
          <Segmented value={permission} options={PERMISSIONS.map((p) => ({ label: p.label, value: p.value }))} onChange={setPermission} />
        </Field>
        <Field label="Buckets">
          <Segmented
            value={scoped ? "some" : "all"}
            options={[
              { label: "all buckets", value: "all" },
              { label: "only some", value: "some" },
            ]}
            onChange={(v) => setScoped(v === "some")}
          />
        </Field>
        {scoped &&
          (buckets.length === 0 ? (
            <p className="text-[12px] text-subtle">No buckets to pick yet.</p>
          ) : (
            <div className="max-h-40 space-y-1 overflow-y-auto rounded-lg border border-line bg-bg p-2">
              {buckets.map((b) => (
                <label key={b.id} className="flex cursor-pointer items-center gap-2 rounded px-1.5 py-1 text-[12px] hover:bg-panel-2">
                  <input
                    type="checkbox"
                    checked={picked.includes(b.name)}
                    onChange={(e) =>
                      setPicked((p) => (e.target.checked ? [...p, b.name] : p.filter((x) => x !== b.name)))
                    }
                    className="accent-accent"
                  />
                  <span className="font-mono">{b.name}</span>
                </label>
              ))}
            </div>
          ))}
        <button type="submit" disabled={busy || !valid} className={primary}>
          {busy ? (
            <span className="shimmer">Creating…</span>
          ) : (
            <>
              <Plus className="size-3.5" /> Create key
            </>
          )}
        </button>
      </form>
    </Panel>
  );
}

/** The secret is only shown here, once. */
function NewKeySecret({ info, k, onDone }: { info: StorageInfo; k: CreatedKey; onDone: () => void }) {
  const { copied, copy } = useCopy();
  const env = `AWS_ACCESS_KEY_ID=${k.id}\nAWS_SECRET_ACCESS_KEY=${k.secret_access_key}\nAWS_ENDPOINT_URL=${info.endpoint}\nAWS_REGION=${info.region}\n`;
  return (
    <section className="rise rounded-xl border border-accent/40 bg-panel">
      <div className="flex min-h-14 flex-wrap items-center justify-between gap-3 border-b border-line px-5 py-3">
        <div>
          <h2 className="text-[15px] font-medium">Key {k.name} created</h2>
          <p className="text-[12px] text-muted">Copy the secret now — it isn&apos;t shown again.</p>
        </div>
        <button onClick={onDone} className={btn}>
          Done
        </button>
      </div>
      <div className="grid gap-4 px-5 py-4 lg:grid-cols-2">
        <div className="space-y-3">
          <Row label="Access key id" value={k.id} id="kid" copied={copied} onCopy={copy} />
          <Row label="Secret access key" value={k.secret_access_key} id="secret" copied={copied} onCopy={copy} />
          <Row label="Endpoint" value={info.endpoint} id="endpoint" copied={copied} onCopy={copy} />
          <button onClick={() => copy("env", env)} className={btn}>
            {copied === "env" ? "Copied .env" : "Copy as .env"}
          </button>
        </div>
        <SdkSnippet info={info} id={k.id} secret={k.secret_access_key} bucket={k.buckets?.[0]} />
      </div>
    </section>
  );
}

function Row({
  label,
  value,
  id,
  copied,
  onCopy,
}: {
  label: string;
  value: string;
  id: string;
  copied: string | null;
  onCopy: (key: string, text: string) => void;
}) {
  return (
    <div className="space-y-1">
      <p className="text-[11px] text-subtle">{label}</p>
      <div className="flex items-center gap-1 rounded-lg border border-line bg-bg px-2.5 py-2">
        <code className="min-w-0 flex-1 truncate font-mono text-[11px]">{value}</code>
        <CopyButton id={id} text={value} copied={copied} onCopy={onCopy} />
      </div>
    </div>
  );
}
