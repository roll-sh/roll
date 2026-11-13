import { MessageSquare } from "lucide-react";
import { useState } from "react";
import { codeApi } from "@/lib/code-api";
import type { Preview } from "@/lib/previews-api";
import { errorText } from "./previews-common";

/** A note from the preview page's feedback button (docs/previews.md): what it pins and where. */
export interface PreviewFeedback {
  path: string;
  selector: string;
  box?: string;
  viewport?: string;
  note: string;
}

/** The feedback from `?fb_path=…&fb_selector=…&fb_note=…` (the button opens roll with it), or
 * none. Read from the raw query: the router's own parsing would turn a note of `42` into a number. */
export function feedbackFrom(query: string): PreviewFeedback | undefined {
  const params = new URLSearchParams(query);
  const s = (k: string) => params.get(k) ?? "";
  if (!s("fb_note").trim()) return undefined;
  return { path: s("fb_path"), selector: s("fb_selector"), box: s("fb_box") || undefined, viewport: s("fb_viewport") || undefined, note: s("fb_note") };
}

/** The review's body: where on the preview, then the note. The page's own names (its path, the
 * element's selector) come from the preview's code, so they're kept to one inert line each. */
export function feedbackBody(p: Preview, f: PreviewFeedback, note: string): string {
  const path = "/" + f.path.replace(/^\/+/, "").replace(/[\s()<>[\]`]/g, "").slice(0, 300);
  const selector = f.selector.replace(/[`\r\n]/g, "").slice(0, 200);
  const where = p.url ? `[\`${path}\`](${p.url.replace(/\/+$/, "")}${path})` : `\`${path}\``;
  const meta = [f.viewport && /^\d+x\d+$/.test(f.viewport) ? `${f.viewport} viewport` : "", `roll preview ${p.id}`].filter(Boolean).join(" · ");
  return `**Feedback on the preview** · ${where}${selector ? ` · \`${selector}\`` : ""}\n\n${note.trim().slice(0, 4000)}\n\n<sub>${meta}</sub>`;
}

/** The note the preview page's button sent, to post on the change as a comment review. */
export function FeedbackForm({ preview: p, feedback: f, onPosted }: { preview: Preview; feedback: PreviewFeedback; onPosted?: () => void }) {
  const [note, setNote] = useState(f.note);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [posted, setPosted] = useState(false);
  const change = p.number && p.repo ? `/dashboard/code/${p.repo}/changes/${p.number}` : null;
  if (!p.change) return <p className="text-[12px] text-muted">Feedback goes on a change; this preview is a branch's.</p>;
  if (posted)
    return (
      <p className="rounded-lg border border-line bg-panel px-3 py-2 text-[12px] text-soft">
        Posted on the change{change && (
          <>
            {" · "}
            <a href={change} className="text-accent hover:underline">
              open it
            </a>
          </>
        )}
        .
      </p>
    );
  return (
    <form
      className="space-y-2 rounded-lg border border-line bg-panel p-3"
      onSubmit={async (e) => {
        e.preventDefault();
        if (!note.trim() || busy || !p.change) return;
        setBusy(true);
        try {
          await codeApi.submitReview(p.change, { verdict: "comment", body: feedbackBody(p, f, note) });
          setPosted(true);
          onPosted?.();
        } catch (err) {
          setError(errorText(err));
        } finally {
          setBusy(false);
        }
      }}
    >
      <div className="flex items-center gap-2 text-[12px] text-soft">
        <MessageSquare className="size-3.5 text-muted" strokeWidth={1.5} />
        Feedback from the preview
      </div>
      <p className="truncate font-mono text-[11px] text-subtle" title={`${f.path} · ${f.selector}`}>
        {f.path} · {f.selector}
      </p>
      <textarea
        value={note}
        onChange={(e) => setNote(e.target.value)}
        aria-label="Feedback"
        maxLength={4000}
        className="min-h-20 w-full rounded-md border border-line-2 bg-canvas px-2 py-1.5 text-[12.5px] outline-none transition-colors focus:border-accent/60"
      />
      <div className="flex items-center justify-end gap-2">
        {error && <span className="mr-auto text-[11.5px] text-danger">{error}</span>}
        <button type="submit" disabled={busy || !note.trim()} className="h-7 rounded-md bg-accent px-2.5 text-[12px] font-medium text-white disabled:opacity-40">
          {busy ? "Posting…" : "Post on the change"}
        </button>
      </div>
    </form>
  );
}
