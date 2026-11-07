"use client";

import { useAgents } from "../use-agents";

/** Ids the runner and the daemon write into messages: `thr_…`, `prj_…`, `mch_…`. */
const IDS = /\b((?:thr|prj|mch)_[A-Za-z0-9_]+)\b/;

/**
 * A run's message (a log line, a step's error, why a run failed, a notice) with the ids in it as
 * names: a thread's title (a button opening it, given `onThread`), a project's or a machine's name.
 * An id nothing here knows stays as it is.
 */
export function Named({ text, onThread }: { text: string; onThread?: (thread: string) => void }) {
  const { client } = useAgents();
  const parts = text.split(IDS);
  if (parts.length === 1) return <>{text}</>;
  const d = client.data;
  return (
    <>
      {parts.map((p, i) => {
        if (i % 2 === 0) return p;
        if (p.startsWith("thr_")) {
          const title = d.threads.get(p)?.value.title;
          if (!title || !onThread) return title ? `“${title}”` : p;
          return (
            <button key={i} type="button" onClick={() => onThread(p)} title="Open the conversation" className="text-soft underline decoration-line-3 underline-offset-2 hover:text-fg">
              {title}
            </button>
          );
        }
        return (p.startsWith("prj_") ? d.projects.get(p)?.value.name : d.machines.get(p)?.value.name) ?? p;
      })}
    </>
  );
}
