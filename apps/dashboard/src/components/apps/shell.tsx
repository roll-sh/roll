import { Download, SquareTerminal, Upload, X } from "lucide-react";
import { useState } from "react";
import { appsApi, followShell, type Instance } from "@/lib/apps-api";
import { tarOf } from "@/lib/bundle";
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
      <FilesPanel service={service} env={env} instance={running.some((i) => i.id === instance) ? instance : undefined} />
    </div>
  );
}

/**
 * Files out of a running instance (a dump, a report) as a tar, and files into a directory of it
 * (`…/files`, as `roll apps cp`): the instance chosen above, or one that's running.
 */
function FilesPanel({ service, env, instance }: { service: string; env: string; instance?: string }) {
  const [path, setPath] = useState("/app/");
  const [dir, setDir] = useState("/tmp/");
  const [busy, setBusy] = useState<"down" | "up" | null>(null);
  const [said, setSaid] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const download = async () => {
    setError(null);
    setSaid(null);
    setBusy("down");
    try {
      const { tar, instance: from } = await appsApi.downloadFiles(service, env, path.trim(), instance);
      const url = URL.createObjectURL(tar);
      const a = document.createElement("a");
      a.href = url;
      a.download = `${path.trim().replace(/\/+$/, "").split("/").pop() || "root"}.tar`;
      a.click();
      setTimeout(() => URL.revokeObjectURL(url), 1000);
      if (from) setSaid(`From ${from}.`);
    } catch (e) {
      setError(message(e));
    } finally {
      setBusy(null);
    }
  };
  const upload = async (files: FileList | null) => {
    if (!files?.length) return;
    setError(null);
    setSaid(null);
    setBusy("up");
    try {
      const tar = tarOf(await Promise.all([...files].map(async (f) => ({ path: f.name, data: new Uint8Array(await f.arrayBuffer()) }))));
      const r = await appsApi.uploadFiles(service, env, dir.trim(), tar, instance);
      setSaid(`${files.length === 1 ? files[0].name : `${files.length} files`} written under ${r.path} in ${r.instance}.`);
    } catch (e) {
      setError(message(e));
    } finally {
      setBusy(null);
    }
  };
  return (
    <Panel title="Files">
      <div className="space-y-3 px-5 py-4 text-[13px]">
        <div className="flex flex-wrap items-center gap-2">
          <input value={path} onChange={(e) => setPath(e.target.value)} aria-label="A path to download" placeholder="/app/report.csv" className={`${input} !w-72 font-mono text-[12px]`} />
          <button type="button" className={btn} disabled={busy !== null || !path.trim().startsWith("/")} onClick={() => void download()}>
            <Download className="size-3.5" /> {busy === "down" ? "Reading…" : "Download as a tar"}
          </button>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <input value={dir} onChange={(e) => setDir(e.target.value)} aria-label="A directory to upload into" placeholder="/tmp/" className={`${input} !w-72 font-mono text-[12px]`} />
          <label className={`${btn} cursor-pointer ${busy !== null || !dir.trim().startsWith("/") ? "pointer-events-none opacity-50" : ""}`}>
            <Upload className="size-3.5" /> {busy === "up" ? "Writing…" : "Upload files"}
            <input type="file" multiple className="sr-only" aria-label="Files to upload" onChange={(e) => void upload(e.target.files)} />
          </label>
        </div>
        <p className="text-muted">
          {said ?? "Read as the shell would. What's written runs next, as an instance's own files do (they go when it's replaced)."} From a terminal:{" "}
          <code className="font-mono text-fg">roll apps cp -s {service} -e {env} :/app/report.csv .</code>
        </p>
        {error && <p className="text-danger">{error}</p>}
      </div>
    </Panel>
  );
}
