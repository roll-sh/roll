"use client";

import { ChevronDown, GitBranch } from "lucide-react";
import { useRouter } from "next/navigation";
import { useEffect, useRef, useState } from "react";
import { codeApi, shortId, type Ref } from "@/lib/code-api";

/** Switch the page to another ref, keeping the section and path. `to` builds the URL for a rev. */
export function RefPicker({ repo, rev, kind, path = "" }: { repo: string; rev: string; kind: "tree" | "blob" | "commits"; path?: string }) {
  const router = useRouter();
  const [open, setOpen] = useState(false);
  const [refs, setRefs] = useState<Ref[] | null>(null);
  const box = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open || refs) return;
    codeApi.refs(repo).then(setRefs, () => setRefs([]));
  }, [open, refs, repo]);

  useEffect(() => {
    if (!open) return;
    const close = (e: MouseEvent) => !box.current?.contains(e.target as Node) && setOpen(false);
    const esc = (e: KeyboardEvent) => e.key === "Escape" && setOpen(false);
    document.addEventListener("mousedown", close);
    document.addEventListener("keydown", esc);
    return () => {
      document.removeEventListener("mousedown", close);
      document.removeEventListener("keydown", esc);
    };
  }, [open]);

  const to = (r: string) => {
    const p = path.split("/").filter(Boolean).map(encodeURIComponent).join("/");
    const base = `/${repo}/${kind}/${encodeURIComponent(r)}`;
    return kind === "commits" ? `${base}${path ? `?path=${encodeURIComponent(path)}` : ""}` : `${base}${p ? `/${p}` : ""}`;
  };
  const label = rev.startsWith("cs_") || /^[0-9a-f]{40,64}$/.test(rev) ? shortId(rev) : rev;

  return (
    <div ref={box} className="relative">
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        className="flex max-w-56 items-center gap-1.5 rounded-lg border border-line bg-panel px-2.5 py-1.5 font-mono text-[12px] transition hover:border-line-2"
        aria-haspopup="listbox"
        aria-expanded={open}
      >
        <GitBranch className="size-3.5 shrink-0 text-subtle" />
        <span className="truncate">{label}</span>
        <ChevronDown className="size-3 shrink-0 text-subtle" />
      </button>
      {open && (
        <div className="rise absolute top-full left-0 z-20 mt-1 w-64 overflow-hidden rounded-lg border border-line bg-panel">
          <p className="border-b border-line px-3 py-2 text-[11px] text-subtle">Switch ref</p>
          {!refs ? (
            <p className="px-3 py-2 text-[12px] text-subtle">
              <span className="shimmer">Loading…</span>
            </p>
          ) : (
            <ul className="max-h-72 overflow-y-auto py-1" role="listbox">
              {refs.map((r) => (
                <li key={r.name}>
                  <button
                    type="button"
                    role="option"
                    aria-selected={r.name === rev}
                    onClick={() => {
                      setOpen(false);
                      router.push(to(r.name));
                    }}
                    className={`flex w-full items-center justify-between gap-2 px-3 py-1.5 text-left font-mono text-[12px] hover:bg-panel-2 ${
                      r.name === rev ? "text-fg" : "text-muted"
                    }`}
                  >
                    <span className="truncate">{r.name}</span>
                    <span className="text-[10px] text-subtle">{shortId(r.changeset)}</span>
                  </button>
                </li>
              ))}
            </ul>
          )}
        </div>
      )}
    </div>
  );
}
