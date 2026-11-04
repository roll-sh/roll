"use client";

import { ArrowUp, ChevronDown, ImagePlus, ListChecks, Square, X } from "lucide-react";
import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import type {
  Attachment,
  InteractionMode,
  Machine,
  ProviderInfo,
  Project,
  RuntimeMode,
  SendMode,
  Thread,
} from "@/lib/agents-api";
import { btn, input } from "../ui";
import { useAgents } from "./use-agents";

/** What a new thread starts with (the `thread.create` fields the composer picks). */
export interface NewThreadFields {
  provider: string;
  model?: string;
  effort?: string;
  runtime_mode: RuntimeMode;
  interaction_mode: InteractionMode;
  worktree: boolean;
  prompt: string;
  attachments?: Attachment[];
}

export type ComposerMode = { kind: "new"; project: Project; machine: Machine } | { kind: "thread"; thread: Thread; machine: Machine | undefined };

/** Images only, at most 5 MiB each (agent-proto `Attachment`). */
export const MAX_ATTACHMENT_BYTES = 5 * 1024 * 1024;

const runtimeModes: { id: RuntimeMode; label: string; hint: string }[] = [
  { id: "full_access", label: "Full access", hint: "Runs commands and edits files without asking" },
  { id: "auto_edit", label: "Auto edit", hint: "Edits in the workspace freely; asks before commands" },
  { id: "ask", label: "Ask", hint: "Asks before everything" },
];

const draftKey = (k: string) => `roll:agents:draft:${k}`;

/** The default provider: the first logged-in one, else the first installed. */
export function defaultProvider(providers: readonly ProviderInfo[]): ProviderInfo | undefined {
  return providers.find((p) => p.installed && p.auth === "ready") ?? providers.find((p) => p.installed);
}

export function readAsAttachment(file: File): Promise<Attachment> {
  return new Promise((resolve, reject) => {
    if (!file.type.startsWith("image/")) return reject(new Error(`${file.name} isn't an image`));
    if (file.size > MAX_ATTACHMENT_BYTES) return reject(new Error(`${file.name} is over 5 MB`));
    const r = new FileReader();
    r.onerror = () => reject(r.error ?? new Error("couldn't read the file"));
    r.onload = () => {
      const url = String(r.result);
      resolve({ name: file.name || "pasted.png", mime: file.type, data: url.slice(url.indexOf(",") + 1) });
    };
    r.readAsDataURL(file);
  });
}

/**
 * The composer, T3 Code's behaviour on the canvas: Enter sends and Shift+Enter breaks the line;
 * while a run is going a message is queued (or steers it; ⌘/Ctrl+Enter does the other one once);
 * Shift+Tab toggles plan mode; images paste or drop in. Provider, model, effort and modes come
 * from the machine's providers. On a thread, changing them reconfigures the thread's next runs.
 */
