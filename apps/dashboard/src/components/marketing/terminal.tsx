import { useReducedMotion, type MarkState } from "@roll/brand";
import { useEffect, useMemo, useRef, useState, type KeyboardEvent } from "react";
import type { Line, Scene } from "@/lib/marketing";
import { useMarkState } from "../mark-state";
import { useInView } from "./in-view";

const TYPE_MS = 26;
const LINE_MS = 120;
const HOLD_MS = 3800;
/** Lines a typed session keeps on screen. */
const KEEP = 11;

/** What a scene's last line does to the shared mark: a deploy rises, then succeeds. */
const MARK: Record<NonNullable<Scene["mark"]>, [MarkState, number][]> = {
  deploying: [["deploying", 1600], ["success", 1400]],
  success: [["success", 1400]],
};

interface Reply {
  out: Line[];
  mark?: Scene["mark"];
}

/** Every command in the scenes, with the output that follows it, so a visitor can type them. */
function repliesOf(scenes: Scene[]) {
  const replies = new Map<string, Reply>();
  for (const s of scenes) {
    s.lines.forEach((l, i) => {
      if (l.cmd === undefined) return;
      const next = s.lines.findIndex((m, k) => k > i && m.cmd !== undefined);
      const out = s.lines.slice(i + 1, next < 0 ? undefined : next);
      replies.set(l.cmd, { out, mark: next < 0 ? s.mark : undefined });
    });
  }
  return replies;
}

/**
 * A terminal that types roll commands and their output, a tab per scene, on its own while on
 * screen; a scene that deploys plays the mark everywhere on the page (header logo, globe). Click
 * it and it's yours: type any command the scenes show (Tab completes, ↑ recalls), or `help`.
 * Reduced motion shows each scene whole, with no typing and no auto-advance.
 */
