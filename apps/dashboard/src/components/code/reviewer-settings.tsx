"use client";

import { Plus, Trash2, X } from "lucide-react";
import Link from "next/link";
import { useEffect, useState } from "react";
import { codeApi, type AiEffort, type Learning, type Repo, type ReviewerView } from "@/lib/code-api";
import { fileKeys, formErrors, formPatch, MODE_HINT, toForm, type ReviewerForm } from "@/lib/ai-review";
import { formatDate } from "@/lib/time";
import { btn, input, Panel, primary } from "../ui";
import { Toggle } from "./review-settings";
import { Field, Segmented, type Run } from "./shared";

const EFFORTS: (AiEffort | "")[] = ["", "low", "medium", "high", "xhigh", "max"];
const area = `${input} block w-full resize-y font-mono text-[12px]`;

/** A note beside a field the repo file sets: the file wins over what's saved here. */
function FromFile({ on }: { on: boolean }) {
  return on ? <span className="ml-1.5 rounded border border-line px-1 py-px font-mono text-[10px] text-subtle">set by .roll/review.yaml</span> : null;
}

/**
 * roll's reviewer for this repo: on or off, depth (auto routing), speed, tone, what to skip, the
 * custom prompt, per-path instructions and rules, model overrides. `.roll/review.yaml`'s
 * `reviewer:` section overlays these; fields it sets are marked.
 */
