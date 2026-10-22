import { describe, expect, it } from "vitest";
import { inlineText, parseInline, parseMarkdown, safeHref } from "./markdown";

describe("inline", () => {
  it("parses code, bold, italics, strikethrough and links", () => {
    expect(parseInline("a `b` **c** *d* _e_ ~~f~~ [g](https://x.dev)")).toEqual([
      { t: "text", v: "a " },
      { t: "code", v: "b" },
      { t: "text", v: " " },
      { t: "strong", c: [{ t: "text", v: "c" }] },
      { t: "text", v: " " },
      { t: "em", c: [{ t: "text", v: "d" }] },
      { t: "text", v: " " },
      { t: "em", c: [{ t: "text", v: "e" }] },
      { t: "text", v: " " },
      { t: "del", c: [{ t: "text", v: "f" }] },
      { t: "text", v: " " },
      { t: "link", href: "https://x.dev", c: [{ t: "text", v: "g" }] },
    ]);
  });

  it("leaves unmatched markers and snake_case alone", () => {
    expect(inlineText(parseInline("2 * 3 = 6, **open, `tick, snake_case_name"))).toBe("2 * 3 = 6, **open, `tick, snake_case_name");
    expect(parseInline("snake_case_name")).toEqual([{ t: "text", v: "snake_case_name" }]);
  });

  it("keeps markdown inside code literal", () => {
    expect(parseInline("`**not bold**`")).toEqual([{ t: "code", v: "**not bold**" }]);
    expect(parseInline("`` a ` b ``")).toEqual([{ t: "code", v: "a ` b" }]);
  });

  it("autolinks bare URLs without trailing punctuation", () => {
    expect(parseInline("see https://roll.sh/acme/web.")).toEqual([
      { t: "text", v: "see " },
      { t: "link", href: "https://roll.sh/acme/web", c: [{ t: "text", v: "https://roll.sh/acme/web" }] },
      { t: "text", v: "." },
    ]);
  });

  it("drops unsafe link targets but keeps their text", () => {
    expect(parseInline("[click](javascript:alert(1))")).toEqual([{ t: "text", v: "click" }, { t: "text", v: ")" }]);
    expect(parseInline("[x](data:text/html,hi)")).toEqual([{ t: "text", v: "x" }]);
    expect(safeHref("javascript:alert(1)")).toBeNull();
    expect(safeHref("JavaScript:alert(1)")).toBeNull();
    expect(safeHref("//evil.dev")).toBeNull();
    expect(safeHref("https://ok.dev")).toBe("https://ok.dev");
    expect(safeHref("mailto:a@b.c")).toBe("mailto:a@b.c");
    expect(safeHref("/acme/web/blob/main/a.rs")).toBe("/acme/web/blob/main/a.rs");
    expect(safeHref("#L10")).toBe("#L10");
    expect(safeHref("./a.md")).toBe("./a.md");
    // Backslashes (`/\evil.dev` is protocol-relative to a browser) and control characters.
    expect(safeHref("/\\evil.dev")).toBeNull();
    expect(safeHref("/\\/evil.dev")).toBeNull();
    expect(safeHref("java\tscript:alert(1)")).toBeNull();
    expect(safeHref("/ok\npath")).toBeNull();
    expect(parseInline("[x](/\\evil.dev)")).toEqual([{ t: "text", v: "x" }]);
  });

  it("handles escapes and hard breaks", () => {
    expect(parseInline("\\*not em\\*")).toEqual([{ t: "text", v: "*not em*" }]);
    expect(parseInline("one  \ntwo")).toEqual([{ t: "text", v: "one" }, { t: "br" }, { t: "text", v: "two" }]);
    expect(parseInline("one\ntwo")).toEqual([{ t: "text", v: "one\ntwo" }]);
  });

  it("keeps raw HTML as text", () => {
    expect(parseInline("<img src=x onerror=alert(1)>")).toEqual([{ t: "text", v: "<img src=x onerror=alert(1)>" }]);
  });
});

