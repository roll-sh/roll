/**
 * A small markdown parser for review comments: paragraphs, headings, lists, quotes, fenced code,
 * rules, pipe tables, `<details>` blocks, and inline code, bold, italics, strikethrough, links and
 * `<sub>`. It returns tokens, never HTML, so the page renders them as React nodes and other raw
 * HTML in a comment (bots send plenty) stays text. READMEs are rendered by the server instead.
 */

export type Inline =
  | { t: "text"; v: string }
  | { t: "code"; v: string }
  | { t: "strong"; c: Inline[] }
  | { t: "em"; c: Inline[] }
  | { t: "del"; c: Inline[] }
  | { t: "link"; href: string; c: Inline[] }
  | { t: "sub"; c: Inline[] }
  | { t: "br" };

export type Align = "left" | "center" | "right" | null;

export type Block =
  | { t: "p"; c: Inline[] }
  | { t: "h"; level: 1 | 2 | 3 | 4; c: Inline[] }
  | { t: "code"; lang: string; v: string }
  | { t: "quote"; c: Block[] }
  | { t: "list"; ordered: boolean; items: Inline[][] }
  | { t: "table"; align: Align[]; head: Inline[][]; rows: Inline[][][] }
  | { t: "details"; summary: Inline[]; c: Block[] }
  | { t: "hr" };

/** http(s), mailto and same-site paths; anything else (javascript:, data:) is dropped. */
export function safeHref(url: string): string | null {
  const u = url.trim();
  // Browsers read `\` as `/` (so `/\evil.com` is protocol-relative) and drop tabs and newlines
  // inside URLs: refuse both rather than guess.
  if (/[\\\u0000-\u001f\u007f]/.test(u)) return null;
  if (/^(https?:|mailto:)/i.test(u)) return u;
  if (/^(\/(?!\/)|#|\.\.?\/)/.test(u)) return u;
  return null;
}

const FENCE = /^[ \t]{0,3}(`{3,}|~{3,})[ \t]*([^\s`]*)[^\n]*$/;
const HEADING = /^[ \t]{0,3}(#{1,6})[ \t]+(.*?)[ \t#]*$/;
const BULLET = /^[ \t]{0,3}[-*+][ \t]+(.*)$/;
const ORDERED = /^[ \t]{0,3}\d{1,9}[.)][ \t]+(.*)$/;
const QUOTE = /^[ \t]{0,3}>[ \t]?(.*)$/;
const RULE = /^[ \t]{0,3}([-*_])([ \t]*\1){2,}[ \t]*$/;
const TABLE_SEP = /^[ \t]*\|?[ \t]*:?-+:?[ \t]*(\|[ \t]*:?-+:?[ \t]*)*\|?[ \t]*$/;
const DETAILS = /^[ \t]*<details>[ \t]*(?:<summary>(.*?)<\/summary>)?[ \t]*$/i;
const DETAILS_END = /^[ \t]*<\/details>[ \t]*$/i;

/** A table row's cells: split on unescaped pipes (`\|` is a literal pipe), outer pipes dropped. */
export function tableCells(line: string): string[] {
  const cells: string[] = [];
  let cur = "";
  for (let i = 0; i < line.length; i++) {
    if (line[i] === "\\" && line[i + 1] === "|") {
      cur += "|";
      i++;
    } else if (line[i] === "|") {
      cells.push(cur);
      cur = "";
    } else cur += line[i];
  }
  cells.push(cur);
  if (cells.length > 1 && !cells[0].trim()) cells.shift();
  if (cells.length > 1 && !cells[cells.length - 1].trim()) cells.pop();
  return cells.map((c) => c.trim());
}

const isTableStart = (lines: string[], i: number) =>
  lines[i].includes("|") && i + 1 < lines.length && TABLE_SEP.test(lines[i + 1]) && lines[i + 1].includes("-");

export function parseMarkdown(src: string): Block[] {
  const lines = src.replace(/\r\n?/g, "\n").split("\n");
  const out: Block[] = [];
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    if (!line.trim()) {
      i++;
      continue;
    }
    const fence = FENCE.exec(line);
    if (fence) {
      const close = new RegExp(`^[ \\t]{0,3}${fence[1][0] === "`" ? "`" : "~"}{${fence[1].length},}[ \\t]*$`);
      const body: string[] = [];
      i++;
      while (i < lines.length && !close.test(lines[i])) body.push(lines[i++]);
      i++; // the closing fence (or the end)
      out.push({ t: "code", lang: fence[2] ?? "", v: body.join("\n") });
      continue;
    }
    const details = DETAILS.exec(line);
    if (details) {
      let summary = details[1] ?? "";
      i++;
      if (!details[1] && i < lines.length) {
        const sm = /^[ \t]*<summary>(.*?)<\/summary>[ \t]*$/i.exec(lines[i]);
        if (sm) {
          summary = sm[1];
          i++;
        }
      }
      const body: string[] = [];
      let depth = 0;
      while (i < lines.length && !(DETAILS_END.test(lines[i]) && depth === 0)) {
        if (DETAILS.test(lines[i])) depth++;
        else if (DETAILS_END.test(lines[i])) depth--;
        body.push(lines[i++]);
      }
      i++; // </details> (or the end)
      out.push({ t: "details", summary: parseInline(summary || "Details"), c: parseMarkdown(body.join("\n")) });
      continue;
    }
    if (isTableStart(lines, i)) {
      const head = tableCells(line);
      const align: Align[] = tableCells(lines[i + 1]).map((c) =>
        c.startsWith(":") && c.endsWith(":") ? "center" : c.endsWith(":") ? "right" : c.startsWith(":") ? "left" : null,
      );
      i += 2;
      const rows: Inline[][][] = [];
      while (i < lines.length && lines[i].trim() && lines[i].includes("|")) {
        const cells = tableCells(lines[i++]);
        rows.push(head.map((_, j) => parseInline(cells[j] ?? "")));
      }
      out.push({ t: "table", align: head.map((_, j) => align[j] ?? null), head: head.map(parseInline), rows });
      continue;
    }
    if (RULE.test(line)) {
      out.push({ t: "hr" });
      i++;
      continue;
    }
    const heading = HEADING.exec(line);
    if (heading) {
      out.push({ t: "h", level: Math.min(4, heading[1].length) as 1 | 2 | 3 | 4, c: parseInline(heading[2]) });
      i++;
      continue;
    }
    if (QUOTE.test(line)) {
      const body: string[] = [];
      while (i < lines.length && QUOTE.test(lines[i])) body.push(QUOTE.exec(lines[i++])![1]);
      out.push({ t: "quote", c: parseMarkdown(body.join("\n")) });
      continue;
    }
    if (BULLET.test(line) || ORDERED.test(line)) {
      const ordered = !BULLET.test(line);
      const re = ordered ? ORDERED : BULLET;
      const items: string[] = [];
      while (i < lines.length) {
        const m = re.exec(lines[i]);
        if (m) items.push(m[1]);
        else if (lines[i].trim() && /^[ \t]{2,}/.test(lines[i]) && items.length) items[items.length - 1] += `\n${lines[i].trim()}`;
        else break;
        i++;
      }
      out.push({ t: "list", ordered, items: items.map(parseInline) });
      continue;
    }
    const para: string[] = [];
    while (
      i < lines.length &&
      lines[i].trim() &&
      !FENCE.test(lines[i]) &&
      !HEADING.test(lines[i]) &&
      !QUOTE.test(lines[i]) &&
      !RULE.test(lines[i]) &&
      !DETAILS.test(lines[i]) &&
      !(para.length && isTableStart(lines, i)) &&
      !(para.length && (BULLET.test(lines[i]) || ORDERED.test(lines[i])))
    )
      para.push(lines[i++]);
    out.push({ t: "p", c: parseInline(para.join("\n")) });
  }
  return out;
}

