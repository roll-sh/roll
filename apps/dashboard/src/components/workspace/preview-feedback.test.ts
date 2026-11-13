import { describe, expect, it } from "vitest";
import type { Preview } from "@/lib/previews-api";
import { feedbackBody, feedbackFrom } from "./preview-feedback";

describe("feedback from a preview page", () => {
  const p = { id: "pv_1", url: "https://web-pr7-shop.preview.roll.site/" } as Preview;

  it("reads the button's note and posts where it was, with the page's names kept inert", () => {
    expect(feedbackFrom({ fb_path: "/x" })).toBeUndefined();
    const f = feedbackFrom({ fb_path: "/checkout?step=2", fb_selector: "button.pay`\n**hi**", fb_viewport: "1280x800", fb_note: "  Too small  " })!;
    expect(feedbackBody(p, f, f.note)).toBe(
      "**Feedback on the preview** · [`/checkout?step=2`](https://web-pr7-shop.preview.roll.site/checkout?step=2) · `button.pay**hi**`\n\nToo small\n\n<sub>1280x800 viewport · roll preview pv_1</sub>",
    );
    const odd = feedbackBody(p, { path: "//evil.example/a b)(", selector: "", note: "x" }, "x");
    expect(odd).toContain("[`/evil.example/ab`](https://web-pr7-shop.preview.roll.site/evil.example/ab)");
  });
});
