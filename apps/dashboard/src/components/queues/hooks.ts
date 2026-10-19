"use client";

import type { MarkState } from "@roll/brand";
import { useCallback, useEffect, useRef, useState } from "react";
import { errorMessage, OfflineError } from "@/lib/queues-api";
import { useMarkState } from "../mark-state";

export const POLL_MS = 5000;

export interface Poll<T> {
  data: T | null;
  /** Last API error (the API answered, but with an error). */
  error: string | null;
  /** rolld could not be reached. */
  offline: boolean;
  /** No data yet for the current key. */
  loading: boolean;
  refresh: () => Promise<void>;
  /** Optimistic local edit of the current data. */
  mutate: (fn: (d: T) => T) => void;
}

/**
 * Poll an API call every `ms` while the tab is visible. Changing `key` drops the previous data, so
 * switching vhost never shows the old vhost's rows. `key = null` pauses the poll.
 */
export function usePoll<T>(key: string | null, fn: () => Promise<T>, ms = POLL_MS): Poll<T> {
  const [state, setState] = useState<{ key: string | null; data: T | null; error: string | null; offline: boolean }>({
    key: null,
    data: null,
    error: null,
    offline: false,
  });
  const fnRef = useRef(fn);
  useEffect(() => {
    fnRef.current = fn;
  });
  const keyRef = useRef(key);
  const seq = useRef(0);

  const refresh = useCallback(async () => {
    const k = keyRef.current;
    if (k === null) return;
    const n = ++seq.current;
    try {
      const data = await fnRef.current();
      if (n === seq.current && k === keyRef.current) setState({ key: k, data, error: null, offline: false });
    } catch (err) {
      if (n !== seq.current || k !== keyRef.current) return;
      if (err instanceof OfflineError) setState((s) => ({ ...s, offline: true }));
      else
        setState((s) => ({ key: k, data: s.key === k ? s.data : null, error: errorMessage(err), offline: false }));
    }
  }, []);

  useEffect(() => {
    keyRef.current = key;
    if (key === null) return;
    let alive = true;
    let timer: ReturnType<typeof setTimeout>;
    const loop = async () => {
      if (document.visibilityState !== "hidden") await refresh();
      if (alive) timer = setTimeout(loop, ms);
    };
    loop();
    const onVisible = () => {
      if (document.visibilityState === "visible") refresh();
    };
    document.addEventListener("visibilitychange", onVisible);
    return () => {
      alive = false;
      clearTimeout(timer);
      document.removeEventListener("visibilitychange", onVisible);
    };
  }, [key, ms, refresh]);

  const mutate = useCallback((f: (d: T) => T) => {
    setState((s) => (s.data === null ? s : { ...s, data: f(s.data) }));
  }, []);

  const current = state.key === key;
  return {
    data: current ? state.data : null,
    error: current ? state.error : null,
    offline: state.offline,
    loading: !current || (state.data === null && state.error === null),
    refresh,
    mutate,
  };
}

export interface RunOptions {
  /** Mark state while the action runs. */
  mark?: MarkState;
  /** Mark state played after success (default `success`). */
  done?: MarkState;
  /** Called after the action failed (e.g. to roll back an optimistic edit). */
  rollback?: () => void;
}

/**
 * Run control-plane actions with a pending key, inline error and the shared logo reflecting
 * progress: `deploying` → `success` | `error`.
 */
export function useAction() {
  const { setState, play } = useMarkState();
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const run = useCallback(
    async <T>(key: string, fn: () => Promise<T>, opts: RunOptions = {}): Promise<T | undefined> => {
      setBusy(key);
      setError(null);
      setState(opts.mark ?? "deploying");
      try {
        const out = await fn();
        play([[opts.done ?? "success", opts.done === "sleep" ? 2000 : 1400]]);
        return out;
      } catch (err) {
        opts.rollback?.();
        setError(errorMessage(err));
        play([["error", 1600]]);
        return undefined;
      } finally {
        setBusy(null);
      }
    },
    [play, setState],
  );

  return { busy, error, setError, run };
}

/** Puts the shared mark into `thinking` while the first load of a page is in flight. */
export function useLoadingMark(loading: boolean) {
  const { state, setState } = useMarkState();
  const stateRef = useRef(state);
  useEffect(() => {
    stateRef.current = state;
  }, [state]);
  useEffect(() => {
    if (loading) {
      if (stateRef.current === "idle") setState("thinking");
    } else if (stateRef.current === "thinking") {
      setState("idle");
    }
  }, [loading, setState]);
  useEffect(
    () => () => {
      if (stateRef.current === "thinking") setState("idle");
    },
    [setState],
  );
}

export function useCopy() {
  const [copied, setCopied] = useState<string | null>(null);
  const copy = useCallback((key: string, text: string) => {
    navigator.clipboard?.writeText(text);
    setCopied(key);
    setTimeout(() => setCopied((c) => (c === key ? null : c)), 1200);
  }, []);
  return { copied, copy };
}

/** A rolling client-side series, e.g. a queue's depth sampled by each poll. */
export function useSamples<P extends { t: number }>(sample: P | null, windowS = 300, seed?: P[] | null) {
  const [points, setPoints] = useState<P[]>([]);
  const last = useRef<number>(0);
  useEffect(() => {
    if (!sample || sample.t === last.current) return;
    last.current = sample.t;
    setPoints((ps) => [...ps.filter((p) => p.t > sample.t - windowS), sample]);
  }, [sample, windowS]);
  // History from the server goes in front of whatever was sampled live.
  useEffect(() => {
    if (!seed?.length) return;
    setPoints((ps) => {
      const first = ps.length ? ps[0].t : Number.POSITIVE_INFINITY;
      return [...seed.filter((p) => p.t < first), ...ps];
    });
  }, [seed]);
  return points;
}
