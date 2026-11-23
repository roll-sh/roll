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
  /** What went wrong on the page (its uncaught errors, requests answered 400 or more), sent with
   * the note only if the person keeps it. */
  errors?: string[];
  requests?: string[];
  /** The browser it was left in (`Chrome 131 on macOS`). */
  browser?: string;
}

/** The page's own words, kept to one inert line each. */
function contextList(raw: string | null): string[] {
  try {
    const v: unknown = JSON.parse(raw ?? "[]");
    return Array.isArray(v) ? v.filter((x): x is string => typeof x === "string").slice(-5).map((x) => x.replace(/[`\r\n]/g, "").slice(0, 200)).filter(Boolean) : [];
  } catch {
    return [];
  }
}

/** A user agent as a person names it: `Chrome 131 on macOS`, or nothing for one it can't tell. */
export function browserOf(ua: string): string | undefined {
  const v = (re: RegExp) => re.exec(ua)?.[1];
  const name = v(/Edg(?:e|A|iOS)?\/(\d+)/)
    ? `Edge ${v(/Edg(?:e|A|iOS)?\/(\d+)/)}`
    : v(/(?:Firefox|FxiOS)\/(\d+)/)
      ? `Firefox ${v(/(?:Firefox|FxiOS)\/(\d+)/)}`
      : v(/(?:Chrome|CriOS)\/(\d+)/)
        ? `Chrome ${v(/(?:Chrome|CriOS)\/(\d+)/)}`
        : v(/Version\/(\d+)[\d.]* (?:Mobile\/\S+ )?Safari\//)
          ? `Safari ${v(/Version\/(\d+)[\d.]* (?:Mobile\/\S+ )?Safari\//)}`
          : undefined;
  if (!name) return undefined;
  const os = /iPhone|iPad/.test(ua) ? "iOS" : /Android/.test(ua) ? "Android" : /Mac OS X/.test(ua) ? "macOS" : /Windows/.test(ua) ? "Windows" : /CrOS/.test(ua) ? "ChromeOS" : /Linux/.test(ua) ? "Linux" : "";
  return os ? `${name} on ${os}` : name;
}

const plural = (n: number, one: string) => `${n} ${one}${n === 1 ? "" : "s"}`;

/** How much went wrong on the page: `1 error, 2 failed requests`. */
export function contextSummary(f: PreviewFeedback): string {
  return [f.errors?.length ? plural(f.errors.length, "error") : "", f.requests?.length ? plural(f.requests.length, "failed request") : ""].filter(Boolean).join(", ");
}

/** The feedback from `?fb_path=…&fb_selector=…&fb_note=…` (the button opens roll with it), or
 * none. Read from the raw query: the router's own parsing would turn a note of `42` into a number. */
export function feedbackFrom(query: string): PreviewFeedback | undefined {
  const params = new URLSearchParams(query);
  const s = (k: string) => params.get(k) ?? "";
  if (!s("fb_note").trim()) return undefined;
  return {
    path: s("fb_path"),
    selector: s("fb_selector"),
    box: s("fb_box") || undefined,
    viewport: s("fb_viewport") || undefined,
    note: s("fb_note"),
    errors: contextList(params.get("fb_errors")),
    requests: contextList(params.get("fb_requests")),
    browser: browserOf(s("fb_ua").slice(0, 300)),
  };
}

/** The note a preview page's Reply or Resolve opened roll for (`?fb_reply=` / `?fb_resolve=<review
 * id>`), or none: the sheet shows its thread with both. */
export function resolveFrom(query: string): string | undefined {
  const params = new URLSearchParams(query);
  const id = (params.get("fb_resolve") ?? params.get("fb_reply") ?? "").trim();
  return /^[A-Za-z0-9_-]{1,64}$/.test(id) ? id : undefined;
}

const REPLY = "**Reply on the preview** · ";

/** A reply's review body: the note it answers (its review id), then the reply
 * (`roll_previews::notes::parse_reply` reads it back). */
export function replyBody(note: string, text: string): string {
  return `${REPLY}\`${note}\`\n\n${text.trim().slice(0, 2000)}`;
}

/** The note a review replies to, and the reply, if it is one. */
export function replyOf(body: string): { note: string; text: string } | undefined {
  const at = body.indexOf("\n\n");
  if (at < 0 || !body.startsWith(REPLY)) return undefined;
  const id = body.slice(REPLY.length, at).match(/^`([A-Za-z0-9_-]{1,64})`$/)?.[1];
  const text = body.slice(at + 2).trim();
  return id && text ? { note: id, text } : undefined;
}

/** The review's body: where on the preview, then the note, then (when kept) what went wrong on the
 * page, after the `<sub>` line so it's never read back as the note (`roll_previews::notes::parse`).
 * The page's own names (its path, the element's selector, its errors) come from the preview's
 * code, so they're kept to one inert line each. */
export function feedbackBody(p: Preview, f: PreviewFeedback, note: string, withContext = true): string {
  const path = "/" + f.path.replace(/^\/+/, "").replace(/[\s()<>[\]`]/g, "").slice(0, 300);
  const selector = f.selector.replace(/[`\r\n]/g, "").slice(0, 200);
  const where = p.url ? `[\`${path}\`](${p.url.replace(/\/+$/, "")}${path})` : `\`${path}\``;
  const meta = [f.viewport && /^\d+x\d+$/.test(f.viewport) ? `${f.viewport} viewport` : "", f.browser ?? "", `roll preview ${p.id}`].filter(Boolean).join(" · ");
  const body = `**Feedback on the preview** · ${where}${selector ? ` · \`${selector}\`` : ""}\n\n${note.trim().slice(0, 4000)}\n\n<sub>${meta}</sub>`;
  const summary = contextSummary(f);
  if (!withContext || !summary) return body;
  const lines = [...(f.errors ?? []).map((e) => `- error: \`${e}\``), ...(f.requests ?? []).map((r) => `- \`${r}\``)];
  return `${body}\n\n<details><summary>On the page: ${summary}</summary>\n\n${lines.join("\n")}\n\n</details>`;
}

/** The note the preview page's button sent, to post on the change as a comment review. */
export function FeedbackForm({ preview: p, feedback: f, onPosted }: { preview: Preview; feedback: PreviewFeedback; onPosted?: () => void }) {
  const [note, setNote] = useState(f.note);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [posted, setPosted] = useState(false);
  const [withContext, setWithContext] = useState(true);
  const summary = contextSummary(f);
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
          await codeApi.submitReview(p.change, { verdict: "comment", body: feedbackBody(p, f, note, withContext) });
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
      {summary && (
        <div className="space-y-1 text-[11.5px]">
          <label className="flex items-center gap-2 text-soft">
            <input type="checkbox" checked={withContext} onChange={(e) => setWithContext(e.target.checked)} className="accent-accent" />
            Include what went wrong on the page ({summary})
          </label>
          <ul className={`ml-5 space-y-0.5 font-mono text-[10.5px] break-words ${withContext ? "text-subtle" : "text-subtle/50 line-through"}`}>
            {[...(f.errors ?? []), ...(f.requests ?? [])].map((x) => (
              <li key={x}>{x}</li>
            ))}
          </ul>
        </div>
      )}
      <div className="flex items-center justify-end gap-2">
        {error && <span className="mr-auto text-[11.5px] text-danger">{error}</span>}
        <button type="submit" disabled={busy || !note.trim()} className="h-7 rounded-md bg-accent px-2.5 text-[12px] font-medium text-white disabled:opacity-40">
          {busy ? "Posting…" : "Post on the change"}
        </button>
      </div>
    </form>
  );
}
