"use client";

import { Globe, ShieldCheck } from "lucide-react";
import { useState } from "react";
import {
  effectiveBlock,
  NO_BLOCK,
  policyError,
  publicReadPolicy,
  storageApi,
  type Bucket,
  type PublicAccessBlock,
  type StorageInfo,
} from "@/lib/storage-api";
import { btn, ErrorNote, input, Panel } from "../ui";
import { Check, SaveBar, useAction } from "./shared";

const BLOCKS: { key: keyof PublicAccessBlock; label: string; hint: string }[] = [
  { key: "block_public_acls", label: "Block public ACLs", hint: "refuse new ACLs that grant access to everyone" },
  { key: "ignore_public_acls", label: "Ignore public ACLs", hint: "existing public ACLs grant nothing" },
  { key: "block_public_policy", label: "Block public policies", hint: "refuse policies (and the public flag) that let anyone in" },
  { key: "restrict_public_buckets", label: "Restrict public buckets", hint: "a public policy only lets in keys of this deployment" },
];

/** S3's Block Public Access for one bucket; the deployment-wide block, when on, is shown on top. */
export function PublicAccessBlockPanel({
  bucket,
  info,
  onChanged,
}: {
  bucket: Bucket;
  info: StorageInfo;
  onChanged: () => Promise<unknown>;
}) {
  const own = bucket.public_access_block;
  const [draft, setDraft] = useState<PublicAccessBlock>(own ?? NO_BLOCK);
  const { busy, error, run } = useAction(onChanged);
  const forced = info.block_public_access;
  const shown = forced ? effectiveBlock(draft, true) : draft;
  const dirty = JSON.stringify(draft) !== JSON.stringify(own ?? NO_BLOCK);
  return (
    <Panel
      title="Public access block"
      aside={
        forced ? (
          <span className="flex items-center gap-1.5 text-[12px] text-subtle">
            <ShieldCheck className="size-3.5" /> blocked for the whole deployment
          </span>
        ) : (
          <span className="text-[12px] text-subtle">{own ? "set on this bucket" : "not set"}</span>
        )
      }
    >
      <form
        className="space-y-4 px-5 py-4"
        onSubmit={(e) => {
          e.preventDefault();
          run("block", () => storageApi.setPublicAccessBlock(bucket.name, draft));
        }}
      >
        <ErrorNote error={error} />
        <div className="grid gap-3 sm:grid-cols-2">
          {BLOCKS.map((b) => (
            <Check
              key={b.key}
              checked={shown[b.key]}
              disabled={forced}
              onChange={(v) => setDraft((d) => ({ ...d, [b.key]: v }))}
              label={b.label}
              hint={b.hint}
            />
          ))}
        </div>
        {forced && (
          <p className="text-[12px] text-subtle">
            rolld runs with <span className="font-mono">ROLL_S3_BLOCK_PUBLIC_ACCESS</span>: every bucket is blocked, whatever is set here.
          </p>
        )}
        {!forced && own && !dirty && (
          <button
            type="button"
            disabled={busy !== null}
            onClick={() => run("unblock", () => storageApi.setPublicAccessBlock(bucket.name, null))}
            className={btn}
          >
            {busy === "unblock" ? <span className="shimmer">Removing…</span> : "Remove the bucket's block"}
          </button>
        )}
        {!forced && <SaveBar dirty={dirty} busy={busy === "block"} onReset={() => setDraft(own ?? NO_BLOCK)} />}
      </form>
    </Panel>
  );
}

const pretty = (p: unknown) => (p ? JSON.stringify(p, null, 2) : "");

/** The bucket policy as JSON, validated in the browser for syntax and by the server for meaning. */
export function PolicyPanel({ bucket, info, onChanged }: { bucket: Bucket; info: StorageInfo; onChanged: () => Promise<unknown> }) {
  const saved = pretty(bucket.policy);
  const [text, setText] = useState(saved);
  const [saving, setSaving] = useState(false);
  const [serverError, setServerError] = useState<string | null>(null);
  const localError = policyError(text);
  const dirty = text.trim() !== saved.trim();
  const removing = dirty && !text.trim();

  const save = async () => {
    if (localError) return;
    setSaving(true);
    setServerError(null);
    try {
      await storageApi.setPolicy(bucket.name, text.trim() ? (JSON.parse(text) as Record<string, unknown>) : null);
      await onChanged();
    } catch (e) {
      setServerError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  };

  return (
    <Panel
      title="Bucket policy"
      aside={
        bucket.policy ? (
          bucket.policy_public ? (
            <span className="flex items-center gap-1.5 text-[12px] text-muted">
              <Globe className="size-3.5" /> grants public access
            </span>
          ) : (
            <span className="text-[12px] text-subtle">not public</span>
          )
        ) : (
          <span className="text-[12px] text-subtle">none</span>
        )
      }
    >
      <form
        className="space-y-3 px-5 py-4"
        onSubmit={(e) => {
          e.preventDefault();
          save();
        }}
      >
        <p className="text-[13px] text-muted">
          An IAM resource policy, as on S3. Principals are roll keys and roles: account{" "}
          <span className="font-mono text-[12px] text-fg">{info.account_id}</span>, e.g.{" "}
          <span className="font-mono text-[12px]">arn:aws:iam::{info.account_id}:role/&lt;name&gt;</span>.
        </p>
        <textarea
          value={text}
          onChange={(e) => {
            setText(e.target.value);
            setServerError(null);
          }}
          rows={Math.min(18, Math.max(6, text.split("\n").length + 1))}
          spellCheck={false}
          placeholder={'{\n  "Version": "2012-10-17",\n  "Statement": [ … ]\n}'}
          aria-label="Bucket policy JSON"
          aria-invalid={!!(localError || serverError)}
          className={`${input} block w-full resize-y font-mono text-[12px] leading-relaxed ${
            localError || serverError ? "border-danger/50 focus:border-danger/70" : ""
          }`}
        />
        {(localError || serverError) && <p className="rise text-[12px] text-danger">{localError ?? serverError}</p>}
        {!text.trim() && !bucket.policy && (
          <button type="button" onClick={() => setText(pretty(publicReadPolicy(bucket.name)))} className={btn}>
            Start from public read
          </button>
        )}
        <SaveBar
          dirty={dirty}
          busy={saving}
          onReset={() => {
            setText(saved);
            setServerError(null);
          }}
          label={removing ? "Remove policy" : "Save policy"}
        />
      </form>
    </Panel>
  );
}
