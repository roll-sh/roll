import { Code2, SquareTerminal } from "lucide-react";
import { type EsRequest, requestText } from "@/lib/search-discover";
import { btn } from "../ui";

/** The button that shows or hides [`RequestList`]. */
export function RequestsButton({ count, shown, onToggle }: { count: number; shown: boolean; onToggle: () => void }) {
  return (
    <button type="button" onClick={onToggle} disabled={count === 0} className={`${btn} py-0.5`} aria-pressed={shown} title="Show the requests this sends">
      <Code2 className="size-3.5" strokeWidth={1.5} /> {count === 1 ? "Request" : `Requests (${count})`}
    </button>
  );
}

/** The requests a part of Discover sends, each with a button to open it in the console (which
 * takes one request at a time). */
export function RequestList({ requests, onOpenConsole }: { requests: EsRequest[]; onOpenConsole: (text: string) => void }) {
  return (
    <div className="rise space-y-2">
      {requests.map((r) => {
        const text = requestText(r);
        return (
          <div key={text} className="relative">
            <pre className="max-h-48 overflow-auto rounded-lg border border-line bg-bg px-3 py-2 font-mono text-[11px] leading-relaxed text-muted">{text}</pre>
            <button type="button" onClick={() => onOpenConsole(text)} className={`${btn} absolute top-1.5 right-1.5 py-0.5`} title="Open this request in the console">
              <SquareTerminal className="size-3.5" strokeWidth={1.5} /> Console
            </button>
          </div>
        );
      })}
    </div>
  );
}
