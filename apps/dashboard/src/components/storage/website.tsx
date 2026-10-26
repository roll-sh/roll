"use client";

import { ExternalLink } from "lucide-react";
import { useState } from "react";
import { indexDocumentError, storageApi, websiteUrlFor, type Bucket, type StorageInfo } from "@/lib/storage-api";
import { btn, ErrorNote, input, Panel, primary } from "../ui";
import { Field, SaveBar, useAction } from "./shared";

/** Static website hosting: index and error documents, and where the site is served. */
export function WebsitePanel({ bucket, info, onChanged }: { bucket: Bucket; info: StorageInfo; onChanged: () => Promise<unknown> }) {
  const w = bucket.website;
  const savedIndex = w?.index_document ?? "";
  const savedError = w?.error_document ?? "";
  const [index, setIndex] = useState(savedIndex || "index.html");
  const [errorDoc, setErrorDoc] = useState(savedError);
  const { busy, error, run } = useAction(onChanged);
  const on = w !== null;
  const dirty = on && (index !== savedIndex || errorDoc !== savedError);
  const invalid = indexDocumentError(index);
  const url = bucket.website_url ?? (on ? null : websiteUrlFor(bucket.name, info));
  const readable = bucket.public || bucket.policy_public;
  const rules = w?.routing_rules.length ?? 0;

  return (
    <Panel
      title="Website"
      aside={<span className="text-[12px] text-subtle">{on ? (w?.redirect_all ? "redirecting" : "on") : "off"}</span>}
    >
      <form
        className="space-y-4 px-5 py-4"
        onSubmit={(e) => {
          e.preventDefault();
          if (!invalid) run("website", () => storageApi.setWebsite(bucket.name, { index_document: index.trim(), error_document: errorDoc.trim() || null }));
        }}
      >
        <ErrorNote error={error} />
        {info.website_domain ? (
          url && (
            <p className="text-[13px] text-muted">
              {on ? "Served at " : "Once on, served at "}
              {on ? (
                <a href={url} target="_blank" rel="noreferrer" className="inline-flex items-center gap-1 font-mono text-[12px] text-accent hover:underline">
                  {url} <ExternalLink className="size-3" />
                </a>
              ) : (
                <span className="font-mono text-[12px] text-fg">{url}</span>
              )}
              .
            </p>
          )
        ) : (
          <p className="text-[13px] text-muted">
            This deployment has no website domain: set <span className="font-mono text-[12px]">ROLL_S3_WEBSITE_DOMAIN</span> on rolld to
            serve buckets at <span className="font-mono text-[12px]">&lt;bucket&gt;.&lt;domain&gt;</span>. The configuration is kept either way.
          </p>
        )}
        {w?.redirect_all && (
          <p className="text-[12px] text-muted">
            Every request redirects to <span className="font-mono">{w.redirect_all.host_name}</span> (set through the S3 API). Saving
            documents here replaces the redirect.
          </p>
        )}
        <div className="grid gap-3 sm:grid-cols-2">
          <Field label="Index document" hint="served for / and every dir/">
            <input value={index} onChange={(e) => setIndex(e.target.value)} placeholder="index.html" spellCheck={false} className={`${input} w-full font-mono text-[12px]`} />
          </Field>
          <Field label="Error document" hint={errorDoc ? "served with the error's status" : "S3's error page"}>
            <input value={errorDoc} onChange={(e) => setErrorDoc(e.target.value)} placeholder="404.html" spellCheck={false} className={`${input} w-full font-mono text-[12px]`} />
          </Field>
        </div>
        {rules > 0 && (
          <p className="font-mono text-[11px] text-subtle" title="Set through the S3 API; kept as is">
            also: {rules} routing rule{rules === 1 ? "" : "s"}
          </p>
        )}
        {!readable && (
          <p className="text-[12px] text-subtle">
            Only objects anyone may read are served: make the bucket public, or allow <span className="font-mono">s3:GetObject</span> in
            its policy.
          </p>
        )}
        {invalid && <p className="text-[12px] text-danger">{invalid}</p>}
        {on && !dirty && (
          <button type="button" disabled={busy !== null} onClick={() => run("website-off", () => storageApi.setWebsite(bucket.name, null))} className={btn}>
            {busy === "website-off" ? <span className="shimmer">Turning off…</span> : "Turn off website"}
          </button>
        )}
        {on ? (
          <SaveBar
            dirty={dirty}
            busy={busy === "website"}
            onReset={() => {
              setIndex(savedIndex);
              setErrorDoc(savedError);
            }}
          />
        ) : (
          <div className="border-t border-line pt-4">
            <button type="submit" disabled={busy !== null || !!invalid} className={primary}>
              {busy === "website" ? <span className="shimmer">Turning on…</span> : "Turn on website"}
            </button>
          </div>
        )}
      </form>
    </Panel>
  );
}