export function ReviewerPanel({ repo, writer, tick, busy, run }: { repo: Repo; writer: boolean; tick: number; busy: string | null; run: Run }) {
  const [view, setView] = useState<ReviewerView | null>(null);
  const [form, setForm] = useState<ReviewerForm | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [version, setVersion] = useState(0);

  useEffect(() => {
    let alive = true;
    codeApi.reviewer(repo.full_name).then(
      (v) => {
        if (!alive) return;
        setView(v);
        setForm(toForm(v.effective));
        setError(null);
      },
      (e) => alive && setError(e instanceof Error ? e.message : String(e)),
    );
    return () => {
      alive = false;
    };
  }, [repo.full_name, tick, version]);

  if (!view || !form) {
    return (
      <Panel title="roll reviewer">
        <p className={`px-5 py-4 text-[12px] ${error ? "text-danger" : "text-subtle"}`}>{error ?? <span className="shimmer">Loading reviewer…</span>}</p>
      </Panel>
    );
  }

  const fromFile = fileKeys(view);
  const patch = formPatch(form, view.effective);
  const errors = formErrors(form);
  const dirty = Object.keys(patch).length > 0;
  const disabled = !writer;
  const set = <K extends keyof ReviewerForm>(k: K, v: ReviewerForm[K]) => setForm((f) => (f ? { ...f, [k]: v } : f));
  const e = view.effective;

  return (
    <Panel
      title="roll reviewer"
      aside={
        <Link href={`/dashboard/code/${repo.owner}#billing`} className="text-[12px] text-subtle transition hover:text-fg">
          Usage & provider →
        </Link>
      }
    >
      <form
        className="space-y-6 px-5 py-4"
        onSubmit={async (ev) => {
          ev.preventDefault();
          if (Object.keys(errors).length) return;
          const out = await run("reviewer", () => codeApi.updateReviewer(repo.full_name, patch));
          if (out !== undefined) setVersion((v) => v + 1);
        }}
      >
        <div className="grid gap-4 sm:grid-cols-2">
          <Toggle on={form.enabled} onChange={(v) => set("enabled", v)} disabled={disabled} label="Review changes with roll" hint="Off by default. Fork changes are never reviewed on their own." />
          <Toggle on={form.auto_review} onChange={(v) => set("auto_review", v)} disabled={disabled || !form.enabled} label="Review automatically" hint="On open and every new revision (only what changed)." />
          <Toggle on={form.drafts} onChange={(v) => set("drafts", v)} disabled={disabled || !form.enabled} label="Review drafts" hint="Otherwise roll waits until a change is ready." />
          <Toggle on={form.required} onChange={(v) => set("required", v)} disabled={disabled || !form.enabled} label="Required" hint="Changes wait for roll's review of their head before landing." />
        </div>

        <div className="space-y-2">
          <p className="text-[13px]">
            Depth <FromFile on={fromFile.has("mode")} />
          </p>
          <Segmented
            value={form.mode}
            options={[
              { label: "Auto", value: "auto" },
              { label: "Standard", value: "standard" },
              { label: "Deep", value: "deep" },
              { label: "Max", value: "max" },
            ]}
            onChange={(v) => !disabled && set("mode", v)}
          />
          <p className="text-[11px] text-subtle">{MODE_HINT[form.mode]}</p>
        </div>

        {form.mode === "auto" && (
          <div className="rise grid gap-4 rounded-lg border border-line p-4 sm:grid-cols-2">
            <Field label="Deep for these paths" hint="One glob per line">
              <textarea value={form.deep_paths} onChange={(ev) => set("deep_paths", ev.target.value)} disabled={disabled} rows={3} spellCheck={false} placeholder="crates/*/src/auth/**" className={area} />
            </Field>
            <Field label="Max for these paths" hint="One glob per line">
              <textarea value={form.max_paths} onChange={(ev) => set("max_paths", ev.target.value)} disabled={disabled} rows={3} spellCheck={false} placeholder="**/crypto/**" className={area} />
            </Field>
            <Field label="Standard below (changed lines)" hint={errors.standard_below_lines}>
              <input value={form.standard_below_lines} onChange={(ev) => set("standard_below_lines", ev.target.value)} disabled={disabled} inputMode="numeric" className={`${input} w-full font-mono`} />
            </Field>
            <Field label="How auto decides">
              <Segmented
                value={form.classifier}
                options={[
                  { label: "Heuristics", value: "heuristic" },
                  { label: "Quick model call", value: "model" },
                ]}
                onChange={(v) => !disabled && set("classifier", v)}
              />
            </Field>
          </div>
        )}

        <div className="grid gap-4 sm:grid-cols-2">
          <div className="space-y-2">
            <p className="text-[13px]">
              Speed <FromFile on={fromFile.has("speed")} />
            </p>
            <Segmented
              value={form.speed}
              options={[
                { label: "Fast", value: "fast" },
                { label: "Balanced", value: "balanced" },
                { label: "Thorough", value: "thorough" },
              ]}
              onChange={(v) => !disabled && set("speed", v)}
            />
            <p className="text-[11px] text-subtle">
              {form.speed === "fast"
                ? "A faster model, files in parallel, comments as each file finishes."
                : form.speed === "thorough"
                  ? "Higher effort, a verification pass and wider context."
                  : "The default balance of depth and latency."}
            </p>
          </div>
          <div className="space-y-2">
            <p className="text-[13px]">
              Tone <FromFile on={fromFile.has("profile")} />
            </p>
            <Segmented
              value={form.profile}
              options={[
                { label: "Chill", value: "chill" },
                { label: "Assertive", value: "assertive" },
              ]}
              onChange={(v) => !disabled && set("profile", v)}
            />
            <p className="text-[11px] text-subtle">
              {form.profile === "assertive" ? "Requests changes for bugs and security findings." : "Comments, and only approves or requests changes when it's clear-cut."}
            </p>
          </div>
        </div>

        <Field label="Custom prompt" hint={`Added after roll's own instructions, inside the cached prefix, so it doesn't slow reviews down.${form.prompt_file ? "" : " Or keep it in the repo with a prompt file."}`}>
          <textarea
            value={form.prompt}
            onChange={(ev) => set("prompt", ev.target.value)}
            disabled={disabled}
            rows={4}
            placeholder="We care most about data loss and tenancy bugs. Be terse."
            className={`${input} block w-full resize-y text-[13px]`}
          />
        </Field>
        <Field label="Prompt file" hint="A path in the repo, read at the default branch's head (e.g. .roll/review-prompt.md)">
          <input value={form.prompt_file} onChange={(ev) => set("prompt_file", ev.target.value)} disabled={disabled} spellCheck={false} placeholder=".roll/review-prompt.md" className={`${input} w-full font-mono text-[12px]`} />
        </Field>

        <div className="space-y-2">
          <p className="text-[13px]">Per-path instructions</p>
          {form.instructions.map((ins, i) => (
            <div key={i} className="flex items-start gap-2">
              <input
                value={ins.path}
                onChange={(ev) => set("instructions", form.instructions.map((x, j) => (j === i ? { ...x, path: ev.target.value } : x)))}
                disabled={disabled}
                placeholder="crates/**/*.rs"
                spellCheck={false}
                className={`${input} w-48 shrink-0 font-mono text-[12px]`}
              />
              <textarea
                value={ins.text}
                onChange={(ev) => set("instructions", form.instructions.map((x, j) => (j === i ? { ...x, text: ev.target.value } : x)))}
                disabled={disabled}
                rows={1}
                placeholder="Never size an allocation from a peer-sent count."
                className={`${input} min-w-0 flex-1 resize-y text-[12px]`}
              />
              <button type="button" onClick={() => set("instructions", form.instructions.filter((_, j) => j !== i))} disabled={disabled} className="p-2 text-subtle hover:text-fg" aria-label="Remove instruction">
                <X className="size-3.5" />
              </button>
            </div>
          ))}
          {errors.instructions && <p className="text-[11px] text-danger">{errors.instructions}</p>}
          <button type="button" onClick={() => set("instructions", [...form.instructions, { path: "", text: "" }])} disabled={disabled} className={btn}>
            <Plus className="size-3.5" /> Add instruction
          </button>
        </div>

        <div className="grid gap-4 sm:grid-cols-2">
          <Field label="Rules every change must follow" hint="One per line; each violation is a finding">
            <textarea value={form.rules} onChange={(ev) => set("rules", ev.target.value)} disabled={disabled} rows={4} placeholder="Every new HTTP route needs a test in tests/api.rs" className={`${input} block w-full resize-y text-[12px]`} />
          </Field>
          <Field label="Ignore" hint="One glob per line; these files are never reviewed">
            <textarea value={form.ignore} onChange={(ev) => set("ignore", ev.target.value)} disabled={disabled} rows={4} spellCheck={false} placeholder={"**/*.lock\nvendor/**"} className={area} />
          </Field>
        </div>

        <details className="group rounded-lg border border-line">
          <summary className="cursor-pointer px-4 py-2.5 text-[12px] text-muted hover:text-fg">Model and limits</summary>
          <div className="grid gap-4 border-t border-line p-4 sm:grid-cols-2">
            <Field label="Model" hint={`Default: ${view.provider?.model ?? "the provider's"}`}>
              <input value={form.model} onChange={(ev) => set("model", ev.target.value)} disabled={disabled} spellCheck={false} placeholder="provider default" className={`${input} w-full font-mono text-[12px]`} />
            </Field>
            <Field label="Fast and classifier model">
              <input value={form.secondary_model} onChange={(ev) => set("secondary_model", ev.target.value)} disabled={disabled} spellCheck={false} placeholder="provider default" className={`${input} w-full font-mono text-[12px]`} />
            </Field>
            <Field label="Effort" hint="Empty: chosen from the speed">
              <select value={form.effort ?? ""} onChange={(ev) => set("effort", ev.target.value as AiEffort | "")} disabled={disabled} className={`${input} w-full`}>
                {EFFORTS.map((x) => (
                  <option key={x} value={x}>
                    {x || "from speed"}
                  </option>
                ))}
              </select>
            </Field>
            <Field label="Max files per review" hint={errors.max_files}>
              <input value={form.max_files} onChange={(ev) => set("max_files", ev.target.value)} disabled={disabled} inputMode="numeric" className={`${input} w-full font-mono`} />
            </Field>
            <div className="sm:col-span-2">
              <Toggle on={form.diagram} onChange={(v) => set("diagram", v)} disabled={disabled} label="Sequence diagrams" hint="A mermaid diagram in the walkthrough when control flow changed." />
            </div>
          </div>
        </details>

        {view.errors.length > 0 && (
          <ul className="space-y-1 rounded-lg border border-danger/30 px-3 py-2 text-[12px] text-danger">
            {view.errors.map((x) => (
              <li key={x}>{x}</li>
            ))}
          </ul>
        )}

        <div className="flex flex-wrap items-center gap-3 border-t border-line pt-4">
          {writer && (
            <button type="submit" disabled={!dirty || busy !== null || Object.keys(errors).length > 0} className={primary}>
              {busy === "reviewer" ? <span className="shimmer">Saving…</span> : "Save reviewer"}
            </button>
          )}
          {dirty && (
            <button type="button" onClick={() => setForm(toForm(e))} className="text-[12px] text-subtle hover:text-fg">
              Reset
            </button>
          )}
          <span className="ml-auto text-[11px] text-subtle">
            {view.provider ? `${view.provider.source === "byok" ? "Your key" : "Hosted"} · ${view.provider.provider} · ${view.provider.model}` : "No model provider configured"}
            {view.index?.stats && ` · index: ${view.index.stats.files.toLocaleString()} files, ${view.index.stats.symbols.toLocaleString()} symbols`}
          </span>
        </div>

        {view.file && (
          <details>
            <summary className="cursor-pointer text-[12px] text-subtle hover:text-fg">.roll/review.yaml · reviewer</summary>
            <pre className="mt-2 overflow-x-auto rounded-lg border border-line bg-panel-2/50 p-3 font-mono text-[11px] text-muted">{JSON.stringify(view.file, null, 2)}</pre>
          </details>
        )}
      </form>
    </Panel>
  );
}

