"use client";

import type { MarkState } from "@roll/brand";
import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from "react";

interface MarkStateContext {
  state: MarkState;
  setState: (state: MarkState) => void;
  /** Play a sequence of states, e.g. deploying → success → idle. Cancels any running sequence. */
  play: (steps: [MarkState, number][], then?: MarkState) => void;
}

const Ctx = createContext<MarkStateContext | null>(null);

/** One shared logo state, so a deploy or an AI question anywhere animates every mark. */
export function MarkStateProvider({ children }: { children: ReactNode }) {
  const [state, setRaw] = useState<MarkState>("idle");
  const timers = useRef<ReturnType<typeof setTimeout>[]>([]);

  const cancel = useCallback(() => {
    timers.current.forEach(clearTimeout);
    timers.current = [];
  }, []);

  const setState = useCallback(
    (s: MarkState) => {
      cancel();
      setRaw(s);
    },
    [cancel],
  );

  const play = useCallback(
    (steps: [MarkState, number][], then: MarkState = "idle") => {
      cancel();
      let at = 0;
      for (const [s, ms] of steps) {
        timers.current.push(setTimeout(() => setRaw(s), at));
        at += ms;
      }
      timers.current.push(setTimeout(() => setRaw(then), at));
    },
    [cancel],
  );

  useEffect(() => cancel, [cancel]);

  const value = useMemo(() => ({ state, setState, play }), [state, setState, play]);
  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}

export function useMarkState() {
  const ctx = useContext(Ctx);
  if (!ctx) throw new Error("useMarkState must be used inside <MarkStateProvider>");
  return ctx;
}
