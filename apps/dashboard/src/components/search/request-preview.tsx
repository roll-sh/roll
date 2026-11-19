import { SquareTerminal } from "lucide-react";
import { JsonText } from "./json-view";

/** The request a form will send, as the console writes it (`METHOD /path` then the body). */
export function consoleText(method: string, path: string, body?: unknown): string {
  return body === undefined ? `${method} ${path}` : `${method} ${path}\n${JSON.stringify(body, null, 2)}`;
}

/**
 * A form's request, folded away under "Request" and a click from the console (to edit it there or
 * keep it), as the new-index form shows its own.
 */
export function RequestPreview({ method, path, body, onOpenConsole, label = "Request" }: { method: string; path: string; body?: unknown; onOpenConsole: (text: string) => void; label?: string }) {
  return (
    <details className="rounded-lg border border-line">
      <summary className="cursor-pointer px-3 py-2 text-[12px] text-muted">{label}</summary>
      <div className="relative border-t border-line">
        <button type="button" onClick={() => onOpenConsole(consoleText(method, path, body))} className="absolute top-2 right-2 rounded-md p-1 text-subtle transition hover:text-fg" title="Open in console">
          <SquareTerminal className="size-3.5" />
        </button>
        <pre className="max-h-72 overflow-auto px-3 py-2 pr-9 font-mono text-[12px] leading-relaxed">
          <span className="text-muted">
            {method} {path}
          </span>
          {body !== undefined && (
            <>
              {"\n"}
              <JsonText text={JSON.stringify(body, null, 2)} />
            </>
          )}
        </pre>
      </div>
    </details>
  );
}
