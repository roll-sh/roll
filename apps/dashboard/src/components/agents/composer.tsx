"use client";

import { Bot, Check, CornerUpRight, ListPlus, Paperclip, PencilRuler, X } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import type {
  Attachment,
  Command,
  InteractionMode,
  Machine,
  ModelOptions,
  ProviderInfo,
  Project,
  Question,
  QueuedMessage,
  RuntimeMode,
  SendMode,
  Thread,
} from "@/lib/agents-api";
import { coming, idleCompactionOf, machineState, reachable } from "@/lib/agents-api";
import { readyPlan } from "@/lib/agents-plan";
import { ContextMeter } from "./context-meter";
import { compactsOnReturn, formatContextTokens } from "@/lib/agents-context";
import { runsOf } from "@/lib/agents-store";
import { DEFAULT_RUNTIME_MODE, ModelPicker, PermissionsMenu, resolveModel, type ModelChoice } from "./model-picker";
import { ImplementButton, PlanBanner, usePlanActions } from "./plan";
import { QueuedMessages } from "./queue";
import { pickByDigit, pill, QuestionsPanel, quiet, useQuestionFlow } from "./request-card";
import { Switch } from "./ui";
import { newThreadSettings } from "./new-thread";
import { useAgents, usePrefs } from "./use-agents";

/** What a new thread starts with (the `thread.create` fields the composer picks). */
export interface NewThreadFields {
  provider: string;
  model?: string;
  effort?: string;
  options?: ModelOptions;
  runtime_mode: RuntimeMode;
  interaction_mode: InteractionMode;
  worktree: boolean;
  prompt: string;
  attachments?: Attachment[];
}

export type ComposerMode = { kind: "new"; project: Project; machine: Machine } | { kind: "thread"; thread: Thread; machine: Machine | undefined };

/** Images only, at most 5 MiB each (agent-proto `Attachment`). */
export const MAX_ATTACHMENT_BYTES = 5 * 1024 * 1024;

const draftKey = (k: string) => `roll:agents:draft:${k}`;

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

/** ⌘ or Ctrl held (T3's `useShortcutModifierState`): the send button shows what a click would do. */
function useModifierHeld(): boolean {
  const [held, setHeld] = useState(false);
  useEffect(() => {
    const on = (e: KeyboardEvent) => setHeld(e.metaKey || e.ctrlKey);
    const off = () => setHeld(false);
    window.addEventListener("keydown", on);
    window.addEventListener("keyup", on);
    window.addEventListener("blur", off);
    return () => {
      window.removeEventListener("keydown", on);
      window.removeEventListener("keyup", on);
      window.removeEventListener("blur", off);
    };
  }, []);
  return held;
}

const NO_QUESTIONS: Question[] = [];
const EMPTY_DRAFT = { text: "", attachments: [] as Attachment[] };

/** One of the composer's slash commands (T3's `/plan`, `/default`, and the providers' `/compact`). */
export interface SlashCommand {
  name: "compact" | "plan" | "default";
  description: string;
}

/** The commands this composer offers: `/compact` on a thread whose provider compacts, plan mode where it has one. */
export function slashCommands(caps: ProviderInfo["capabilities"] | undefined, onThread: boolean): SlashCommand[] {
  const out: SlashCommand[] = [];
  if (onThread && caps?.compact) out.push({ name: "compact", description: "Summarize the conversation and reduce context usage" });
  if (caps?.plan_mode ?? true) {
    out.push({ name: "plan", description: "Switch this thread into plan mode" });
    out.push({ name: "default", description: "Switch this thread back to build mode" });
  }
  return out;
}

/** The commands a draft that is a bare `/word` could be (none for anything else). */
export function matchSlash(text: string, commands: readonly SlashCommand[]): SlashCommand[] {
  const m = /^\/([a-z-]*)$/i.exec(text.trim());
  return m ? commands.filter((c) => c.name.startsWith(m[1].toLowerCase())) : [];
}

