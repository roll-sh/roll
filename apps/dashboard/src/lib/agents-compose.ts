/**
 * Puts text and images into a thread's composer from elsewhere (T3's `useTimelineRewind`: "Edit
 * from here" puts the message back in the composer, appended to whatever was typed). The open
 * composer of that thread takes it; with none open, it waits in the thread's saved draft.
 */

import type { Attachment, ThreadId } from "./agents-api";

export interface ComposeDraft {
  text: string;
  attachments: Attachment[];
}

const EVENT = "roll:agents:compose";
const draftKey = (thread: ThreadId) => `roll:agents:draft:${thread}`;

/** `text` joined onto `before` with a blank line, as T3 appends. */
export function appendDraft(before: string, text: string): string {
  return before.trim() ? `${before.replace(/\s+$/, "")}\n\n${text}` : text;
}

export function composeInto(thread: ThreadId, draft: ComposeDraft) {
  const ev = new CustomEvent<{ thread: ThreadId; draft: ComposeDraft; taken: boolean }>(EVENT, { detail: { thread, draft, taken: false } });
  window.dispatchEvent(ev);
  if (ev.detail.taken) return;
  // No composer open for it: into the saved draft (its images can't wait there; text only).
  try {
    localStorage.setItem(draftKey(thread), appendDraft(localStorage.getItem(draftKey(thread)) ?? "", draft.text));
  } catch {
    // Private mode: nothing to keep it in.
  }
}

/** A composer listens for its thread's drafts. Returns the unsubscribe. */
export function onCompose(thread: ThreadId, take: (draft: ComposeDraft) => void): () => void {
  const listener = (e: Event) => {
    const d = (e as CustomEvent<{ thread: ThreadId; draft: ComposeDraft; taken: boolean }>).detail;
    if (d.thread !== thread || d.taken) return;
    d.taken = true;
    take(d.draft);
  };
  window.addEventListener(EVENT, listener);
  return () => window.removeEventListener(EVENT, listener);
}
