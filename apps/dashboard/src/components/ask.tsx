"use client";

import { HorizonMark } from "@roll/brand";
import { ArrowUp, Check } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { answerFor, suggestions } from "@/lib/ask";
import { useMarkState } from "./mark-state";

type Phase = "idle" | "thinking" | "answering" | "done";

const STEP_MS = 950;
const CHARS_PER_TICK = 2;
const TICK_MS = 16;

/** Drives the simulated assistant and the shared logo state: thinking → (stream) → success. */
export function useAsk() {
  const { setState, play } = useMarkState();
  const [phase, setPhase] = useState<Phase>("idle");
  const [question, setQuestion] = useState("");
  const [steps, setSteps] = useState<string[]>([]);
  const [step, setStep] = useState(0);
  const [text, setText] = useState("");
  const timers = useRef<ReturnType<typeof setTimeout>[]>([]);

  const clear = useCallback(() => {
    timers.current.forEach(clearTimeout);
    timers.current = [];
  }, []);

  useEffect(() => clear, [clear]);

  const ask = useCallback(
    (q: string) => {
      const prompt = q.trim();
      if (!prompt) return;
      clear();
      const reply = answerFor(prompt);
      setQuestion(prompt);
      setSteps(reply.steps);
      setStep(0);
      setText("");
      setPhase("thinking");
      setState("thinking");

      reply.steps.forEach((_, i) => timers.current.push(setTimeout(() => setStep(i + 1), STEP_MS * (i + 1))));
      const streamAt = STEP_MS * (reply.steps.length + 0.4);
      timers.current.push(setTimeout(() => setPhase("answering"), streamAt));
      const ticks = Math.ceil(reply.answer.length / CHARS_PER_TICK);
      for (let i = 1; i <= ticks; i++) {
        timers.current.push(
          setTimeout(() => setText(reply.answer.slice(0, i * CHARS_PER_TICK)), streamAt + i * TICK_MS),
        );
      }
      timers.current.push(
        setTimeout(
          () => {
            setPhase("done");
            play([["success", 1600]]);
          },
          streamAt + ticks * TICK_MS + 80,
        ),
      );
    },
    [clear, play, setState],
  );

  const reset = useCallback(() => {
    clear();
    setPhase("idle");
    setQuestion("");
    setText("");
    setSteps([]);
    setState("idle");
  }, [clear, setState]);

  return { phase, question, steps, step, text, ask, reset };
}

export function AskPanel({ autoFocus = false }: { autoFocus?: boolean }) {
  const { state } = useMarkState();
  const { phase, question, steps, step, text, ask, reset } = useAsk();
  const [value, setValue] = useState("");
  const busy = phase === "thinking" || phase === "answering";

  const submit = (q: string) => {
    if (busy) return;
    ask(q);
    setValue("");
  };

  return (
    <div className="w-full max-w-xl">
      <form
        onSubmit={(e) => {
          e.preventDefault();
          submit(value);
        }}
        className="flex items-center gap-3 rounded-2xl border border-line-2 bg-panel/80 py-2 pr-2 pl-3 shadow-[0_8px_40px_-12px_rgba(0,0,0,0.6)] backdrop-blur focus-within:border-subtle"
      >
        <HorizonMark state={state} size={28} interactive={false} />
        <input
          autoFocus={autoFocus}
          value={value}
          onChange={(e) => setValue(e.target.value)}
          placeholder={busy ? "roll is thinking…" : "Ask roll anything about your infrastructure"}
          disabled={busy}
          className="min-w-0 flex-1 bg-transparent text-[15px] outline-none placeholder:text-subtle disabled:opacity-60"
        />
        <button
          type="submit"
          disabled={busy || !value.trim()}
          aria-label="Ask"
          className="grid size-8 place-items-center rounded-lg bg-fg text-bg transition disabled:opacity-25"
        >
          <ArrowUp className="size-4" strokeWidth={2.5} />
        </button>
      </form>

      {phase === "idle" ? (
        <div className="mt-3 flex flex-wrap justify-center gap-2">
          {suggestions.map((s) => (
            <button
              key={s}
              onClick={() => submit(s)}
              className="rounded-full border border-line px-3 py-1 text-[13px] text-muted transition hover:border-line-2 hover:text-fg"
            >
              {s}
            </button>
          ))}
        </div>
      ) : (
        <div className="rise mt-3 rounded-2xl border border-line bg-panel/80 p-4 text-left backdrop-blur">
          <p className="text-[13px] text-subtle">{question}</p>
          <ol className="mt-3 space-y-1.5">
            {steps.map((s, i) =>
              i > step ? null : (
                <li key={s} className="rise flex items-center gap-2 text-[13px]">
                  {i < step ? (
                    <Check className="size-3.5 text-ok" strokeWidth={3} />
                  ) : (
                    <span className="size-1.5 rounded-full bg-accent" />
                  )}
                  <span className={i < step ? "text-muted" : "shimmer"}>{s}</span>
                </li>
              ),
            )}
          </ol>
          {text && (
            <p className={`mt-3 text-[15px] leading-relaxed ${phase === "answering" ? "caret" : ""}`}>{text}</p>
          )}
          {phase === "done" && (
            <button onClick={reset} className="mt-3 text-[13px] text-subtle hover:text-fg">
              Ask something else
            </button>
          )}
        </div>
      )}
    </div>
  );
}
