import { describe, expect, it } from "vitest";
import type { ErrorGroup } from "./previews-api";
import { workFromError } from "./preview-work";

describe("a preview's error made a work item", () => {
  const p = { repo: "acme/web", number: 7 };

  it("keeps an error's sample in a block it can't close", () => {
    const g = { service: "web", fingerprint: "f", kind: "line", title: "TypeError: x\nis undefined", count: 3, first_seen: 0, last_seen: 0, sample: "a ```` b", trace: null } as unknown as ErrorGroup;
    const w = workFromError(p, g);
    expect(w.title).toBe("web: TypeError: x is undefined");
    expect(w.body).toBe("Seen 3 times in the preview of acme/web#7 (`web`), last at 1970-01-01T00:00:00.000Z.\n\n`````\na ```` b\n`````");
  });
});
