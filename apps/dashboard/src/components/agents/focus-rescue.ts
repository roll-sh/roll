"use client";

/**
 * Keyboard focus never falls to the page when what had it goes away: Settle on a row (the row
 * moves to its shelf), Discard (its bar goes), an endpoint saved or removed (its form or row goes).
 * Focus moves to what took the removed thing's place: the next thing after it, else the one before,
 * else the nearest thing around it that takes focus. Only after a key (`lib/input-modality.ts`): a
 * click leaves focus where the browser put it (Safari doesn't focus a clicked button anyway).
 * Dialogs give focus back themselves; this is for everything else in `root`.
 */

import { useEffect, type RefObject } from "react";
import { lastInputWasKeyboard } from "@/lib/input-modality";

const FOCUSABLE = 'button:not([disabled]), a[href], input:not([disabled]):not([type="hidden"]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

function focusable(el: Element | null): HTMLElement | null {
  if (!(el instanceof HTMLElement) || el.closest("[inert]")) return null;
  // Laid out and not hidden (browsers without checkVisibility, and jsdom, take it as shown).
  const shown = (e: HTMLElement) => !e.closest("[inert], [hidden], [aria-hidden=true]") && (typeof e.checkVisibility !== "function" || e.checkVisibility());
  if (el.matches(FOCUSABLE) && shown(el)) return el;
  for (const e of el.querySelectorAll<HTMLElement>(FOCUSABLE)) if (shown(e)) return e;
  return null;
}

/** Where focus goes when what held it left `parent` from between `prev` and `next`. */
export function rescueTarget(parent: Node, prev: Node | null, next: Node | null, root: HTMLElement): HTMLElement | null {
  for (let n = next; n; n = n.nextSibling) {
    const f = focusable(n as Element);
    if (f) return f;
  }
  for (let n = prev; n; n = n.previousSibling) {
    const f = focusable(n as Element);
    if (f) return f;
  }
  for (let p: Node | null = parent; p && p !== root.parentNode; p = p.parentNode) {
    const f = p instanceof Element && p.isConnected ? focusable(p) : null;
    if (f) return f;
  }
  return null;
}

export function useFocusRescue(root: RefObject<HTMLElement | null>) {
  useEffect(() => {
    const el = root.current;
    if (!el) return;
    let held: HTMLElement | null = null;
    const onIn = (e: FocusEvent) => {
      if (e.target instanceof HTMLElement && el.contains(e.target)) held = e.target;
    };
    document.addEventListener("focusin", onIn);
    const mo = new MutationObserver((records) => {
      const lost = held;
      if (!lost || lost.isConnected) return;
      const active = document.activeElement;
      if (active && active !== document.body && active.isConnected) return;
      if (!lastInputWasKeyboard()) return;
      for (const r of records) {
        if (![...r.removedNodes].some((n) => n === lost || n.contains(lost))) continue;
        held = null;
        // A whole panel closing (settings, a conversation): the page gives focus back to what opened it.
        if (lost.closest("[data-panel]")) return;
        rescueTarget(r.target, r.previousSibling, r.nextSibling, el)?.focus();
        return;
      }
    });
    mo.observe(el, { childList: true, subtree: true });
    return () => {
      document.removeEventListener("focusin", onIn);
      mo.disconnect();
    };
  }, [root]);
}
