import { Fragment, type ReactNode } from "react";
import { parseMarkdown, type Block, type Inline } from "@/lib/markdown";

/**
 * A review comment's markdown, as React nodes (raw HTML stays text). Uses the README styles at
 * comment size. Works in server and client components.
 */
export function Markdown({ source, className = "" }: { source: string; className?: string }) {
  if (!source.trim()) return null;
  return <div className={`markdown markdown-sm ${className}`}>{parseMarkdown(source).map(block)}</div>;
}

function block(b: Block, i: number): ReactNode {
  switch (b.t) {
    case "p":
      return <p key={i}>{inline(b.c)}</p>;
    case "h": {
      const H = (["h3", "h3", "h4", "h5"] as const)[b.level - 1];
      return <H key={i}>{inline(b.c)}</H>;
    }
    case "code":
      // Diagrams stay source: no mermaid renderer ships with the dashboard.
      return b.lang === "mermaid" ? (
        <figure key={i}>
          <figcaption className="mb-1 text-[10px] tracking-wide text-subtle uppercase">diagram</figcaption>
          <pre>
            <code>{b.v}</code>
          </pre>
        </figure>
      ) : (
        <pre key={i}>
          <code>{b.v}</code>
        </pre>
      );
    case "table":
      return (
        <div key={i} className="overflow-x-auto">
          <table>
            <thead>
              <tr>
                {b.head.map((c, j) => (
                  <th key={j} style={b.align[j] ? { textAlign: b.align[j]! } : undefined}>
                    {inline(c)}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {b.rows.map((row, r) => (
                <tr key={r}>
                  {row.map((c, j) => (
                    <td key={j} style={b.align[j] ? { textAlign: b.align[j]! } : undefined}>
                      {inline(c)}
                    </td>
                  ))}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      );
    case "details":
      return (
        <details key={i}>
          <summary>{inline(b.summary)}</summary>
          {b.c.map(block)}
        </details>
      );
    case "quote":
      return <blockquote key={i}>{b.c.map(block)}</blockquote>;
    case "list": {
      const L = b.ordered ? "ol" : "ul";
      return (
        <L key={i}>
          {b.items.map((it, j) => (
            <li key={j}>{inline(it)}</li>
          ))}
        </L>
      );
    }
    case "hr":
      return <hr key={i} />;
  }
}

function inline(nodes: Inline[]): ReactNode {
  return nodes.map((n, i) => {
    switch (n.t) {
      case "text":
        return <Fragment key={i}>{n.v}</Fragment>;
      case "code":
        return <code key={i}>{n.v}</code>;
      case "strong":
        return <strong key={i}>{inline(n.c)}</strong>;
      case "em":
        return <em key={i}>{inline(n.c)}</em>;
      case "del":
        return <del key={i}>{inline(n.c)}</del>;
      case "link":
        return (
          <a key={i} href={n.href} target={n.href.startsWith("http") ? "_blank" : undefined} rel="noreferrer nofollow">
            {inline(n.c)}
          </a>
        );
      case "sub":
        return (
          <sub key={i} className="text-[11px] text-subtle">
            {inline(n.c)}
          </sub>
        );
      case "br":
        return <br key={i} />;
    }
  });
}
