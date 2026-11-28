import { Check, Copy } from "lucide-react";
import { Fragment, useState, type ReactNode } from "react";
import type { Lang, Snippet } from "@/lib/marketing-connect";

const KEYWORDS = /\b(import|from|export|const|await|async|with|as|new|return|print)\b/;
/** A comment to the end of the line, a quoted string, or a keyword. Everything else is plain. */
const TOKEN: Record<Lang, RegExp> = {
  sh: /(#.*$)|("(?:[^"\\]|\\.)*"|'[^']*')/g,
  py: new RegExp(`(#.*$)|("(?:[^"\\\\]|\\\\.)*"|'[^']*')|${KEYWORDS.source}`, "g"),
  ts: new RegExp(`(//.*$)|("(?:[^"\\\\]|\\\\.)*"|'[^']*'|\`[^\`]*\`)|${KEYWORDS.source}`, "g"),
  toml: /(#.*$)|("(?:[^"\\]|\\.)*")|^(\[+[^\]]+\]+)/g,
  yaml: /(#.*$)|("(?:[^"\\]|\\.)*")|^(\s*-?\s*[\w-]+:)/g,
  json: /(\/\/.*$)|("(?:[^"\\]|\\.)*"(?=\s*:))/g,
};

/**
 * One line in the page's inks: comments faint, strings and keywords a step down from the code
 * itself. No colours: the accent stays for what's live.
 */
function tint(line: string, lang: Lang): ReactNode[] {
  const out: ReactNode[] = [];
  let at = 0;
  for (const m of line.matchAll(TOKEN[lang])) {
    if (m.index > at) out.push(line.slice(at, m.index));
    const cls = m[1] ? "text-subtle" : lang === "json" || m[3] || !m[2] ? "text-muted" : "text-fg/70";
    out.push(
      <span key={m.index} className={cls}>
        {m[0]}
      </span>,
    );
    at = m.index + m[0].length;
  }
  if (at < line.length) out.push(line.slice(at));
  return out;
}

/**
 * Code in tabs, one per client or tool, with a copy button. A shell snippet gets a prompt on each
 * command (not on comments, output or a line a backslash continued), which is never selected or
 * copied.
 */
export function CodePanel({ snippets, className }: { snippets: Snippet[]; className?: string }) {
  const [at, setAt] = useState(0);
  const [copied, setCopied] = useState(false);
  const s = snippets[Math.min(at, snippets.length - 1)];
  if (!s) return null;
  const lines = s.code.split("\n");
  const copy = () => {
    void navigator.clipboard?.writeText(s.code).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 1600);
    });
  };
  return (
    <div className={`overflow-hidden rounded-2xl border border-line bg-panel ${className ?? ""}`}>
      <div className="flex items-center gap-1 border-b border-line px-2.5 py-2">
        <div role="tablist" aria-label="Clients" className="flex min-w-0 flex-1 items-center gap-1 overflow-x-auto [scrollbar-width:none]">
          {snippets.map((x, i) => (
            <button
              key={x.label}
              role="tab"
              aria-selected={i === at}
              onClick={() => setAt(i)}
              className={`shrink-0 rounded-md px-3 py-1.5 font-mono text-[11.5px] transition-colors duration-150 ${i === at ? "bg-panel-2 text-fg" : "text-subtle hover:text-muted"}`}
            >
              {x.label}
            </button>
          ))}
        </div>
        <button
          onClick={copy}
          className="grid size-8 shrink-0 place-items-center rounded-md text-subtle transition-colors hover:bg-panel-2 hover:text-fg"
          aria-label={copied ? "Copied" : `Copy the ${s.label} snippet`}
        >
          {copied ? <Check className="size-3.5 text-ok" strokeWidth={1.5} /> : <Copy className="size-3.5" strokeWidth={1.5} />}
        </button>
      </div>
      <pre key={s.label} role="tabpanel" className="rise min-h-[15.5rem] overflow-x-auto px-5 py-5 font-mono text-[12.5px] leading-[1.75] text-fg sm:px-6 sm:text-[13px]">
        {lines.map((line, i) => {
          const command = s.lang === "sh" && line.trim() !== "" && !line.startsWith("#") && !line.startsWith(" ") && !lines[i - 1]?.endsWith("\\");
          return (
            <Fragment key={i}>
              {s.lang === "sh" && <span className="text-subtle select-none">{command ? "$ " : "  "}</span>}
              {tint(line, s.lang)}
              {"\n"}
            </Fragment>
          );
        })}
      </pre>
    </div>
  );
}
