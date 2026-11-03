"use client";

import { Check, Copy } from "lucide-react";
import { useState } from "react";

/** A command in a hairline box with a copy button. */
export function CopyLine({ text, label, className = "" }: { text: string; label?: string; className?: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <div className={`flex min-w-0 items-center gap-2 rounded-lg border border-line bg-panel px-2.5 py-1.5 ${className}`}>
      {label && <span className="shrink-0 text-[10px] font-medium tracking-wide text-subtle uppercase">{label}</span>}
      <code className="min-w-0 flex-1 truncate font-mono text-[12px]">{text}</code>
      <button
        type="button"
        onClick={() => {
          navigator.clipboard?.writeText(text);
          setCopied(true);
          setTimeout(() => setCopied(false), 1200);
        }}
        className="shrink-0 p-0.5 text-subtle transition hover:text-fg"
        title="Copy"
        aria-label="Copy"
      >
        {copied ? <Check className="size-3.5 text-ok" /> : <Copy className="size-3.5" />}
      </button>
    </div>
  );
}

/** A multi-line snippet with a copy button in the corner. */
export function CopyBlock({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <div className="relative rounded-lg border border-line bg-bg">
      <pre className="overflow-x-auto px-4 py-3 pr-10 font-mono text-[12px] leading-6">{text}</pre>
      <button
        type="button"
        onClick={() => {
          navigator.clipboard?.writeText(text);
          setCopied(true);
          setTimeout(() => setCopied(false), 1200);
        }}
        className="absolute top-2.5 right-2.5 p-0.5 text-subtle transition hover:text-fg"
        title="Copy"
        aria-label="Copy"
      >
        {copied ? <Check className="size-3.5 text-ok" /> : <Copy className="size-3.5" />}
      </button>
    </div>
  );
}