export function Terminal({ scenes, className }: { scenes: Scene[]; className?: string }) {
  const ref = useRef<HTMLDivElement>(null);
  const input = useRef<HTMLInputElement>(null);
  const inView = useInView(ref);
  const reduced = useReducedMotion();
  const { play } = useMarkState();
  const [scene, setScene] = useState(0);
  // How far the scene has played: lines fully shown, and characters of the line being typed.
  const [shown, setShown] = useState({ lines: 0, chars: 0 });
  // Once the visitor types, the terminal stops playing and shows their session instead.
  const [session, setSession] = useState<Line[] | null>(null);
  const [draft, setDraft] = useState("");
  const [history, setHistory] = useState<string[]>([]);
  const [recall, setRecall] = useState(-1);
  const replies = useMemo(() => repliesOf(scenes), [scenes]);
  const commands = useMemo(() => [...replies.keys()], [replies]);
  const lines = scenes[scene].lines;

  useEffect(() => {
    if (session) return;
    if (reduced) return setShown({ lines: lines.length, chars: 0 });
    if (!inView) return;
    setShown({ lines: 0, chars: 0 });
    let cancelled = false;
    const timers: ReturnType<typeof setTimeout>[] = [];
    const wait = (ms: number) => new Promise<void>((r) => timers.push(setTimeout(r, ms)));
    (async () => {
      await wait(400);
      for (let i = 0; i < lines.length && !cancelled; i++) {
        const line = lines[i];
        if (line.cmd !== undefined) {
          for (let c = 1; c <= line.cmd.length && !cancelled; c++) {
            setShown({ lines: i, chars: c });
            await wait(TYPE_MS + (line.cmd[c - 1] === " " ? 30 : 0));
          }
          await wait(380);
        } else {
          await wait(LINE_MS);
        }
        if (cancelled) return;
        setShown({ lines: i + 1, chars: 0 });
      }
      const mark = scenes[scene].mark;
      if (mark) play(MARK[mark]);
      await wait(HOLD_MS);
      if (!cancelled) setScene((s) => (s + 1) % scenes.length);
    })();
    return () => {
      cancelled = true;
      timers.forEach(clearTimeout);
    };
  }, [scene, inView, reduced, lines, scenes, play, session]);

  const takeOver = () => {
    if (!session) setSession(lines.slice(0, Math.min(shown.lines, lines.length)));
    input.current?.focus({ preventScroll: true });
  };

  const run = (raw: string) => {
    const c = raw.trim();
    setDraft("");
    setRecall(-1);
    if (!c) return setSession((s) => [...(s ?? []), { cmd: "" }].slice(-KEEP));
    setHistory((h) => [c, ...h.filter((x) => x !== c)].slice(0, 20));
    if (c === "clear") return setSession([]);
    let out: Line[];
    const reply = replies.get(c);
    if (reply) {
      out = reply.out;
      if (reply.mark) play(MARK[reply.mark]);
    } else if (c === "help" || c === "roll" || c === "roll --help") {
      out = [{ out: "Try one of these (Tab completes):" }, ...commands.slice(0, 6).map((x) => ({ out: `  ${x}`, tone: "accent" as const }))];
    } else {
      const near = commands.find((x) => x.startsWith(c.split(" ").slice(0, 2).join(" "))) ?? commands[0];
      out = [{ out: `${c.split(" ")[0]}: not in this demo. Try: ${near}` }];
    }
    setSession((s) => [...(s ?? []), { cmd: c }, ...out].slice(-KEEP));
  };

  const onKey = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "Enter") run(draft);
    else if (e.key === "Tab") {
      const hit = commands.find((x) => x.startsWith(draft) && x !== draft);
      if (hit && draft) {
        e.preventDefault();
        setDraft(hit);
      }
    } else if (e.key === "ArrowUp" || e.key === "ArrowDown") {
      e.preventDefault();
      const next = Math.max(-1, Math.min(history.length - 1, recall + (e.key === "ArrowUp" ? 1 : -1)));
      setRecall(next);
      setDraft(next < 0 ? "" : history[next]);
    }
  };

  const ghost = draft ? commands.find((x) => x.startsWith(draft) && x !== draft) : undefined;

  return (
    <div ref={ref} onClick={takeOver} className={`group/term cursor-text overflow-hidden rounded-xl border border-line bg-panel ${className ?? ""}`}>
      <div className="flex items-center gap-1 overflow-x-auto border-b border-line px-2 py-1.5 [scrollbar-width:none]">
        {scenes.map((s, i) => (
          <button
            key={s.label}
            onClick={(e) => {
              e.stopPropagation();
              setSession(null);
              setScene(i);
            }}
            className={`shrink-0 rounded-md px-2.5 py-1 font-mono text-[11px] transition ${
              !session && i === scene ? "bg-panel-2 text-fg" : "text-subtle hover:text-muted"
            }`}
          >
            {s.label}
          </button>
        ))}
        <span className="ml-auto hidden shrink-0 pr-1.5 font-mono text-[10.5px] text-subtle transition group-hover/term:text-muted sm:block">
          {session ? "your session" : "click to type"}
        </span>
      </div>
      <pre className={`flex h-[17rem] flex-col overflow-hidden ${session ? "justify-end" : ""} px-4 py-3.5 font-mono text-[12.5px] leading-[1.7] whitespace-pre-wrap`}>
        <div className="min-h-0">
          {session
            ? session.map((line, i) => <Row key={i} line={line} />)
            : lines.map((line, i) => {
                if (i > shown.lines) return null;
                if (line.cmd !== undefined) {
                  const typing = i === shown.lines;
                  return (
                    <div key={i}>
                      <span className="text-subtle select-none">$ </span>
                      <span className={typing ? "caret" : ""}>{typing ? line.cmd.slice(0, shown.chars) : line.cmd}</span>
                    </div>
                  );
                }
                return i === shown.lines ? null : <Row key={i} line={line} />;
              })}
          {(session || (!reduced && shown.lines >= lines.length)) && (
            <div className="relative flex">
              <span className="text-subtle select-none">$ </span>
              <span className="relative min-w-0 flex-1">
                <span aria-hidden className={`pointer-events-none whitespace-pre ${session ? "" : "caret"}`}>
                  <span className="invisible">{draft}</span>
                  {ghost && <span className="text-subtle">{ghost.slice(draft.length)}</span>}
                </span>
                <input
                  ref={input}
                  value={draft}
                  onChange={(e) => setDraft(e.target.value)}
                  onKeyDown={onKey}
                  onFocus={takeOver}
                  spellCheck={false}
                  autoCapitalize="off"
                  autoComplete="off"
                  aria-label="Type a roll command"
                  className="absolute inset-0 w-full bg-transparent text-fg caret-accent outline-none"
                />
              </span>
            </div>
          )}
        </div>
      </pre>
    </div>
  );
}

function Row({ line }: { line: Line }) {
  if (line.cmd !== undefined)
    return (
      <div>
        <span className="text-subtle select-none">$ </span>
        {line.cmd}
      </div>
    );
  return <div className={`rise ${line.tone === "ok" ? "text-ok" : line.tone === "accent" ? "text-accent" : "text-muted"}`}>{line.out || " "}</div>;
}
