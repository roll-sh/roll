import { describe, expect, it } from "vitest";
import type { Preview } from "@/lib/previews-api";
import { browserOf, feedbackBody, feedbackFrom } from "./preview-feedback";

describe("feedback from a preview page", () => {
  const p = { id: "pv_1", url: "https://web-pr7-shop.preview.roll.site/" } as Preview;

  it("reads the button's note and posts where it was, with the page's names kept inert", () => {
    expect(feedbackFrom("fb_path=/x")).toBeUndefined();
    expect(feedbackFrom("fb_path=/x&fb_note=42")?.note, "a note that looks like JSON stays text").toBe("42");
    const f = feedbackFrom(new URLSearchParams({ fb_path: "/checkout?step=2", fb_selector: "button.pay`\n**hi**", fb_viewport: "1280x800", fb_note: "  Too small  " }).toString())!;
    expect(feedbackBody(p, f, f.note)).toBe(
      "**Feedback on the preview** · [`/checkout?step=2`](https://web-pr7-shop.preview.roll.site/checkout?step=2) · `button.pay**hi**`\n\nToo small\n\n<sub>1280x800 viewport · roll preview pv_1</sub>",
    );
    const odd = feedbackBody(p, { path: "//evil.example/a b)(", selector: "", note: "x" }, "x");
    expect(odd).toContain("[`/evil.example/ab`](https://web-pr7-shop.preview.roll.site/evil.example/ab)");
  });

  it("carries what went wrong on the page and the browser, inert, after the note and only if kept", () => {
    const mac = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
    const f = feedbackFrom(
      new URLSearchParams({
        fb_path: "/cart",
        fb_note: "Broken",
        fb_errors: JSON.stringify(["TypeError: x is `undefined`\n**hi**", 42]),
        fb_requests: JSON.stringify(["/api/cart 500"]),
        fb_ua: mac,
      }).toString(),
    )!;
    expect(feedbackBody(p, f, f.note)).toBe(
      "**Feedback on the preview** · [`/cart`](https://web-pr7-shop.preview.roll.site/cart)\n\nBroken\n\n<sub>Chrome 131 on macOS · roll preview pv_1</sub>\n\n<details><summary>On the page: 1 error, 1 failed request</summary>\n\n- error: `TypeError: x is undefined**hi**`\n- `/api/cart 500`\n\n</details>",
    );
    expect(feedbackBody(p, f, f.note, false)).not.toContain("details");
    expect(feedbackFrom("fb_path=/&fb_note=x&fb_errors=not-json")?.errors).toEqual([]);
    expect(browserOf("Mozilla/5.0 (iPhone; CPU iPhone OS 18_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.1 Mobile/15E148 Safari/604.1")).toBe("Safari 18 on iOS");
    expect(browserOf(mac.replace("Safari/537.36", "Safari/537.36 Edg/131.0.0.0"))).toBe("Edge 131 on macOS");
  });
});
