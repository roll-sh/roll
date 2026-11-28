import type { MarkState } from "@roll/brand";
import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from "react";

interface MarkStateContext {
  state: MarkState;
  setState: (state: MarkState) => void;
  /** Play a sequence of states, e.g. deploying → success → idle. Cancels any running sequence. */
  play: (steps: [MarkState, number][], then?: MarkState) => void;
  /** Something is loading (a page, its data): the marks think while any hold is open and nothing
   *  else is playing. Returns the release. */
  hold: () => () => void;
}

const Ctx = createContext<MarkStateContext | null>(null);

/** One shared logo state, so a deploy, an AI question or a page loading anywhere animates every mark. */
export function MarkStateProvider({ children }: { children: ReactNode }) {
  const [raw, setRaw] = useState<MarkState>("idle");
  const [holds, setHolds] = useState(0);
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

  const hold = useCallback(() => {
    setHolds((n) => n + 1);
    let released = false;
    return () => {
      if (released) return;
      released = true;
      setHolds((n) => n - 1);
    };
  }, []);

  useEffect(() => cancel, [cancel]);

  // A deploy or an answer playing wins over loading; loading only fills the idle mark.
  const state: MarkState = raw === "idle" && holds > 0 ? "thinking" : raw;
  const value = useMemo(() => ({ state, setState, play, hold }), [state, setState, play, hold]);
  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}

export function useMarkState() {
  const ctx = useContext(Ctx);
  if (!ctx) throw new Error("useMarkState must be used inside <MarkStateProvider>");
  return ctx;
}

/** The shared state for a mark that only shows it (idle outside a provider, as in tests). */
export function useMarkStateValue() {
  return useContext(Ctx)?.state ?? "idle";
}

/**
 * The marks think while `active` stays true for longer than `after` ms (so a fast load never makes
 * the logo twitch). A no-op outside a provider.
 */
export function useMarkBusy(active: boolean, after = 200) {
  const hold = useContext(Ctx)?.hold;
  useEffect(() => {
    if (!active || !hold) return;
    let release: (() => void) | undefined;
    const t = setTimeout(() => (release = hold()), after);
    return () => {
      clearTimeout(t);
      release?.();
    };
  }, [active, after, hold]);
}
