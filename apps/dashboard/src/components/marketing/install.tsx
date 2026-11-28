import { Check, Copy } from "lucide-react";
import { useState } from "react";
import { INSTALL } from "@/lib/marketing";

/** The one-line install, with a copy button. */
export function InstallCommand({ className }: { className?: string }) {
  const [copied, setCopied] = useState(false);
  const copy = () => {
    void navigator.clipboard?.writeText(INSTALL).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 1600);
    });
  };
  return (
    <button
      type="button"
      onClick={copy}
      className={`group flex w-full items-center gap-3 rounded-lg border border-line bg-panel px-3.5 py-2.5 text-left font-mono text-[12.5px] transition-colors duration-200 outline-none hover:border-line-2 focus-visible:border-accent ${className ?? ""}`}
    >
      <span className="text-subtle select-none">$</span>
      <span className="min-w-0 flex-1 truncate text-fg">{INSTALL}</span>
      {copied ? <Check className="size-3.5 shrink-0 text-ok" strokeWidth={1.5} /> : <Copy className="size-3.5 shrink-0 text-subtle transition-colors group-hover:text-fg" strokeWidth={1.5} />}
      <span className="sr-only" aria-live="polite">
        {copied ? "Copied" : "Copy the install command"}
      </span>
    </button>
  );
}
