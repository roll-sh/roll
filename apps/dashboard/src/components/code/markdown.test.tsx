import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { comment, review, thread, user } from "@/test/fixtures";
import { Markdown } from "./markdown";
import { DraftCard, LineComposer, ReviewTimeline } from "./review";
import { ThreadCard } from "./thread";

/**
 * Markdown from comments, review bodies, drafts and the composer's preview, through the components
 * that show it: nothing executable may reach the DOM. src/lib/markdown.test.ts covers the parser.
 */

const ACTIVE = "script, iframe, frame, object, embed, img, math, style, link, meta, base, form, input, video, audio, source";
const SAFE_HREF = /^(https?:|mailto:|\/(?!\/)|#|\.\.?\/)/i;

/**
 * No active elements, no handler attributes, and only safe link targets anywhere; inside the
 * rendered markdown, no SVG at all and no style but table alignment. (The cards' own chrome has
 * lucide icons and sized avatars, which aren't from the body.)
 */
function expectInert(root: HTMLElement) {
  expect(root.querySelectorAll(ACTIVE)).toHaveLength(0);
  for (const svg of root.querySelectorAll("svg")) expect(svg.getAttribute("class") ?? "").toMatch(/\blucide\b/);
  for (const el of root.querySelectorAll("*")) {
    for (const a of el.attributes) {
      expect(a.name.startsWith("on"), `${el.tagName} has ${a.name}`).toBe(false);
      if (["src", "srcset", "action", "formaction", "xlink:href", "srcdoc"].includes(a.name)) throw new Error(`${el.tagName} has ${a.name}`);
    }
  }
  for (const md of root.querySelectorAll(".markdown")) {
    expect(md.querySelectorAll("svg")).toHaveLength(0);
    for (const el of md.querySelectorAll("[style]")) expect(el.getAttribute("style")).toMatch(/^text-align: (left|center|right);?$/);
  }
  for (const a of root.querySelectorAll("a")) {
    const href = a.getAttribute("href") ?? "";
    expect(href, `link ${href}`).toMatch(SAFE_HREF);
    expect(href).not.toMatch(/[\\\u0000-\u001f\u007f]/);
    if (a.getAttribute("target") === "_blank") expect(a.getAttribute("rel")).toContain("noreferrer");
  }
}

const BAD_LINKS: [string, string][] = [
  ["javascript:", "[click](javascript:alert(1))"],
  ["mixed-case javascript:", "[click](JaVaScRiPt:alert(1))"],
  ["percent-encoded javascript:", "[click](%6Aavascript:alert(1))"],
  ["data:", "[click](data:text/html;base64,PHNjcmlwdD5hbGVydCgxKTwvc2NyaXB0Pg==)"],
  ["data: svg", "[click](data:image/svg+xml,<svg/onload=alert(1)>)"],
  ["vbscript:", "[click](vbscript:msgbox(1))"],
  ["file:", "[click](file:///etc/passwd)"],
  ["backslash path", "[click](/\\evil.dev)"],
  ["leading backslashes", "[click](\\\\evil.dev)"],
  ["https with a backslash", "[click](https:/\\evil.dev)"],
  ["protocol-relative", "[click](//evil.dev)"],
  ["tab inside the scheme", "[click](java\tscript:alert(1))"],
  ["control character inside the scheme", "[click](java\u0001script:alert(1))"],
  ["DEL inside the path", "[click](/\u007f/evil.dev)"],
  ["newline inside the scheme", "[click](java\nscript:alert(1))"],
];

const RAW_HTML: [string, string][] = [
  ["<script>", "<script>alert(1)</script>"],
  ["<img onerror>", '<img src=x onerror="alert(1)">'],
  ["<iframe>", '<iframe src="javascript:alert(1)"></iframe>'],
  ["srcdoc iframe", '<iframe srcdoc="<script>alert(1)</script>"></iframe>'],
  ["<svg onload>", "<svg onload=alert(1)><circle r=1 /></svg>"],
  ["<svg><script>", '<svg><script xlink:href="data:,alert(1)"></script></svg>'],
  ["<svg> <a xlink:href>", '<svg><a xlink:href="javascript:alert(1)"><text>x</text></a></svg>'],
  ["<math> link", '<math><mi xlink:href="javascript:alert(1)">x</mi></math>'],
  ["style attribute", '<p style="background:url(javascript:alert(1))">x</p>'],
  ["<style>", "<style>body{display:none}</style>"],
  ["event handler", '<a href="https://ok.dev" onclick="alert(1)">x</a>'],
  ["raw javascript: anchor", '<a href="javascript:alert(1)">x</a>'],
  ["<object>", '<object data="javascript:alert(1)"></object>'],
  ["<form>", '<form action="javascript:alert(1)"><button>go</button></form>'],
  ["<meta refresh>", '<meta http-equiv="refresh" content="0;url=javascript:alert(1)">'],
  ["<base>", '<base href="https://evil.dev/">'],
  ["details with handlers", '<details open ontoggle="alert(1)">\n<summary>x</summary>\n</details>'],
  ["handler in a summary", "<details>\n<summary><img src=x onerror=alert(1)></summary>\nbody\n</details>"],
  ["handler in <sub>", "<sub><img src=x onerror=alert(1)></sub>"],
  ["handler in a table cell", "| a | b |\n|:-:|--:|\n| <img src=x onerror=alert(1)> | <script>alert(1)</script> |"],
  ["link title breakout", '[x](https://ok.dev" onmouseover="alert(1))'],
  ["autolink with a quote", 'see https://ok.dev/"onmouseover=alert(1) now'],
  ["link text with HTML", "[<img src=x onerror=alert(1)>](https://ok.dev)"],
];

/** Every place a comment, review or draft body is shown, rendering `body`. */
const SURFACES: [string, (body: string) => HTMLElement][] = [
  ["a thread comment", (body) => render(<ThreadCard thread={thread({ body })} />).container],
  ["a review body", (body) => render(<ReviewTimeline reviews={[review({ body })]} status={null} />).container],
  [
    "a pending draft",
    (body) => render(<DraftCard draft={{ id: "d1", path: "a.ts", line: 1, body }} original={null} onEdit={() => {}} onRemove={() => {}} />).container,
  ],
  ["Markdown itself", (body) => render(<Markdown source={body} />).container],
];

describe("Markdown in the dashboard: links", () => {
  for (const [surface, show] of SURFACES) {
    it.each(BAD_LINKS)(`${surface} drops a %s link and keeps its text`, (_name, body) => {
      const root = show(body);
      expectInert(root);
      expect(root.querySelector("a[href]:not([href^='http']):not([href^='/'])")).toBeNull();
      expect(root).toHaveTextContent("click");
    });
  }

  it("keeps http(s), mailto, same-site and fragment links, with rel on new tabs", () => {
    const root = render(
      <ThreadCard
        thread={thread({
          body: "[a](https://roll.sh/docs) [b](http://x.dev) [c](mailto:ada@x.dev) [d](/acme/web) [e](#L3) [f](./x.md) [g](../y.md) and https://auto.dev/x.",
        })}
      />,
    ).container;
    expectInert(root);
    const hrefs = [...root.querySelectorAll("a")].map((a) => [a.textContent, a.getAttribute("href"), a.getAttribute("target")]);
    expect(hrefs).toEqual([
      ["a", "https://roll.sh/docs", "_blank"],
      ["b", "http://x.dev", "_blank"],
      ["c", "mailto:ada@x.dev", null],
      ["d", "/acme/web", null],
      ["e", "#L3", null],
      ["f", "./x.md", null],
      ["g", "../y.md", null],
      ["https://auto.dev/x", "https://auto.dev/x", "_blank"],
    ]);
    for (const a of root.querySelectorAll("a")) expect(a).toHaveAttribute("rel", "noreferrer nofollow");
  });
});

describe("Markdown in the dashboard: raw HTML stays text", () => {
  for (const [surface, show] of SURFACES) {
    it.each(RAW_HTML)(`${surface}: %s`, (_name, body) => {
      const root = show(body);
      expectInert(root);
    });
  }

  it("shows the markup as text rather than dropping it", () => {
    const root = render(<ThreadCard thread={thread({ body: '<script>alert("x")</script> and <img src=x onerror=alert(1)>' })} />).container;
    expect(root).toHaveTextContent('<script>alert("x")</script> and <img src=x onerror=alert(1)>');
    expectInert(root);
  });

  it("a fenced block, a mermaid diagram and inline code stay source", () => {
    const body = "```html\n<script>alert(1)</script>\n```\n\n```mermaid\nclick A \"javascript:alert(1)\"\n```\n\n`<iframe src=x>`";
    const root = render(<Markdown source={body} />).container;
    expectInert(root);
    const code = [...root.querySelectorAll("code")].map((c) => c.textContent);
    expect(code).toEqual(["<script>alert(1)</script>", 'click A "javascript:alert(1)"', "<iframe src=x>"]);
    expect(within(root).getByText("diagram")).toBeInTheDocument();
  });

  it("table alignment is the only style, and only from the separator row", () => {
    const root = render(<Markdown source={"| a | b | c |\n|:-|:-:|-:|\n| 1 | 2 | 3 |"} />).container;
    expectInert(root);
    const styles = [...root.querySelectorAll("th")].map((th) => th.getAttribute("style"));
    expect(styles).toEqual(["text-align: left;", "text-align: center;", "text-align: right;"]);
  });

  it("a suggestion's lines render as text in the mini-diff", () => {
    const c = comment({ body: "```suggestion\n<img src=x onerror=alert(1)>\n```", suggestion: "<img src=x onerror=alert(1)>" });
    const root = render(<ThreadCard thread={{ ...thread(), comments: [c] }} original={["<script>old</script>"]} />).container;
    expectInert(root);
    expect(screen.getByText("Suggested change")).toBeInTheDocument();
    expect(root).toHaveTextContent("<img src=x onerror=alert(1)>");
    expect(root).toHaveTextContent("<script>old</script>");
  });

  it("a dismissal reason and reviewer names are text", () => {
    const root = render(
      <ReviewTimeline
        reviews={[
          review({
            reviewer: user('<img src=x onerror="alert(1)">'),
            verdict: "approve",
            dismissed: true,
            dismissed_by: "<script>alert(1)</script>",
            dismiss_reason: '"><img src=x onerror=alert(1)>',
          }),
        ]}
        status={null}
      />,
    ).container;
    expectInert(root);
    expect(root).toHaveTextContent("Dismissed: \"><img src=x onerror=alert(1)>");
  });
});

describe("Markdown in the composer's preview", () => {
  it("previews what's typed through the same renderer, inert", async () => {
    const u = userEvent.setup();
    const { container } = render(
      <LineComposer
        selection={{ path: "a.ts", side: "new", start: 1, end: 1 }}
        original={null}
        busy={null}
        hasPending={false}
        onAdd={vi.fn()}
        onPostNow={vi.fn(async () => true)}
        onCancel={vi.fn()}
      />,
    );
    const box = screen.getByPlaceholderText(/Leave a comment/);
    // user-event treats `[` and `{` as key descriptors; paste the payload instead.
    await u.click(box);
    await u.paste("**hi** [x](javascript:alert(1)) [ok](https://ok.dev) <img src=x onerror=alert(1)> <svg onload=alert(1)>");
    await u.click(screen.getByRole("button", { name: /preview/ }));
    expect(screen.queryByPlaceholderText(/Leave a comment/)).toBeNull();
    expectInert(container);
    expect(container.querySelector("strong")).toHaveTextContent("hi");
    expect([...container.querySelectorAll("a")].map((a) => a.getAttribute("href"))).toEqual(["https://ok.dev"]);
    expect(container).toHaveTextContent("<img src=x onerror=alert(1)> <svg onload=alert(1)>");
  });
});
