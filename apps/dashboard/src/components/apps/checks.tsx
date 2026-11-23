import { Plus, Trash2 } from "lucide-react";
import { useState } from "react";
import { appsApi, type Environment, type UptimeCheck } from "@/lib/apps-api";
import { input, Panel, primary } from "../ui";
import { since, SureButton, type Runner } from "./shared";

const NAME = /^[a-z0-9-]{1,32}$/;

/**
 * An environment's uptime checks (Cloud Monitoring's): a path GET'd every so often through its
 * own proxy; two failures in a row are `check.down` for its notification targets, the next
 * pass resolves it. A check wakes a service that scales to zero. With a browser script (Checkly's,
 * through Vercel) the page loads in Chromium and the script runs against it, every 5 min at most.
 */
export function ChecksPanel({ service, env, runner }: { service: string; env: Environment; runner: Runner }) {
  const checks = env.checks ?? [];
  const [name, setName] = useState("");
  const [path, setPath] = useState("/");
  const [every, setEvery] = useState("60");
  const [contains, setContains] = useState("");
  const [script, setScript] = useState("");
  const save = (next: UptimeCheck[]) => runner.run("checks", () => appsApi.putEnvironment(service, env.name, { checks: next.length ? next : null }));
  const ok = NAME.test(name) && !checks.some((c) => c.name === name) && path.startsWith("/") && !/\s/.test(path) && Number(every) >= (script.trim() ? 300 : 30) && Number(every) <= 3600 && script.length <= 16384 && checks.length < 5;
  const busy = runner.busy !== null;
  // A failure's file, opened (the screenshot) or saved (the trace) from a blob: the routes need the session.
  const open = async (check: string, which: "screenshot" | "trace") => {
    const blob = await appsApi.checkFailure(service, env.name, check, which).catch(() => null);
    if (!blob) return;
    const url = URL.createObjectURL(blob);
    if (which === "screenshot") window.open(url, "_blank", "noopener");
    else Object.assign(document.createElement("a"), { href: url, download: `${check}-trace.zip` }).click();
    setTimeout(() => URL.revokeObjectURL(url), 60_000);
  };
  return (
    <Panel title="Uptime checks" aside={<span className="text-[12px] text-muted">two failures in a row notify; a check keeps a sleeping service awake</span>}>
      {checks.length > 0 && (
        <ul className="divide-y divide-line border-b border-line">
          {checks.map((c) => {
            const st = env.check_states?.find((s) => s.name === c.name);
            const tone = st?.up === false ? "bg-danger" : st?.up ? "bg-accent" : "bg-subtle";
            return (
              <li key={c.name} className="flex flex-wrap items-center gap-x-3 gap-y-1 px-5 py-2.5 text-[12.5px]">
                <i className={`size-1.5 rounded-full ${tone}`} aria-label={st?.up === false ? "down" : st?.up ? "up" : "not tried yet"} />
                <span className="font-medium">{c.name}</span>
                <code className="min-w-0 flex-1 truncate font-mono text-[12px]">
                  {c.browser ? "Browser" : "GET"} {c.path}
                  {c.contains ? ` · contains ${JSON.stringify(c.contains)}` : ""}
                </code>
                <span className="text-[11.5px] text-subtle">
                  every {c.every_s ?? 60}s
                  {st?.last_at ? ` · ${st.status ?? "no answer"}${st.ms != null ? ` in ${st.ms} ms` : ""}, ${since(st.last_at, Date.now())}` : ""}
                </span>
                {st?.metrics && (
                  <span className="w-full font-mono text-[11.5px] text-subtle">
                    LCP {st.metrics.lcp_ms} ms · CLS {st.metrics.cls} · TTFB {st.metrics.ttfb_ms} ms · {st.metrics.kb} KiB · {st.metrics.requests} requests
                    {c.budgets ? ` (budgets: ${Object.entries(c.budgets).map(([k, v]) => `${k} ${v}`).join(", ")})` : ""}
                  </span>
                )}
                {st?.over_budget && <span className="w-full truncate text-[11.5px] text-muted">{st.over_budget}</span>}
                {st?.error && <span className="w-full truncate text-[11.5px] text-danger">{st.error}</span>}
                {c.browser && st?.failure_at != null && (
                  <span className="flex w-full gap-3 text-[11.5px]">
                    <button type="button" className="text-accent hover:underline" onClick={() => void open(c.name, "screenshot")}>
                      Screenshot of the failure
                    </button>
                    <button type="button" className="text-accent hover:underline" onClick={() => void open(c.name, "trace")} title="open it with npx playwright show-trace, or at trace.playwright.dev">
                      Its Playwright trace
                    </button>
                  </span>
                )}
                <SureButton className="p-1 text-subtle hover:text-fg" label={`Remove the check ${c.name}`} sure="Sure? Remove" disabled={busy} onSure={() => save(checks.filter((x) => x.name !== c.name))}>
                  <Trash2 className="size-3.5" />
                </SureButton>
              </li>
            );
          })}
        </ul>
      )}
      <form
        className="flex flex-wrap gap-2 px-5 py-4"
        onSubmit={(e) => {
          e.preventDefault();
          void save([...checks, { name, path: path.trim(), every_s: Number(every), ...(contains.trim() ? { contains: contains.trim() } : {}), ...(script.trim() ? { browser: script, timeout_s: 60 } : {}) }]).then((r) => {
            if (!r) return;
            setName("");
            setPath("/");
            setContains("");
            setScript("");
          });
        }}
      >
        <input value={name} onChange={(e) => setName(e.target.value.toLowerCase())} placeholder="health" aria-label="Check name" spellCheck={false} className={`${input} w-32 font-mono text-[12px]`} />
        <input value={path} onChange={(e) => setPath(e.target.value)} placeholder="/healthz" aria-label="Path" spellCheck={false} className={`${input} min-w-40 flex-1 font-mono text-[12px]`} />
        <input value={contains} onChange={(e) => setContains(e.target.value)} placeholder="its answer contains (optional)" aria-label="Contains" spellCheck={false} className={`${input} w-52 text-[12px]`} />
        <select value={every} onChange={(e) => setEvery(e.target.value)} aria-label="Every" className={`${input} w-28 text-[12px]`}>
          {[["30", "30 s"], ["60", "1 min"], ["300", "5 min"], ["900", "15 min"], ["3600", "1 h"]].map(([v, l]) => (
            <option key={v} value={v}>
              every {l}
            </option>
          ))}
        </select>
        <textarea
          value={script}
          onChange={(e) => setScript(e.target.value)}
          rows={script ? 4 : 1}
          placeholder="a browser check's Playwright script (optional): await page.getByRole('button', { name: 'Sign in' }).click()"
          aria-label="Browser script"
          spellCheck={false}
          className={`${input} w-full font-mono text-[12px]`}
        />
        {script.trim() && Number(every) < 300 && <span className="w-full text-[11.5px] text-muted">A browser check runs every 5 min at most.</span>}
        <button type="submit" className={primary} disabled={!ok || busy}>
          <Plus className="size-3.5" /> Add
        </button>
      </form>
    </Panel>
  );
}
