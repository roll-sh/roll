"use client";

import { Minus, Plus, X } from "lucide-react";
import Link from "next/link";
import { useEffect, useState } from "react";
import { codeApi, shortId, type Repo, type ReviewRules, type ReviewSettings } from "@/lib/code-api";
import { clampApprovals, parseGlobs, rulesPatch, tightened } from "@/lib/review";
import { btn, input, Panel, primary } from "../ui";
import type { Run } from "./shared";

const TOGGLES: { key: keyof ReviewRules; label: string; hint: string }[] = [
  { key: "require_codeowners", label: "Code owners must approve", hint: "Each CODEOWNERS pattern a change touches needs one of its owners." },
  { key: "count_ai_approval", label: "AI approvals count", hint: "roll's reviewer and review apps can satisfy the required approvals." },
  { key: "block_on_request_changes", label: "Block on requested changes", hint: "An open request for changes stops it landing." },
  { key: "dismiss_stale", label: "Dismiss stale approvals", hint: "A new revision needs approving again." },
  { key: "require_resolved_threads", label: "Resolve every thread", hint: "Open threads block landing." },
  { key: "allow_self_approval", label: "Allow self-approval", hint: "The author's own approval counts." },
];

/** An on/off switch with the label beside it. */
export function Toggle({ on, onChange, disabled, label, hint }: { on: boolean; onChange: (v: boolean) => void; disabled: boolean; label: string; hint: string }) {
  return (
    <label className={`flex items-start gap-3 ${disabled ? "opacity-60" : "cursor-pointer"}`}>
      <button
        type="button"
        role="switch"
        aria-checked={on}
        disabled={disabled}
        onClick={() => onChange(!on)}
        className={`relative mt-0.5 inline-flex h-4 w-7 shrink-0 rounded-full border transition-colors duration-200 motion-reduce:transition-none ${
          on ? "border-accent bg-accent" : "border-line-2 bg-panel-2"
        }`}
      >
        <span
          className={`absolute top-1/2 size-2.5 -translate-y-1/2 rounded-full bg-fg transition-[left] duration-200 motion-reduce:transition-none ${
            on ? "left-[14px] bg-white" : "left-[2px]"
          }`}
        />
      </button>
      <span className="min-w-0">
        <span className="block text-[13px]">{label}</span>
        <span className="block text-[11px] text-subtle">{hint}</span>
      </span>
    </label>
  );
}

/**
 * The repo's review rules: approvals, code owners, stale reviews, threads and auto-approve. What
 * applies is these settings tightened by `.roll/review.yaml` on the default branch, shown below.
 */