describe("blocks", () => {
  it("parses paragraphs, headings, lists, quotes, rules and fences", () => {
    const src = [
      "## Walkthrough",
      "",
      "Changes the **lander**.",
      "Second line.",
      "",
      "- one",
      "- two",
      "  continued",
      "",
      "1. first",
      "2. second",
      "",
      "> quoted *text*",
      "> more",
      "",
      "---",
      "",
      "```rust",
      "fn main() {}",
      "",
      "```",
    ].join("\n");
    expect(parseMarkdown(src)).toEqual([
      { t: "h", level: 2, c: [{ t: "text", v: "Walkthrough" }] },
      { t: "p", c: [{ t: "text", v: "Changes the " }, { t: "strong", c: [{ t: "text", v: "lander" }] }, { t: "text", v: ".\nSecond line." }] },
      { t: "list", ordered: false, items: [[{ t: "text", v: "one" }], [{ t: "text", v: "two\ncontinued" }]] },
      { t: "list", ordered: true, items: [[{ t: "text", v: "first" }], [{ t: "text", v: "second" }]] },
      {
        t: "quote",
        c: [{ t: "p", c: [{ t: "text", v: "quoted " }, { t: "em", c: [{ t: "text", v: "text" }] }, { t: "text", v: "\nmore" }] }],
      },
      { t: "hr" },
      { t: "code", lang: "rust", v: "fn main() {}\n" },
    ]);
  });

  it("keeps an unclosed fence open to the end, and longer fences intact", () => {
    expect(parseMarkdown("```\na\nb")).toEqual([{ t: "code", lang: "", v: "a\nb" }]);
    expect(parseMarkdown("````md\n```\ninner\n```\n````")).toEqual([{ t: "code", lang: "md", v: "```\ninner\n```" }]);
    expect(parseMarkdown("~~~\nx\n~~~")).toEqual([{ t: "code", lang: "", v: "x" }]);
  });

  it("caps heading levels and starts lists right after a paragraph line", () => {
    expect(parseMarkdown("###### tiny")).toEqual([{ t: "h", level: 4, c: [{ t: "text", v: "tiny" }] }]);
    expect(parseMarkdown("intro\n- item")).toEqual([
      { t: "p", c: [{ t: "text", v: "intro" }] },
      { t: "list", ordered: false, items: [[{ t: "text", v: "item" }]] },
    ]);
    expect(parseMarkdown("")).toEqual([]);
    expect(parseMarkdown("a\r\n\r\nb")).toEqual([
      { t: "p", c: [{ t: "text", v: "a" }] },
      { t: "p", c: [{ t: "text", v: "b" }] },
    ]);
  });
});

describe("roll review bodies", () => {
  it("parses pipe tables with escaped pipes and alignment", () => {
    const [t] = parseMarkdown("| File | Change |\n| :--- | ---: |\n| `a\\|b.rs` | Adds **x** |\n| c.rs |");
    expect(t.t).toBe("table");
    if (t.t !== "table") return;
    expect(t.align).toEqual(["left", "right"]);
    expect(t.head.map(inlineText)).toEqual(["File", "Change"]);
    expect(t.rows.map((r) => r.map(inlineText))).toEqual([
      ["a|b.rs", "Adds x"],
      ["c.rs", ""],
    ]);
    expect(t.rows[0][0]).toEqual([{ t: "code", v: "a|b.rs" }]);
  });

  it("ends a paragraph at a table and needs a separator row", () => {
    const blocks = parseMarkdown("Walkthrough.\n| a | b |\n| - | - |\n| 1 | 2 |");
    expect(blocks.map((b) => b.t)).toEqual(["p", "table"]);
    expect(parseMarkdown("a | b\nc | d").map((b) => b.t)).toEqual(["p"]);
  });

  it("parses <details> with a summary, on one line or two, and nests", () => {
    const [d] = parseMarkdown("<details><summary>Not reviewed</summary>\n\n- `Cargo.lock`: ignored\n\n</details>");
    expect(d.t).toBe("details");
    if (d.t !== "details") return;
    expect(inlineText(d.summary)).toBe("Not reviewed");
    expect(d.c.map((b) => b.t)).toEqual(["list"]);
    const [d2] = parseMarkdown("<details>\n<summary>Outer</summary>\n<details><summary>Inner</summary>\nx\n</details>\nafter\n</details>\ntail");
    if (d2.t !== "details") throw new Error("expected details");
    expect(inlineText(d2.summary)).toBe("Outer");
    expect(d2.c.map((b) => b.t)).toEqual(["details", "p"]);
    expect(parseMarkdown("<details>\nbody").map((b) => b.t)).toEqual(["details"]);
  });

  it("parses <sub> inline and keeps other HTML as text", () => {
    expect(parseInline("x <sub>confidence 84%</sub>")).toEqual([
      { t: "text", v: "x " },
      { t: "sub", c: [{ t: "text", v: "confidence 84%" }] },
    ]);
    expect(inlineText(parseInline("<b>bold</b>"))).toBe("<b>bold</b>");
    expect(inlineText(parseInline("<sub>unclosed"))).toBe("<sub>unclosed");
  });

  it("keeps mermaid blocks as code with their language", () => {
    const [c] = parseMarkdown("```mermaid\nsequenceDiagram\nA->>B: hi\n```");
    expect(c).toEqual({ t: "code", lang: "mermaid", v: "sequenceDiagram\nA->>B: hi" });
  });
});