/** What roll has learned about this repo: from `@roll don't flag …`, resolved threads and dismissals. */
export function LearningsPanel({ repo, writer, tick, busy, run }: { repo: Repo; writer: boolean; tick: number; busy: string | null; run: Run }) {
  const [items, setItems] = useState<Learning[] | null>(null);
  const [text, setText] = useState("");
  const [path, setPath] = useState("");
  const [version, setVersion] = useState(0);

  useEffect(() => {
    let alive = true;
    codeApi.learnings(repo.full_name).then(
      (l) => alive && setItems(l),
      () => alive && setItems([]),
    );
    return () => {
      alive = false;
    };
  }, [repo.full_name, tick, version]);

  return (
    <Panel title="Learnings" aside={<span className="text-[11px] text-subtle">fed into every review</span>}>
      <div className="space-y-3 px-5 py-4">
        {items === null ? (
          <p className="text-[12px] text-subtle">
            <span className="shimmer">Loading…</span>
          </p>
        ) : items.length === 0 ? (
          <p className="text-[12px] text-subtle">
            Nothing yet. Reply <span className="font-mono">@roll don&apos;t flag …</span> in a thread, or resolve one of roll&apos;s comments with a reason.
          </p>
        ) : (
          <ul className="divide-y divide-line">
            {items.map((l) => (
              <li key={l.id} className="flex items-start gap-3 py-2">
                <div className="min-w-0 flex-1">
                  <p className="text-[13px]">{l.text}</p>
                  <p className="mt-0.5 text-[11px] text-subtle">
                    {l.path ? <span className="font-mono">{l.path}</span> : "whole repo"} · {l.source} · {l.created_by} · {formatDate(l.created_at)}
                  </p>
                </div>
                {writer && (
                  <button
                    type="button"
                    onClick={async () => {
                      if ((await run(`learning-${l.id}`, () => codeApi.deleteLearning(repo.full_name, l.id))) !== undefined) setVersion((v) => v + 1);
                    }}
                    disabled={busy !== null}
                    className="p-1.5 text-subtle transition hover:text-fg"
                    aria-label="Forget this learning"
                  >
                    <Trash2 className="size-3.5" />
                  </button>
                )}
              </li>
            ))}
          </ul>
        )}
        {writer && (
          <form
            className="flex flex-wrap items-center gap-2 border-t border-line pt-3"
            onSubmit={async (ev) => {
              ev.preventDefault();
              if (!text.trim()) return;
              const out = await run("learning", () => codeApi.addLearning(repo.full_name, { text: text.trim(), path: path.trim() || undefined }));
              if (out !== undefined) {
                setText("");
                setPath("");
                setVersion((v) => v + 1);
              }
            }}
          >
            <input value={text} onChange={(ev) => setText(ev.target.value)} placeholder="Don't flag unwrap() in tests" className={`${input} min-w-0 flex-1 text-[12px]`} />
            <input value={path} onChange={(ev) => setPath(ev.target.value)} placeholder="path glob (optional)" spellCheck={false} className={`${input} w-44 font-mono text-[12px]`} />
            <button type="submit" disabled={!text.trim() || busy !== null} className={btn}>
              <Plus className="size-3.5" /> Add
            </button>
          </form>
        )}
      </div>
    </Panel>
  );
}
