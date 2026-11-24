import { describe, expect, it } from "vitest";
import type { ErrorGroup } from "./previews-api";
import { workFromError, workFromNote } from "./preview-work";

describe("a preview's note or error made a work item", () => {
  const p = { repo: "acme/web", number: 7 };

  it("takes the note's first line as its title and its whole review as its body", () => {
    const review = "**Feedback on the preview** · [`/cart`](https://web-pr7.preview.roll.site/cart) · `button.pay`\n\nThe total is `wrong`\nafter a coupon\n\n<sub>Chrome 131 on macOS · roll preview pv_1</sub>";
    const w = workFromNote(p, "rv_1", review);
    expect(w.title).toBe("Preview feedback: The total is wrong");
    expect(w.body).toBe(`${review}\n\nFrom a note on the preview of acme/web#7 (review \`rv_1\`).`);
    expect(w.labels).toEqual(["preview-feedback"]);
  });

  it("keeps an error's sample in a block it can't close", () => {
    const g = { service: "web", fingerprint: "f", kind: "line", title: "TypeError: x\nis undefined", count: 3, first_seen: 0, last_seen: 0, sample: "a ```` b", trace: null } as unknown as ErrorGroup;
    const w = workFromError(p, g);
    expect(w.title).toBe("web: TypeError: x is undefined");
    expect(w.body).toBe("Seen 3 times in the preview of acme/web#7 (`web`), last at 1970-01-01T00:00:00.000Z.\n\n`````\na ```` b\n`````");
  });
});
