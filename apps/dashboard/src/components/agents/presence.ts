import { useReducedMotion } from "@roll/brand";
import { animate, PresenceContext, useMotionValue, usePresence, type MotionValue, type Transition } from "motion/react";
import { useContext, useEffect, useRef } from "react";

/**
 * One animated value of something under `AnimatePresence`: `shown` while it's present, `hidden`
 * before it enters and while it leaves.
 */
export type Track = { value: MotionValue<number> | MotionValue<string>; shown: number | string; hidden: number | string; enter: Transition; leave: Transition };

/**
 * Drives `tracks` from the presence of the component calling it, and unmounts it once every one has
 * arrived at `hidden`. Each change animates from where the value is at that moment, so closing
 * mid-open, or opening again mid-close, turns around without a jump (motion's own `exit` replays
 * `initial` when a child comes back, which jumped a reopened sheet to nothing first). `key` names
 * what the targets depend on: a new key retargets.
 */
export function usePresenceTracks(tracks: Track[], key: string): boolean {
  const [present, safeToRemove] = usePresence();
  const latest = useRef({ tracks, safeToRemove });
  latest.current = { tracks, safeToRemove };
  // Not on `safeToRemove`: it's a new function whenever AnimatePresence renders (a busy page renders
  // all the time), and restarting on it never let a leaving sheet finish (seen in WebKit).
  useEffect(() => {
    let live = true;
    const runs = latest.current.tracks.map((t) => animate(t.value as MotionValue<number | string>, present ? t.shown : t.hidden, present ? t.enter : t.leave));
    if (!present) {
      void Promise.all(runs).then(() => {
        if (live) latest.current.safeToRemove?.();
      });
    }
    return () => {
      live = false;
      for (const r of runs) r.stop();
    };
  }, [present, key]);
  return present;
}

/**
 * A motion value starting at `hidden` when its component enters through `AnimatePresence` (at
 * `shown` if the presence says `initial={false}`, outside any presence, or with reduced motion: a
 * page switch drew one blank frame there, the old page gone and the new one still at 0).
 */
export function useEnteringValue(hidden: number, shown: number): MotionValue<number>;
export function useEnteringValue(hidden: string, shown: string): MotionValue<string>;
export function useEnteringValue(hidden: number | string, shown: number | string): MotionValue<number> | MotionValue<string> {
  const presence = useContext(PresenceContext);
  const reduced = useReducedMotion();
  return useMotionValue(presence && presence.initial !== false && !reduced ? hidden : shown) as MotionValue<number> | MotionValue<string>;
}