/** Inline spans. Unmatched markers stay as text. */
export function parseInline(src: string): Inline[] {
  const out: Inline[] = [];
  let text = "";
  const flush = () => {
    if (text) out.push({ t: "text", v: text });
    text = "";
  };
  let i = 0;
  while (i < src.length) {
    const ch = src[i];
    const rest = src.slice(i);
    if (ch === "\\" && i + 1 < src.length && /[\\`*_~[\]()#>!-]/.test(src[i + 1])) {
      text += src[i + 1];
      i += 2;
      continue;
    }
    if (ch === "\n") {
      // Two trailing spaces (or a backslash) make a hard break; otherwise a newline is a space.
      if (text.endsWith("  ")) {
        text = text.replace(/ +$/, "");
        flush();
        out.push({ t: "br" });
      } else text += "\n";
      i++;
      continue;
    }
    if (ch === "`") {
      const ticks = /^`+/.exec(rest)![0];
      const end = src.indexOf(ticks, i + ticks.length);
      if (end !== -1) {
        flush();
        out.push({ t: "code", v: src.slice(i + ticks.length, end).replace(/^ (.*) $/, "$1") });
        i = end + ticks.length;
        continue;
      }
      text += ticks;
      i += ticks.length;
      continue;
    }
    if (ch === "<" && /^<sub>/i.test(rest)) {
      const end = rest.toLowerCase().indexOf("</sub>");
      if (end > 5) {
        flush();
        out.push({ t: "sub", c: parseInline(rest.slice(5, end)) });
        i += end + 6;
        continue;
      }
    }
    if (ch === "[") {
      const m = /^\[([^\]\n]*)\]\(([^)\s]+)(?:\s+"[^"]*")?\)/.exec(rest);
      if (m) {
        const href = safeHref(m[2]);
        flush();
        if (href) out.push({ t: "link", href, c: parseInline(m[1]) });
        else out.push(...parseInline(m[1]));
        i += m[0].length;
        continue;
      }
    }
    if (ch === "h" && /^https?:\/\//.test(rest) && (i === 0 || /[\s(]/.test(src[i - 1]))) {
      const m = /^https?:\/\/[^\s<>"'`)]+/.exec(rest)!;
      const url = m[0].replace(/[.,;:!?]+$/, "");
      flush();
      out.push({ t: "link", href: url, c: [{ t: "text", v: url }] });
      i += url.length;
      continue;
    }
    const span = (marker: string, kind: "strong" | "em" | "del") => {
      if (!rest.startsWith(marker)) return false;
      const inner = rest.slice(marker.length);
      if (!inner || /^\s/.test(inner)) return false;
      const end = inner.indexOf(marker);
      if (end <= 0 || /\s$/.test(inner.slice(0, end))) return false;
      flush();
      out.push({ t: kind, c: parseInline(inner.slice(0, end)) });
      i += marker.length * 2 + end;
      return true;
    };
    if (span("**", "strong") || span("__", "strong") || span("~~", "del")) continue;
    if ((ch === "*" || (ch === "_" && (i === 0 || !/\w/.test(src[i - 1])))) && span(ch, "em")) continue;
    text += ch;
    i++;
  }
  flush();
  return out;
}

/** The text of inline tokens, for titles and tests. */
export function inlineText(c: Inline[]): string {
  return c
    .map((n) => (n.t === "text" || n.t === "code" ? n.v : n.t === "br" ? "\n" : inlineText(n.c)))
    .join("");
}