export function ReviewSettingsPanel({ repo, writer, tick, busy, run }: { repo: Repo; writer: boolean; tick: number; busy: string | null; run: Run }) {
  const [data, setData] = useState<ReviewSettings | null>(null);
  const [rules, setRules] = useState<ReviewRules | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [version, setVersion] = useState(0);

  useEffect(() => {
    let alive = true;
    codeApi.reviewSettings(repo.full_name).then(
      (d) => {
        if (!alive) return;
        setData(d);
        setRules(d.settings);
        setError(null);
      },
      (e) => alive && setError(e instanceof Error ? e.message : String(e)),
    );
    return () => {
      alive = false;
    };
  }, [repo.full_name, tick, version]);

  if (!data || !rules) {
    return (
      <Panel title="Reviews">
        <p className={`px-5 py-4 text-[12px] ${error ? "text-danger" : "text-subtle"}`}>
          {error ?? <span className="shimmer">Loading review rules…</span>}
        </p>
      </Panel>
    );
  }

  const patch = rulesPatch(data.settings, rules);
  const dirty = Object.keys(patch).length > 0;
  const set = <K extends keyof ReviewRules>(k: K, v: ReviewRules[K]) => setRules((r) => (r ? { ...r, [k]: v } : r));
  const tight = tightened(data.settings, data.effective);
  const disabled = !writer;

  return (
    <Panel
      title="Reviews"
      aside={
        <Link href={`/dashboard/code/${repo.owner}#review-apps`} className="text-[12px] text-subtle transition hover:text-fg">
          Review apps →
        </Link>
      }
    >
      <form
        className="space-y-6 px-5 py-4"
        onSubmit={async (e) => {
          e.preventDefault();
          const out = await run("review-settings", () => codeApi.updateReviewSettings(repo.full_name, patch));
          if (out !== undefined) setVersion((v) => v + 1);
        }}
      >
        <div className="flex flex-wrap items-center gap-4">
          <div>
            <p className="text-[13px]">Required approvals</p>
            <p className="text-[11px] text-subtle">0 lands without review (unless rules below say otherwise).</p>
          </div>
          <div className="ml-auto flex items-center rounded-lg border border-line bg-panel">
            <button
              type="button"
              onClick={() => set("required_approvals", clampApprovals(rules.required_approvals - 1))}
              disabled={disabled || rules.required_approvals <= 0}
              className="p-2 text-subtle transition hover:text-fg disabled:opacity-40"
              aria-label="Fewer approvals"
            >
              <Minus className="size-3.5" />
            </button>
            <span className="w-8 text-center font-mono text-[13px] tabular-nums" aria-live="polite">
              {rules.required_approvals}
            </span>
            <button
              type="button"
              onClick={() => set("required_approvals", clampApprovals(rules.required_approvals + 1))}
              disabled={disabled || rules.required_approvals >= 20}
              className="p-2 text-subtle transition hover:text-fg disabled:opacity-40"
              aria-label="More approvals"
            >
              <Plus className="size-3.5" />
            </button>
          </div>
        </div>

        <div className="grid gap-4 sm:grid-cols-2">
          {TOGGLES.map((t) => (
            <Toggle key={t.key} on={!!rules[t.key]} onChange={(v) => set(t.key, v as never)} disabled={disabled} label={t.label} hint={t.hint} />
          ))}
        </div>

        <div className="space-y-2">
          <div>
            <p className="text-[13px]">Auto-approve</p>
            <p className="text-[11px] text-subtle">Changes that touch only these paths, or only comments and whitespace, need no human review.</p>
          </div>
          {rules.auto_approve.length === 0 && <p className="text-[12px] text-subtle">No rules.</p>}
          <ul className="space-y-2">
            {rules.auto_approve.map((rule, i) => (
              <li key={i} className="flex flex-wrap items-center gap-2">
                <input
                  value={rule.paths.join(", ")}
                  onChange={(e) =>
                    set(
                      "auto_approve",
                      rules.auto_approve.map((r, j) => (j === i ? { ...r, paths: parseGlobs(e.target.value) } : r)),
                    )
                  }
                  disabled={disabled}
                  placeholder="docs/**, **/*.md"
                  spellCheck={false}
                  className={`${input} min-w-0 flex-1 font-mono text-[12px]`}
                />
                <label className="flex items-center gap-1.5 text-[12px] text-muted">
                  <input
                    type="checkbox"
                    checked={rule.comment_only}
                    disabled={disabled}
                    onChange={(e) => set("auto_approve", rules.auto_approve.map((r, j) => (j === i ? { ...r, comment_only: e.target.checked } : r)))}
                    className="accent-[var(--accent)]"
                  />
                  comment-only
                </label>
                {!disabled && (
                  <button
                    type="button"
                    onClick={() => set("auto_approve", rules.auto_approve.filter((_, j) => j !== i))}
                    className="p-1 text-subtle hover:text-fg"
                    aria-label="Remove rule"
                  >
                    <X className="size-3.5" />
                  </button>
                )}
              </li>
            ))}
          </ul>
          {!disabled && (
            <button type="button" onClick={() => set("auto_approve", [...rules.auto_approve, { paths: [], comment_only: false }])} className={btn}>
              <Plus className="size-3.5" /> Add rule
            </button>
          )}
        </div>

        {writer && (
          <div className="flex items-center gap-3">
            <button type="submit" disabled={!dirty || busy !== null} className={primary}>
              {busy === "review-settings" ? <span className="shimmer">Saving…</span> : "Save"}
            </button>
            {dirty && (
              <button type="button" onClick={() => setRules(data.settings)} className="text-[12px] text-subtle hover:text-fg">
                Reset
              </button>
            )}
          </div>
        )}
      </form>

      <div className="space-y-4 border-t border-line px-5 py-4">
        <div>
          <p className="flex items-center gap-2 text-[13px]">
            <span className="font-mono text-[12px]">{data.file?.path ?? ".roll/review.yaml"}</span>
            <span className="font-mono text-[11px] text-subtle">
              {repo.default_branch} @ {shortId(data.at)}
            </span>
          </p>
          <p className="mt-1 text-[11px] text-subtle">
            Read from the default branch, never from the change being reviewed. It can only tighten these settings, not loosen
            them.
          </p>
          {!data.file ? (
            <p className="mt-2 text-[12px] text-subtle">No file; the settings above apply as they are.</p>
          ) : (
            <>
              {data.file.errors.map((e) => (
                <p key={e} className="mt-2 rounded-lg border border-danger/40 bg-danger/10 px-3 py-2 text-[12px] text-danger">
                  {e}
                </p>
              ))}
              {tight.length > 0 ? (
                <p className="mt-2 text-[12px] text-muted">
                  Tightens: <span className="font-mono text-fg">{tight.join(", ")}</span>
                </p>
              ) : (
                <p className="mt-2 text-[12px] text-subtle">Tightens nothing beyond the settings.</p>
              )}
            </>
          )}
          <dl className="mt-3 grid grid-cols-[auto_1fr] gap-x-6 gap-y-1 text-[12px]">
            {(Object.keys(data.effective) as (keyof ReviewRules)[])
              .filter((k) => k !== "auto_approve")
              .map((k) => (
                <div key={k} className="contents">
                  <dt className="font-mono text-subtle">{k}</dt>
                  <dd className={`font-mono ${tight.includes(k) ? "text-accent" : ""}`}>{String(data.effective[k])}</dd>
                </div>
              ))}
            <div className="contents">
              <dt className="font-mono text-subtle">auto_approve</dt>
              <dd className="font-mono">
                {data.effective.auto_approve.length
                  ? data.effective.auto_approve.map((r) => [r.paths.join(", "), r.comment_only ? "comment-only" : ""].filter(Boolean).join(" · ")).join(" | ")
                  : "none"}
              </dd>
            </div>
          </dl>
        </div>

        <div>
          <p className="text-[13px]">
            <span className="font-mono text-[12px]">{data.codeowners.path ?? "CODEOWNERS"}</span>
          </p>
          {!data.codeowners.path ? (
            <p className="mt-1 text-[12px] text-subtle">
              No CODEOWNERS file (looked in <span className="font-mono">CODEOWNERS</span>, <span className="font-mono">.roll/</span>,{" "}
              <span className="font-mono">.github/</span> and <span className="font-mono">docs/</span>).
            </p>
          ) : (
            <>
              {data.codeowners.errors.map((e) => (
                <p key={e} className="mt-2 rounded-lg border border-danger/40 bg-danger/10 px-3 py-2 text-[12px] text-danger">
                  {e}
                </p>
              ))}
              <ul className="mt-2 max-h-56 space-y-0.5 overflow-y-auto font-mono text-[12px]">
                {data.codeowners.rules.map((r) => (
                  <li key={`${r.line}-${r.pattern}`} className="flex gap-3">
                    <span className="w-8 shrink-0 text-right text-subtle tabular-nums">{r.line}</span>
                    <span className="min-w-0 truncate">{r.pattern}</span>
                    <span className="min-w-0 truncate text-muted">{r.owners.join(" ") || "—"}</span>
                  </li>
                ))}
              </ul>
              <p className="mt-1 text-[11px] text-subtle">The last matching pattern wins, as on GitHub.</p>
            </>
          )}
        </div>
      </div>
    </Panel>
  );
}
