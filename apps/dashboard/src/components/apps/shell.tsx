import { SquareTerminal, X } from "lucide-react";
import { useState } from "react";
import { appsApi, followShell, type Instance } from "@/lib/apps-api";
import { TerminalScreen } from "../terminal-screen";
import { btn, EmptyPanel, input, Panel, primary } from "../ui";
import { message } from "./shared";

/**
 * A shell in a running instance (as `roll apps ssh`): xterm.js on an HTTP session, the screen over
 * SSE, keystrokes batched into POSTs, the size sent when the panel changes. A web service asleep is
 * woken and kept up while the shell is open.
 */
const CONSOLE = "+console";

export function ShellTab({ service, env, instances = [] }: { service: string; env: string; instances?: Instance[] }) {
  const [session, setSession] = useState<string | null>(null);
  // An instance of its own (empty: one that's running, or a woken one; CONSOLE: a fresh one).
  const [instance, setInstance] = useState("");
  const [command, setCommand] = useState("");
  const console_ = instance === CONSOLE;
  const running = instances.filter((i) => i.state === "running");
  const [state, setState] = useState<"idle" | "opening" | "open" | "ended">("idle");
  const [error, setError] = useState<string | null>(null);

  const open = async () => {
    setError(null);
    setState("opening");
    try {
      const where = console_ ? { console: true, ...(command.trim() ? { command: command.trim() } : {}) } : instance && running.some((i) => i.id === instance) ? { instance } : {};
      const { id } = await appsApi.openShell(service, env, { cols: 100, rows: 30, ...where });
      setSession(id);
      setState("open");
    } catch (e) {
      setError(message(e));
      setState("idle");
    }
  };

  const close = () => {
    if (session) appsApi.closeShell(service, env, session).catch(() => {});
    setSession(null);
    setState("idle");
  };

  return (
    <div className="space-y-4">
      <Panel
        title="Shell"
        aside={
          session ? (
            <button type="button" className={btn} onClick={close}>
              <X className="size-3.5" /> Close
            </button>
          ) : (
            <span className="flex items-center gap-2">
              {console_ && (
                <input value={command} onChange={(e) => setCommand(e.target.value)} placeholder="rails console" aria-label="Command" className={`${input} !w-48 font-mono text-[12px]`} />
              )}
              <select value={instance} onChange={(e) => setInstance(e.target.value)} aria-label="Where" className={`${input} !w-auto font-mono text-[12px]`}>
                <option value="">{running.length > 1 ? "Any instance" : "A running instance"}</option>
                {running.length > 1 &&
                  running.map((i) => (
                    <option key={i.id} value={i.id}>
                      {i.id}
                    </option>
                  ))}
                <option value={CONSOLE}>A new console</option>
              </select>
            <button type="button" className={primary} disabled={state === "opening"} onClick={() => void open()}>
              <SquareTerminal className="size-3.5" /> {state === "opening" ? "Opening…" : state === "ended" ? "Open another" : "Open a shell"}
            </button>
            </span>
          )
        }
      >
        {session ? (
          <TerminalScreen
            key={session}
            session={session}
            link={{
              follow: (h) => followShell(service, env, session, h),
              keys: (data) => appsApi.shellKeys(service, env, session, data),
              resize: (size) => appsApi.resizeShell(service, env, session, size),
              close: () => appsApi.closeShell(service, env, session),
            }}
            onEnded={() => {
              setSession(null);
              setState("ended");
            }}
          />
        ) : (
          <div className="px-5 py-4 text-[13px] text-muted">
            {state === "ended"
              ? "The shell ended."
              : console_
                ? "A fresh container of the live deployment, with the environment's variables and no traffic: what you type runs there (a login shell when no command is given), and the container goes when you close it."
                : "bash (or sh) in a running instance, with the environment's variables. A service that's asleep is woken while it's open."}{" "}
            From a terminal: <code className="font-mono text-fg">{console_ ? `roll apps run -i -s ${service} -e ${env}${command.trim() ? ` -- ${command.trim()}` : ""}` : `roll apps ssh -s ${service} -e ${env}`}</code>.
          </div>
        )}
      </Panel>
      {error && <EmptyPanel>{error}</EmptyPanel>}
    </div>
  );
}
