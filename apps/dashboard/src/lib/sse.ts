/**
 * Server-sent events over `fetch`, so requests can carry `Authorization` (EventSource can't send
 * headers, and private repos need the token). Reconnects with backoff after a drop, like
 * EventSource does.
 */

export interface SseEvent {
  event: string;
  data: string;
  id?: string;
}

/** Split complete events off the front of `buffer`; returns them and what's left (a partial event). */
export function parseSse(buffer: string): { events: SseEvent[]; rest: string } {
  const events: SseEvent[] = [];
  const text = buffer.replace(/\r\n?/g, "\n");
  let start = 0;
  for (;;) {
    const end = text.indexOf("\n\n", start);
    if (end === -1) break;
    const block = text.slice(start, end);
    start = end + 2;
    let event = "message";
    let id: string | undefined;
    const data: string[] = [];
    let any = false;
    for (const line of block.split("\n")) {
      if (!line || line.startsWith(":")) continue;
      const i = line.indexOf(":");
      const field = i === -1 ? line : line.slice(0, i);
      let value = i === -1 ? "" : line.slice(i + 1);
      if (value.startsWith(" ")) value = value.slice(1);
      if (field === "event") event = value;
      else if (field === "data") {
        data.push(value);
        any = true;
      } else if (field === "id") id = value;
    }
    if (any) events.push({ event, data: data.join("\n"), ...(id !== undefined ? { id } : {}) });
  }
  return { events, rest: text.slice(start) };
}

export interface SseOptions {
  headers?: () => Record<string, string>;
  onEvent: (e: SseEvent) => void;
  onOpen?: (open: boolean) => void;
  /** Delays between reconnects, in ms; the last one repeats. */
  backoff?: number[];
  fetch?: typeof fetch;
}

/** Follow `url` until the returned function is called. */
export function streamSse(url: string, opts: SseOptions): () => void {
  const ctrl = new AbortController();
  const backoff = opts.backoff ?? [500, 1000, 2000, 5000, 10000];
  const doFetch = opts.fetch ?? fetch;
  let attempt = 0;
  (async () => {
    while (!ctrl.signal.aborted) {
      try {
        const res = await doFetch(url, { headers: { accept: "text/event-stream", ...opts.headers?.() }, signal: ctrl.signal, cache: "no-store" });
        if (!res.ok || !res.body) throw new Error(`${res.status}`);
        opts.onOpen?.(true);
        attempt = 0;
        const reader = res.body.getReader();
        const decoder = new TextDecoder();
        let buffer = "";
        for (;;) {
          const { value, done } = await reader.read();
          if (done) break;
          buffer += decoder.decode(value, { stream: true });
          const { events, rest } = parseSse(buffer);
          buffer = rest;
          for (const e of events) opts.onEvent(e);
        }
      } catch {
        // Dropped, refused or aborted: reconnect below unless we're done.
      }
      if (ctrl.signal.aborted) break;
      opts.onOpen?.(false);
      const delay = backoff[Math.min(attempt++, backoff.length - 1)];
      await new Promise((r) => setTimeout(r, delay));
    }
  })();
  return () => ctrl.abort();
}
