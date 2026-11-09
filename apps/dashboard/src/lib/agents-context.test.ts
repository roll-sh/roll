import { describe, expect, it } from "vitest";
import { compactionNote, compactsOnReturn, RESUME_COMPACTION_FALLBACK_MS } from "./agents-context";

describe("compaction (T3's ContextWindowMeter.logic)", () => {
  const big = { used_tokens: 312_000, max_tokens: 1_000_000, total_processed_tokens: null, cache_ttl_ms: 3_600_000 };
  const base = { provider: "claude", context: big, idleSince: 0, now: 3_600_000, mode: "on_return" as const, canCompact: true };

  it("says where Claude compacts when a setting moved it, else that it compacts when needed", () => {
    expect(compactionNote({ ...big, auto_compact_at: 267_000 })).toBe("Compacts automatically at about 267,000 tokens.");
    expect(compactionNote(big)).toBe("Context compacts automatically when needed.");
  });

  it("compacts first on return once a big conversation's prompt cache has expired", () => {
    expect(compactsOnReturn(base)).toBe(true);
    expect(compactsOnReturn({ ...base, now: 3_599_999 })).toBe(false);
    expect(compactsOnReturn({ ...base, context: { ...big, used_tokens: 199_999 } })).toBe(false);
    for (const mode of ["before_cache_expires", "off"] as const) expect(compactsOnReturn({ ...base, mode })).toBe(false);
    expect(compactsOnReturn({ ...base, canCompact: false })).toBe(false);
    expect(compactsOnReturn({ ...base, idleSince: null })).toBe(false);
    // Without a reported lifetime: Claude's fallback; another provider not at all.
    const unknown = { ...big, cache_ttl_ms: null };
    expect(compactsOnReturn({ ...base, context: unknown, now: RESUME_COMPACTION_FALLBACK_MS - 1 })).toBe(false);
    expect(compactsOnReturn({ ...base, context: unknown, now: RESUME_COMPACTION_FALLBACK_MS })).toBe(true);
    expect(compactsOnReturn({ ...base, provider: "codex", context: unknown, now: RESUME_COMPACTION_FALLBACK_MS })).toBe(false);
  });
});

