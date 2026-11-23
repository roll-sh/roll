import { act, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { EventPage, EventsQuery, SecretsEvent } from "@/lib/secrets-api";
import { EventsFeed, POLL_MS } from "./events";

afterEach(() => vi.useRealTimers());

const event = (seq: number, key: string): SecretsEvent => ({ seq, at: 0, kind: "secret.changed", environment: "production", key, data: { actor: "operator", via: "write" } });

/** A fake feed: its head at 40, then each poll's page by the cursor it's asked with. */
function feed(polls: Record<number, EventPage>) {
  return vi.fn(async (q: EventsQuery): Promise<EventPage> => {
    if (q.latest) return { events: [], next: 40 };
    if (q.after !== undefined && polls[q.after]) return polls[q.after];
    return { events: [], next: 40 };
  });
}

const flush = () => act(async () => {});

describe("the Events tab", () => {
  it("follows next from one poll to the next", async () => {
    vi.useFakeTimers();
    // The first poll's next (55) is past its last event (52): rows a filter left out.
    const load = feed({ 40: { events: [event(52, "DB_URL")], next: 55 }, 55: { events: [event(61, "API_KEY")], next: 61 } });
    render(<EventsFeed load={load} envs={[]} project="shop" />);
    await flush();
    await act(async () => void vi.advanceTimersByTime(POLL_MS));
    await act(async () => void vi.advanceTimersByTime(POLL_MS));
    await act(async () => void vi.advanceTimersByTime(POLL_MS));

    const afters = load.mock.calls.map(([q]) => q.after).filter((a) => a !== undefined && a >= 40);
    expect(afters).toEqual([40, 55, 61]);
    expect(screen.getByText("API_KEY")).toBeTruthy();
    expect(screen.getByText("DB_URL")).toBeTruthy();
  });

  it("sends a kind chip's kinds to the server", async () => {
    const load = feed({});
    render(<EventsFeed load={load} envs={[]} project="shop" />);
    await flush();
    load.mockClear();
    await act(async () => void fireEvent.click(screen.getByRole("button", { name: "Rotations" })));
    await flush();
    expect(load).toHaveBeenCalled();
    for (const [q] of load.mock.calls) expect(q.kinds).toEqual(["rotation.*"]);
  });
});