export function Composer({
  mode,
  onSubmit,
  onCancel,
  autoFocus = true,
}: {
  mode: ComposerMode;
  /** New-thread mode: create it. */
  onSubmit?: (fields: NewThreadFields) => Promise<unknown>;
  onCancel?: () => void;
  autoFocus?: boolean;
}) {
  const { client } = useAgents();
  const providers = useMemo(() => (mode.machine?.info?.providers ?? []).filter((p) => p.installed), [mode.machine]);
  const thread = mode.kind === "thread" ? mode.thread : null;
  const key = thread ? thread.id : `new:${mode.kind === "new" ? mode.project.id : ""}`;

  const [text, setText] = useState("");
  const [attachments, setAttachments] = useState<Attachment[]>([]);
  const [provider, setProvider] = useState<string>(() => thread?.provider ?? defaultProvider(providers)?.id ?? "");
  const [model, setModel] = useState<string | null>(thread?.model ?? null);
  const [effort, setEffort] = useState<string | null>(thread?.effort ?? null);
  const [runtime, setRuntime] = useState<RuntimeMode>(thread?.runtime_mode ?? "auto_edit");
  const [plan, setPlan] = useState<InteractionMode>(thread?.interaction_mode ?? "default");
  // A roll code project publishes threads as changes, which needs a worktree: on by default there.
  const [worktree, setWorktree] = useState(mode.kind === "new" && !!mode.project.repo);
  const [sendMode, setSendMode] = useState<SendMode>("queue");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [dragging, setDragging] = useState(false);
  const area = useRef<HTMLTextAreaElement>(null);
  const files = useRef<HTMLInputElement>(null);

  // Follow the thread's own settings when they change elsewhere.
  useEffect(() => {
    if (!thread) return;
    setProvider(thread.provider);
    setModel(thread.model);
    setEffort(thread.effort);
    setRuntime(thread.runtime_mode);
    setPlan(thread.interaction_mode);
  }, [thread?.provider, thread?.model, thread?.effort, thread?.runtime_mode, thread?.interaction_mode, thread]);

  // Drafts survive closing the card (per thread, in this browser).
  useEffect(() => {
    try {
      const d = localStorage.getItem(draftKey(key));
      if (d) setText(d);
    } catch {}
  }, [key]);
  useEffect(() => {
    const t = setTimeout(() => {
      try {
        if (text) localStorage.setItem(draftKey(key), text);
        else localStorage.removeItem(draftKey(key));
      } catch {}
    }, 300);
    return () => clearTimeout(t);
  }, [text, key]);

  useEffect(() => {
    if (autoFocus) area.current?.focus({ preventScroll: true });
  }, [autoFocus]);

  const info = providers.find((p) => p.id === provider);
  const models = info?.models ?? [];
  const modelInfo = models.find((m) => m.id === model) ?? models.find((m) => m.default) ?? models[0];
  const efforts = modelInfo?.efforts ?? [];
  const running = !!thread && (thread.status === "starting" || thread.status === "running" || thread.status === "waiting");
  const caps = info?.capabilities;
  const machineOnline = !!mode.machine?.online;
  const loggedOut = info && info.auth === "missing";

  const configure = (patch: Partial<Pick<Thread, "provider" | "model" | "effort" | "runtime_mode" | "interaction_mode">>) => {
    if (!thread || !mode.machine) return;
    const clean = Object.fromEntries(Object.entries(patch).filter(([, v]) => v !== null && v !== undefined));
    client.run(mode.machine.id, { type: "thread.configure", thread: thread.id, ...clean }).catch((e: Error) => setError(e.message));
  };

  const pickProvider = (id: string) => {
    setProvider(id);
    const p = providers.find((x) => x.id === id);
    const m = p?.models.find((x) => x.default) ?? p?.models[0];
    setModel(m?.id ?? null);
    setEffort(null);
    configure({ provider: id, model: m?.id ?? undefined });
  };

  const addFiles = async (list: FileList | File[]) => {
    setError(null);
    for (const f of Array.from(list)) {
      try {
        const a = await readAsAttachment(f);
        setAttachments((x) => [...x, a]);
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
      }
    }
  };

  const submit = async (flip = false) => {
    const prompt = text.trim();
    if ((!prompt && !attachments.length) || busy || !machineOnline) return;
    setBusy(true);
    setError(null);
    try {
      if (mode.kind === "new") {
        await onSubmit?.({
          provider,
          ...(modelInfo ? { model: modelInfo.id } : {}),
          ...(effort ? { effort } : {}),
          runtime_mode: runtime,
          interaction_mode: plan,
          worktree,
          prompt,
          ...(attachments.length ? { attachments } : {}),
        });
      } else {
        const m: SendMode = running ? (flip ? (sendMode === "queue" ? "steer" : "queue") : sendMode) : "queue";
        await client.run(mode.machine!.id, {
          type: "message.send",
          thread: mode.thread.id,
          text: prompt,
          ...(attachments.length ? { attachments } : {}),
          ...(running ? { mode: m } : {}),
        });
      }
      setText("");
      setAttachments([]);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const interrupt = () => {
    if (!thread || !mode.machine) return;
    client.run(mode.machine.id, { type: "run.interrupt", thread: thread.id }).catch((e: Error) => setError(e.message));
  };

  const togglePlan = () => {
    const next: InteractionMode = plan === "plan" ? "default" : "plan";
    setPlan(next);
    configure({ interaction_mode: next });
  };

  const placeholder = !machineOnline
    ? "The machine is offline"
    : thread?.status === "waiting"
      ? "Answer above, or send a message to redirect the agent"
      : running
        ? sendMode === "queue"
          ? "Queue a message for when this run ends…"
          : "Steer the running agent…"
        : mode.kind === "new"
          ? "Describe the task. Enter starts it."
          : "Send a follow-up…";

  return (
    <div
      className={`rounded-xl border bg-bg transition-colors ${dragging ? "border-accent/60" : "border-line-2 focus-within:border-subtle"}`}
      onDragOver={(e) => {
        if (caps?.images === false) return;
        e.preventDefault();
        setDragging(true);
      }}
      onDragLeave={() => setDragging(false)}
      onDrop={(e) => {
        e.preventDefault();
        setDragging(false);
        if (e.dataTransfer.files.length) void addFiles(e.dataTransfer.files);
      }}
      data-nodrag
    >
      {attachments.length > 0 && (
        <ul className="flex flex-wrap gap-1.5 px-2.5 pt-2.5" aria-label="Attachments">
          {attachments.map((a, i) => (
            <li key={`${a.name}-${i}`} className="group relative">
              {/* eslint-disable-next-line @next/next/no-img-element */}
              <img src={`data:${a.mime};base64,${a.data}`} alt={a.name} className="size-12 rounded-md border border-line object-cover" />
              <button
                type="button"
                onClick={() => setAttachments((x) => x.filter((_, j) => j !== i))}
                aria-label={`Remove ${a.name}`}
                className="absolute -top-1.5 -right-1.5 grid size-4 place-items-center rounded-full border border-line bg-panel text-subtle hover:text-fg"
              >
                <X className="size-2.5" />
              </button>
            </li>
          ))}
        </ul>
      )}
      <textarea
        ref={area}
        rows={mode.kind === "new" ? 3 : 2}
        value={text}
        disabled={!machineOnline}
        onChange={(e) => setText(e.target.value)}
        onPaste={(e) => {
          const imgs = Array.from(e.clipboardData.files).filter((f) => f.type.startsWith("image/"));
          if (imgs.length && caps?.images !== false) {
            e.preventDefault();
            void addFiles(imgs);
          }
        }}
        onKeyDown={(e) => {
          if (e.nativeEvent.isComposing || e.keyCode === 229) return;
          if (e.key === "Enter" && !e.shiftKey && !e.altKey) {
            e.preventDefault();
            void submit(e.metaKey || e.ctrlKey);
          } else if (e.key === "Tab" && e.shiftKey && (caps?.plan_mode ?? true)) {
            e.preventDefault();
            togglePlan();
          } else if (e.key === "Escape" && onCancel) {
            onCancel();
          }
        }}
        placeholder={placeholder}
        aria-label="Message"
        className="block max-h-60 min-h-12 w-full resize-none bg-transparent px-3 pt-2.5 pb-1 text-[13.5px] leading-relaxed outline-none placeholder:text-subtle"
        style={{ fieldSizing: "content" } as React.CSSProperties}
      />
      <div className="flex flex-wrap items-center gap-1 px-2 pb-2">
        <Picker label="Provider" value={provider} onChange={pickProvider} options={providers.map((p) => ({ id: p.id, label: p.name, hint: p.auth === "missing" ? "log in first" : undefined }))} />
        {models.length > 0 && (
          <Picker
            label="Model"
            value={modelInfo?.id ?? ""}
            onChange={(id) => {
              setModel(id);
              setEffort(null);
              configure({ model: id });
            }}
            options={models.map((m) => ({ id: m.id, label: m.name }))}
          />
        )}
        {efforts.length > 0 && (
          <Picker
            label="Effort"
            value={effort ?? ""}
            onChange={(id) => {
              setEffort(id || null);
              if (id) configure({ effort: id });
            }}
            options={[{ id: "", label: "default effort" }, ...efforts.map((x) => ({ id: x, label: x }))]}
          />
        )}
        <Picker
          label="Runtime mode"
          value={runtime}
          onChange={(id) => {
            setRuntime(id as RuntimeMode);
            configure({ runtime_mode: id as RuntimeMode });
          }}
          options={runtimeModes.map((r) => ({ id: r.id, label: r.label, hint: r.hint }))}
        />
        {(caps?.plan_mode ?? true) && (
          <button
            type="button"
            onClick={togglePlan}
            aria-pressed={plan === "plan"}
            title="Plan first, change nothing until approved (Shift+Tab)"
            className={`inline-flex items-center gap-1 rounded-md px-1.5 py-1 text-[11.5px] transition ${
              plan === "plan" ? "bg-accent/15 text-accent" : "text-subtle hover:text-fg"
            }`}
          >
            <ListChecks className="size-3.5" /> Plan
          </button>
        )}
        {mode.kind === "new" && (
          <label className="inline-flex cursor-pointer items-center gap-1.5 px-1.5 py-1 text-[11.5px] text-subtle hover:text-fg" title="Work in its own git worktree on a new branch">
            <input type="checkbox" checked={worktree} onChange={(e) => setWorktree(e.target.checked)} className="accent-[var(--accent)]" />
            Worktree
          </label>
        )}
        {caps?.images !== false && (
          <>
            <button type="button" onClick={() => files.current?.click()} className="p-1 text-subtle hover:text-fg" title="Attach images" aria-label="Attach images">
              <ImagePlus className="size-3.5" />
            </button>
            <input
              ref={files}
              type="file"
              accept="image/*"
              multiple
              hidden
              onChange={(e) => {
                if (e.target.files) void addFiles(e.target.files);
                e.target.value = "";
              }}
            />
          </>
        )}
        <span className="ml-auto flex items-center gap-1">
          {running && (
            <span className="flex rounded-md border border-line p-px text-[11px]" role="radiogroup" aria-label="While running">
              {(["queue", "steer"] as const).map((m) => (
                <button
                  key={m}
                  type="button"
                  role="radio"
                  aria-checked={sendMode === m}
                  onClick={() => setSendMode(m)}
                  title={m === "queue" ? "Send after this run ends" : caps?.steer ? "Add to the running turn" : "Interrupt and restart with this added"}
                  className={`rounded px-1.5 py-0.5 ${sendMode === m ? "bg-panel-2 text-fg" : "text-subtle hover:text-fg"}`}
                >
                  {m}
                </button>
              ))}
            </span>
          )}
          {onCancel && (
            <button type="button" onClick={onCancel} className={btn}>
              Cancel
            </button>
          )}
          {running && !text.trim() ? (
            <button type="button" onClick={interrupt} aria-label="Interrupt" title="Interrupt the run" className="grid size-7 place-items-center rounded-lg bg-fg text-bg hover:opacity-85">
              <Square className="size-3" fill="currentColor" />
            </button>
          ) : (
            <button
              type="button"
              onClick={() => void submit()}
              disabled={busy || !machineOnline || (!text.trim() && !attachments.length) || (mode.kind === "new" && (!provider || !!loggedOut))}
              aria-label={running ? (sendMode === "queue" ? "Queue message" : "Steer") : mode.kind === "new" ? "Start thread" : "Send"}
              className="grid size-7 place-items-center rounded-lg bg-fg text-bg transition hover:opacity-85 disabled:opacity-25"
            >
              <ArrowUp className="size-4" strokeWidth={2.25} />
            </button>
          )}
        </span>
      </div>
      {(error || loggedOut) && (
        <p className="px-3 pb-2 text-[12px] text-danger" role="alert">
          {error ?? `${info!.name} isn't logged in on this machine. Use "Log in" on the machine.`}
        </p>
      )}
    </div>
  );
}

/** A compact select that looks like a chip. */
function Picker({
  label,
  value,
  options,
  onChange,
}: {
  label: string;
  value: string;
  options: { id: string; label: string; hint?: string }[];
  onChange: (id: string) => void;
}): ReactNode {
  const current = options.find((o) => o.id === value);
  return (
    <label className="relative inline-flex items-center gap-0.5 rounded-md px-1.5 py-1 text-[11.5px] text-muted hover:bg-panel-2 hover:text-fg" title={current?.hint ?? label}>
      <span className="max-w-32 truncate">{current?.label ?? label}</span>
      <ChevronDown className="size-3 text-subtle" />
      <select aria-label={label} value={value} onChange={(e) => onChange(e.target.value)} className={`${input} absolute inset-0 cursor-pointer opacity-0`}>
        {options.map((o) => (
          <option key={o.id} value={o.id}>
            {o.label}
            {o.hint && o.id !== value ? ` (${o.hint})` : ""}
          </option>
        ))}
      </select>
    </label>
  );
}