/**
 * The composer, T3 Code's `ChatComposer` (MIT): a raised card, the prompt over a footer with
 * attach on the left and the model, permissions and plan mode beside the send button on the right.
 * Enter sends, Shift+Enter breaks the line, Shift+Tab toggles plan mode, images paste or drop in.
 * While a run is going a message steers it (T3's default follow-up); ⌘/Ctrl (Enter or a click)
 * queues it for after the run instead, and the send button shows which. With nothing typed the
 * button stops the run. Questions the agent asked sit on top of the card (T3's user-input panel):
 * 1–9 pick, what's typed here is the custom answer, and the button goes to the next question or
 * submits. On a thread, changing the pickers reconfigures its next runs.
 *
 * Also T3's: the queued messages sit on top of the card (`queue.tsx`; ⌥↑ edits the last one in
 * the composer, ⌘⇧Enter sends the first into the running turn, and a stopped thread's button
 * resumes its queue); a ready plan says so there too, and the button becomes Implement (with
 * "Implement in a new thread") or, once something is typed, Refine (`plan.tsx`); `/compact`,
 * `/plan` and `/default` are commands, offered as you type `/`; and the context meter compacts.
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
  const prefs = usePrefs();
  const providers = useMemo(() => (mode.machine?.info?.providers ?? []).filter((p) => p.installed), [mode.machine]);
  const thread = mode.kind === "thread" ? mode.thread : null;
  const key = thread ? thread.id : `new:${mode.kind === "new" ? mode.project.id : ""}`;

  const [text, setText] = useState("");
  const [attachments, setAttachments] = useState<Attachment[]>([]);
  // A new thread starts from the same defaults as every other way to start one (`new-thread.ts`).
  const start = mode.kind === "new" ? newThreadSettings(mode.project, mode.machine, prefs.defaults) : undefined;
  const [provider, setProvider] = useState<string>(() => thread?.provider ?? start?.provider ?? "");
  const [model, setModel] = useState<string | null>(thread?.model ?? start?.model ?? null);
  const [effort, setEffort] = useState<string | null>(thread?.effort ?? start?.effort ?? null);
  const [options, setOptions] = useState<ModelOptions>(thread?.options ?? start?.options ?? {});
  const [runtime, setRuntime] = useState<RuntimeMode>(thread?.runtime_mode ?? start?.runtime_mode ?? DEFAULT_RUNTIME_MODE);
  const [plan, setPlan] = useState<InteractionMode>(thread?.interaction_mode ?? start?.interaction_mode ?? "default");
  const [worktree, setWorktree] = useState(start?.worktree ?? false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [dragging, setDragging] = useState(false);
  const area = useRef<HTMLTextAreaElement>(null);
  const files = useRef<HTMLInputElement>(null);
  const held = useModifierHeld();
  // A queued message being edited here (T3's "Edit in the composer"), and the draft it pushed aside.
  const [editing, setEditing] = useState<QueuedMessage | null>(null);
  const stash = useRef<{ text: string; attachments: Attachment[] }>(EMPTY_DRAFT);
  const [slashAt, setSlashAt] = useState(0);
  const [slashClosed, setSlashClosed] = useState(false);

  // What the agent is waiting on in this thread: questions are answered here, an approval above.
  const pending = thread ? [...client.data.requests.values()].map((r) => r.value).filter((r) => r.thread === thread.id && r.status === "pending") : [];
  const asked = pending.find((r) => r.body.type === "questions");
  const approvalWaiting = pending.some((r) => r.body.type === "approval");
  const flow = useQuestionFlow(asked?.body.type === "questions" ? asked.body.questions : NO_QUESTIONS, asked?.id);

  // Follow the thread's own settings when they change elsewhere.
  useEffect(() => {
    if (!thread) return;
    setProvider(thread.provider);
    setModel(thread.model);
    setEffort(thread.effort);
    setOptions(thread.options ?? {});
    setRuntime(thread.runtime_mode);
    setPlan(thread.interaction_mode);
  }, [thread?.provider, thread?.model, thread?.effort, thread?.options, thread?.runtime_mode, thread?.interaction_mode, thread]);

  // Drafts survive closing the card (per thread, in this browser).
  useEffect(() => {
    try {
      const d = localStorage.getItem(draftKey(key));
      if (d) setText(d);
    } catch {}
  }, [key]);
  useEffect(() => {
    // A queued message being edited isn't the draft.
    if (editing) return;
    const t = setTimeout(() => {
      try {
        if (text) localStorage.setItem(draftKey(key), text);
        else localStorage.removeItem(draftKey(key));
      } catch {}
    }, 300);
    return () => clearTimeout(t);
  }, [text, key, editing]);

  useEffect(() => {
    if (autoFocus) area.current?.focus({ preventScroll: true });
  }, [autoFocus]);

  const info = providers.find((p) => p.id === provider);
  const modelInfo = resolveModel(info, model);
  const running = !!thread && (thread.status === "starting" || thread.status === "running" || thread.status === "waiting");
  const caps = info?.capabilities;
  // An asleep (or stopped) sandbox takes messages: sending wakes it.
  const machineOnline = reachable(mode.machine);
  const state = mode.machine ? machineState(mode.machine) : "offline";
  const loggedOut = info && info.auth === "missing";
  // A follow-up while running steers; with ⌘/Ctrl it queues (the owner's "While a run is going" can
  // swap the two). Waiting on an approval, it always steers (redirects).
  const queues = (mod: boolean) => running && !approvalWaiting && (prefs.follow_up === "queue" ? !mod : mod);
  // The queue waits on a stopped thread (T3's held runs): its button sends it on.
  const queueHeld = !!thread && !running && thread.queued.length > 0;
  // A plan ready to carry out (T3's plan follow-up), unless the agent is asking something else.
  const ready = thread && !asked ? readyPlan(client.data, thread) : null;
  const planActive = !!ready && !editing;
  const planActions = usePlanActions(thread, mode.machine, ready);
  const commands = slashCommands(caps, !!thread);
  const slash = slashClosed || editing ? [] : matchSlash(text, commands);

  const configure = (patch: Partial<Pick<Thread, "provider" | "model" | "effort" | "options" | "runtime_mode" | "interaction_mode">>) => {
    if (!thread || !mode.machine) return;
    const clean = Object.fromEntries(Object.entries(patch).filter(([, v]) => v !== null && v !== undefined));
    client.run(mode.machine.id, { type: "thread.configure", thread: thread.id, ...clean }).catch((e: Error) => setError(e.message));
  };

  // The picker keeps what still applies to a new model; the thread follows.
  const pick = (next: ModelChoice) => {
    const patch: Partial<Pick<Thread, "provider" | "model" | "effort" | "options">> = {};
    if (next.provider !== provider) patch.provider = next.provider;
    const nextModel = resolveModel(providers.find((p) => p.id === next.provider), next.model)?.id ?? null;
    if (nextModel && (nextModel !== modelInfo?.id || patch.provider)) patch.model = nextModel;
    if (next.effort && next.effort !== effort) patch.effort = next.effort;
    if (JSON.stringify(next.options) !== JSON.stringify(options)) patch.options = next.options;
    setProvider(next.provider);
    setModel(nextModel);
    setEffort(next.effort);
    setOptions(next.options);
    if (Object.keys(patch).length) configure(patch);
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

  const submit = async (mod = false) => {
    const prompt = text.trim();
    if (busy || !machineOnline) return;
    // `/plan` and `/default` switch the mode here and send nothing (T3's composer commands).
    if (!attachments.length && (prompt === "/plan" || prompt === "/default") && (caps?.plan_mode ?? true)) {
      const next: InteractionMode = prompt === "/plan" ? "plan" : "default";
      setPlan(next);
      configure({ interaction_mode: next });
      setText("");
      return;
    }
    if (editing) return void act(async () => {
      await command({ type: "message.edit", thread: thread!.id, message: editing.id, text: prompt, attachments });
      leaveEditing();
    });
    // A ready plan: typed words refine it, nothing typed carries it out (T3's plan follow-up).
    if (planActive && !prompt && !attachments.length) return void act(planActions.implement);
    if (planActive && ready?.approval) return void act(async () => {
      await planActions.refine(prompt);
      setText("");
    });
    // A held queue with nothing typed: send it on (T3's "Resume thread").
    if (queueHeld && !prompt && !attachments.length) return void act(() => promote(thread!.queued[0]));
    if (!prompt && !attachments.length) return;
    setBusy(true);
    setError(null);
    try {
      if (mode.kind === "new") {
        await onSubmit?.({
          provider,
          ...(modelInfo ? { model: modelInfo.id } : {}),
          ...(effort ? { effort } : {}),
          ...(Object.keys(options).length ? { options } : {}),
          runtime_mode: runtime,
          interaction_mode: plan,
          worktree,
          prompt,
          ...(attachments.length ? { attachments } : {}),
        });
      } else {
        // A compaction can't steer a run: it waits behind it.
        const m: SendMode = queues(mod) || prompt === "/compact" ? "queue" : "steer";
        // Compact and send: the compaction runs first, and the message waits behind it.
        if (compactFirst && prompt !== "/compact") {
          await client.run(mode.machine!.id, { type: "message.send", thread: mode.thread.id, text: "/compact" });
          await client.run(mode.machine!.id, { type: "message.send", thread: mode.thread.id, text: prompt, ...(attachments.length ? { attachments } : {}), mode: "queue" });
          setText("");
          setAttachments([]);
          return;
        }
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

  /** Run something with the button busy and its error shown. */
  const act = async (fn: () => Promise<unknown>) => {
    setBusy(true);
    setError(null);
    try {
      await fn();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
  const command = (c: Command) => client.run(mode.machine!.id, c);

  // The queue (T3's queued runs).
  const promote = (q: QueuedMessage) => command({ type: "message.promote", thread: thread!.id, message: q.id });
  const startEditing = (q: QueuedMessage) => {
    if (!editing) stash.current = { text, attachments };
    setEditing(q);
    setText(q.text);
    setAttachments(q.attachments);
    requestAnimationFrame(() => area.current?.focus());
  };
  const leaveEditing = () => {
    setEditing(null);
    setText(stash.current.text);
    setAttachments(stash.current.attachments);
    stash.current = EMPTY_DRAFT;
  };
  const queueActions = {
    sendNow: (q: QueuedMessage) => void act(() => promote(q)),
    edit: startEditing,
    remove: (q: QueuedMessage) => {
      if (editing?.id === q.id) leaveEditing();
      void act(() => command({ type: "message.unqueue", thread: thread!.id, message: q.id }));
    },
    move: (q: QueuedMessage, before: QueuedMessage | null) => void act(() => command({ type: "message.reorder", thread: thread!.id, message: q.id, ...(before ? { before: before.id } : {}) })),
  };
  // A message edited here that went meanwhile (sent, or removed elsewhere): what's typed stays as a draft.
  useEffect(() => {
    if (editing && thread && !thread.queued.some((q) => q.id === editing.id)) {
      setEditing(null);
      stash.current = EMPTY_DRAFT;
    }
  }, [editing, thread]);

  // T3's idle compaction "When you return": back to a big conversation whose prompt cache has
  // expired, the button compacts first, then sends ("Compact and send").
  const lastEnded = thread && !running ? runsOf(client.data, thread.id).at(-1)?.completed_at : null;
  const compactFirst =
    !!thread &&
    !running &&
    !editing &&
    compactsOnReturn({
      provider: thread.provider,
      context: thread.context,
      idleSince: lastEnded,
      now: Date.now(),
      mode: idleCompactionOf(mode.machine?.info?.settings?.set, thread.provider),
      canCompact: !!caps?.compact,
    });

  const compactAction =
    thread && caps?.compact
      ? {
          onCompact: () => void act(() => command({ type: "message.send", thread: thread.id, text: "/compact" })),
          disabled: !machineOnline ? "The machine is offline" : running ? "Compacting waits for the agent to finish" : undefined,
        }
      : undefined;

  // The questions: the button goes on to the next one, and answers on the last.
  const answer = async () => {
    if (!asked || busy || !machineOnline) return;
    if (!flow.last) {
      if (flow.answered) flow.next();
      return;
    }
    if (!flow.complete) return;
    setBusy(true);
    setError(null);
    try {
      await client.run(mode.machine!.id, { type: "request.respond", request: asked.id, response: { type: "answers", answers: flow.answers() } });
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

  /** Run a slash command: `/plan` and `/default` switch the mode, `/compact` compacts (after the run, if one is going). */
  const runSlash = (c: SlashCommand) => {
    setText("");
    setSlashAt(0);
    if (c.name === "compact") {
      if (thread) void act(() => command({ type: "message.send", thread: thread.id, text: "/compact", ...(running ? { mode: "queue" as const } : {}) }));
      return;
    }
    const next: InteractionMode = c.name === "plan" ? "plan" : "default";
    setPlan(next);
    configure({ interaction_mode: next });
  };

  const question = asked ? flow.question : null;
  const placeholder = !machineOnline
    ? "The machine is offline"
    : state === "asleep"
      ? "The sandbox is asleep; sending wakes it"
      : state === "stopped"
        ? "The sandbox is stopped; sending starts it"
        : coming(state)
          ? "Waking the sandbox; this goes once it's up"
          : question
            ? question.options.length
              ? "Type your own answer, or pick one above"
              : "Type your answer"
            : editing
              ? "Edit the queued message. Esc stops editing."
              : planActive
                ? "Implement the plan, or say what to change"
                : approvalWaiting
                  ? "Send a message to redirect the agent"
                  : mode.kind === "new"
                    ? "Describe the task. Enter starts it."
                    : "Ask anything. / for commands";
  const value = question ? flow.custom(question) : text;
  const setValue = question ? flow.setCustom : setText;
  const hasText = !!text.trim() || attachments.length > 0;
  const queuing = queues(held);

  return (
    <div
      className="relative"
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
      {asked ? (
        // T3's attached banner: tucked 1rem under the card's top edge (the padding keeps its rows clear), scrolling on its own past 24rem or 40% of the screen.
        <div className="mx-[1.375rem] -mb-4 rounded-t-2xl border border-b-0 border-outline bg-raised pt-3 pb-5 transition-opacity duration-150 starting:opacity-0">
          <div className="max-h-[min(24rem,40dvh)] overflow-y-auto overscroll-contain px-3.5 pb-1" data-scroll>
            <QuestionsPanel flow={flow} />
          </div>
        </div>
      ) : (
        (planActive || (thread && thread.queued.length > 0)) && (
          <div className="mx-[1.375rem] -mb-4 space-y-1 rounded-t-2xl border border-b-0 border-outline bg-raised pt-2 pb-5 transition-opacity duration-150 starting:opacity-0">
            {thread && thread.queued.length > 0 && (
              <div className="max-h-[min(14rem,30dvh)] overflow-y-auto overscroll-contain" data-scroll>
                <QueuedMessages queued={thread.queued} held={queueHeld} running={running} editing={editing?.id ?? null} actions={queueActions} onResume={() => void act(() => promote(thread.queued[0]))} />
              </div>
            )}
            {planActive && <PlanBanner ready={ready!} />}
          </div>
        )
      )}
      {slash.length > 0 && (
        // The commands a `/` could be (T3's composer command menu): ↑↓ to choose, Enter or Tab to run.
        <ul role="listbox" aria-label="Commands" className="pop absolute inset-x-3 bottom-full z-30 mb-2 rounded-lg border border-line bg-panel p-1 shadow-pop">
          {slash.map((c, i) => (
            <li key={c.name} role="option" aria-selected={i === Math.min(slashAt, slash.length - 1)}>
              <button
                type="button"
                onMouseDown={(e) => e.preventDefault()}
                onClick={() => runSlash(c)}
                className={`flex w-full items-baseline gap-2 rounded-md px-2 py-1.5 text-left text-[12.5px] ${i === Math.min(slashAt, slash.length - 1) ? "bg-fg/[0.06]" : "hover:bg-fg/[0.04]"}`}
              >
                <span className="font-mono text-fg">/{c.name}</span>
                <span className="min-w-0 truncate text-muted">{c.description}</span>
              </button>
            </li>
          ))}
        </ul>
      )}
      <div
        className={`@container/composer relative z-10 rounded-2xl border bg-raised shadow-float inset-shadow-highlight transition-colors duration-200 ${dragging ? "border-accent/60 ring-1 ring-accent/40 ring-inset" : "border-outline"}`}
      >
        {attachments.length > 0 && (
          <ul className="flex flex-wrap gap-1.5 px-4 pt-3.5" aria-label="Attachments">
            {attachments.map((a, i) => (
              <li key={`${a.name}-${i}`} className="group relative transition-[opacity,scale] duration-300 ease-drawer starting:scale-95 starting:opacity-0">
                {/* eslint-disable-next-line @next/next/no-img-element */}
                <img src={`data:${a.mime};base64,${a.data}`} alt={a.name} className="size-12 rounded-lg border border-line-2 object-cover" />
                <button
                  type="button"
                  onClick={() => setAttachments((x) => x.filter((_, j) => j !== i))}
                  aria-label={`Remove ${a.name}`}
                  className="absolute -top-1.5 -right-1.5 grid size-4 place-items-center rounded-full border border-line-2 bg-popover text-subtle hover:text-fg"
                >
                  <X className="size-2.5" />
                </button>
              </li>
            ))}
          </ul>
        )}
        <textarea
          ref={area}
          rows={2}
          value={value}
          disabled={!machineOnline}
          onChange={(e) => {
            setValue(e.target.value);
            if (!e.target.value.startsWith("/")) setSlashClosed(false);
            setSlashAt(0);
          }}
          onPaste={(e) => {
            const imgs = Array.from(e.clipboardData.files).filter((f) => f.type.startsWith("image/"));
            if (imgs.length && caps?.images !== false && !question) {
              e.preventDefault();
              void addFiles(imgs);
            }
          }}
          onKeyDown={(e) => {
            if (e.nativeEvent.isComposing || e.keyCode === 229) return;
            if (question && pickByDigit(flow, e, value !== "")) return;
            if (slash.length && !question) {
              if (e.key === "ArrowDown" || e.key === "ArrowUp") {
                e.preventDefault();
                const n = slash.length;
                setSlashAt((i) => (Math.min(i, n - 1) + (e.key === "ArrowDown" ? 1 : n - 1)) % n);
                return;
              }
              if ((e.key === "Enter" && !e.shiftKey) || e.key === "Tab") {
                e.preventDefault();
                runSlash(slash[Math.min(slashAt, slash.length - 1)]);
                return;
              }
              if (e.key === "Escape") {
                e.preventDefault();
                setSlashClosed(true);
                return;
              }
            }
            // The owner's send key (T3's `sendShortcut`): Enter, or ⌘/Ctrl+Enter (then Enter breaks the line).
            const mod = e.metaKey || e.ctrlKey;
            if (e.key === "Enter" && e.shiftKey && mod && !question) {
              // T3: ⌘⇧Enter sends the first queued message into the running turn; with ⌘Enter to
              // send, it does the opposite of what ⌘Enter does for one message.
              e.preventDefault();
              if (prefs.send !== "enter") void submit(true);
              else if (running && thread && thread.queued.length > 0 && !hasText) void act(() => promote(thread.queued[0]));
              return;
            }
            if (e.key === "Enter" && !e.shiftKey && !e.altKey && (prefs.send === "enter" || mod || question)) {
              e.preventDefault();
              if (question) void answer();
              else void submit(prefs.send === "enter" && mod);
            } else if (e.key === "Tab" && e.shiftKey && (caps?.plan_mode ?? true)) {
              e.preventDefault();
              togglePlan();
            } else if (e.key === "ArrowUp" && e.altKey && thread && thread.queued.length > 0 && !question) {
              // T3's "Edit last queued message".
              e.preventDefault();
              startEditing(thread.queued[thread.queued.length - 1]);
            } else if (e.key === "Escape" && editing) {
              e.preventDefault();
              e.stopPropagation();
              leaveEditing();
            } else if (e.key === "Escape" && onCancel) {
              onCancel();
            }
          }}
          placeholder={placeholder}
          aria-label="Message"
          className="block max-h-52 min-h-[3.25rem] w-full resize-none bg-transparent px-4 pt-3.5 pb-2 text-[14px] leading-relaxed outline-none placeholder:text-subtle sm:pt-4"
          style={{ fieldSizing: "content" } as React.CSSProperties}
        />
        <div className="flex min-w-0 items-center px-2 pb-2">
          {caps?.images !== false && (
            <>
              <button
                type="button"
                onClick={() => files.current?.click()}
                className="me-1 grid size-8 shrink-0 place-items-center rounded-lg text-muted transition-colors hover:bg-fg/[0.06] hover:text-fg"
                title="Attach images"
                aria-label="Attach images"
              >
                <Paperclip className="size-4" />
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
          <div className="flex min-w-0 flex-1 items-center justify-end gap-2" data-composer-actions>
            {/* Narrow (a side panel, a phone): the chips keep their icons and the model its name, as T3's footer compacts. */}
            <div className="flex min-w-0 flex-1 items-center justify-end gap-0.5 overflow-hidden whitespace-nowrap [&>*]:shrink-0 [&>*:first-child]:min-w-0 [&>*:first-child]:shrink @max-[28rem]/composer:[&_[aria-label^='Permissions:']]:gap-0.5 @max-[28rem]/composer:[&_[aria-label^='Permissions:']]:text-[0px]">
              <ModelPicker
                providers={mode.machine?.info?.providers ?? providers}
                value={{ provider, model: modelInfo?.id ?? model, effort, options }}
                onChange={pick}
                side={mode.kind === "new" ? "bottom" : "top"}
                shortcuts
              />
              <PermissionsMenu
                value={runtime}
                side={mode.kind === "new" ? "bottom" : "top"}
                onChange={(id) => {
                  setRuntime(id);
                  configure({ runtime_mode: id });
                }}
              />
              {(caps?.plan_mode ?? true) && (
                <button
                  type="button"
                  onClick={togglePlan}
                  aria-pressed={plan === "plan"}
                  aria-label="Plan mode"
                  title={plan === "plan" ? "Plan mode: plans first and changes nothing until you approve (Shift+Tab)" : "Build mode (Shift+Tab for plan mode)"}
                  className={`inline-flex h-7 shrink-0 items-center gap-1.5 rounded-lg px-2 text-[12px] font-medium transition-colors hover:bg-fg/[0.06] ${plan === "plan" ? "text-accent" : "text-soft"}`}
                >
                  {plan === "plan" ? <PencilRuler className="size-4" /> : <Bot className="size-4" />}
                  <span className="@max-[28rem]/composer:hidden">{plan === "plan" ? "Plan" : "Build"}</span>
                </button>
              )}
              {mode.kind === "new" && (
                <label className="inline-flex h-7 shrink-0 cursor-pointer items-center gap-1.5 rounded-lg px-2 text-[12px] font-medium text-soft hover:bg-fg/[0.06]" title="Work in its own git worktree on a new branch">
                  <Switch size="sm" checked={worktree} onCheckedChange={setWorktree} aria-label="Worktree" />
                  Worktree
                </label>
              )}
            </div>
            {thread?.context && <ContextMeter context={thread.context} size={18} compact={compactAction} />}
            {onCancel && (
              <button type="button" onClick={onCancel} className={quiet}>
                Cancel
              </button>
            )}
            {editing && !question && (
              <button type="button" onClick={leaveEditing} className={quiet} title="Stop editing (Esc)">
                Cancel
              </button>
            )}
            {question ? (
              <span className="flex shrink-0 items-center gap-1">
                {flow.index > 0 && (
                  <button type="button" onClick={flow.back} className={quiet}>
                    Previous
                  </button>
                )}
                <button type="button" onClick={() => void answer()} disabled={busy || !machineOnline || (flow.last ? !flow.complete : !flow.answered)} className={pill}>
                  {busy ? (
                    <span className="shimmer">Sending…</span>
                  ) : flow.last ? (
                    flow.questions.length === 1 ? (
                      "Submit answer"
                    ) : (
                      "Submit answers"
                    )
                  ) : (
                    <span>
                      Next<span className="@max-[28rem]/composer:hidden"> question</span>
                    </span>
                  )}
                </button>
              </span>
            ) : editing ? (
              <button
                type="button"
                onClick={() => void submit()}
                disabled={busy || !machineOnline || !hasText}
                aria-label="Update queued message"
                title="Update queued message"
                className="grid size-8 shrink-0 place-items-center rounded-full bg-accent text-white shadow-xs transition-[transform,background-color,opacity] duration-150 enabled:hover:scale-105 enabled:active:scale-[0.97] disabled:bg-fg/[0.12] disabled:text-subtle motion-reduce:transition-none"
              >
                <Check className="size-4" />
              </button>
            ) : planActive && (!running || ready?.approval) ? (
              hasText ? (
                <button type="button" onClick={() => void submit()} disabled={busy || !machineOnline} className={pill} title="Send this to change the plan; it stays in plan mode">
                  {busy ? <span className="shimmer">Sending…</span> : "Refine"}
                </button>
              ) : (
                <ImplementButton busy={busy} disabled={busy || !machineOnline} onImplement={() => void act(planActions.implement)} onNewThread={() => void act(planActions.implementInNewThread)} />
              )
            ) : compactFirst && hasText && thread?.context ? (
              <button
                type="button"
                onClick={() => void submit()}
                disabled={busy || !machineOnline}
                className={pill}
                title={`Summarize ${formatContextTokens(thread.context.used_tokens)} tokens of history, then send`}
              >
                {busy ? <span className="shimmer">Compacting…</span> : "Compact and send"}
              </button>
            ) : running && !hasText ? (
              <button
                type="button"
                onClick={interrupt}
                aria-label="Stop generation"
                title="Interrupt"
                className="grid size-8 shrink-0 place-items-center rounded-full bg-danger/90 text-white shadow-xs transition-colors duration-150 hover:bg-danger"
              >
                <svg width="12" height="12" viewBox="0 0 12 12" fill="currentColor" aria-hidden>
                  <rect x="2" y="2" width="8" height="8" rx="1.5" />
                </svg>
              </button>
            ) : (
              <button
                type="button"
                onClick={(e) => void submit(e.metaKey || e.ctrlKey)}
                disabled={busy || !machineOnline || (!hasText && !queueHeld) || (mode.kind === "new" && (!provider || !!loggedOut))}
                aria-label={running ? (queuing ? "Queue message" : "Steer message") : mode.kind === "new" ? "Start thread" : queueHeld && !hasText ? "Resume thread" : "Send"}
                title={running ? (approvalWaiting ? "Send to redirect the agent" : `Click to ${queuing ? "queue" : "steer"}, ${MOD}-click to ${queuing ? "steer" : "queue"}`) : queueHeld && !hasText ? "Send the queued messages" : undefined}
                className="grid size-8 shrink-0 place-items-center rounded-full bg-accent text-white shadow-xs transition-[transform,background-color,opacity] duration-150 enabled:hover:scale-105 enabled:active:scale-[0.97] disabled:bg-fg/[0.12] disabled:text-subtle motion-reduce:transition-none"
              >
                {busy ? (
                  <span className="size-3.5 animate-spin rounded-full border-2 border-current border-t-transparent motion-reduce:animate-none" aria-hidden />
                ) : running ? (
                  queuing ? (
                    <ListPlus className="size-4" />
                  ) : (
                    <CornerUpRight className="size-4" />
                  )
                ) : (
                  <svg width="14" height="14" viewBox="0 0 14 14" fill="none" aria-hidden>
                    <path d="M7 11.5V2.5M7 2.5L3 6.5M7 2.5L11 6.5" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round" />
                  </svg>
                )}
              </button>
            )}
          </div>
        </div>
      </div>
      {(error || loggedOut) && (
        <p className="px-3 pt-2 text-[12px] text-danger" role="alert">
          {error ?? `${info!.name} isn't logged in on this machine. Use "Log in" on the machine.`}
        </p>
      )}
    </div>
  );
}

const MOD = typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.platform) ? "⌘" : "Ctrl";
