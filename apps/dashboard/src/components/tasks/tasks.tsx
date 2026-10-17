"use client";

import { DotField, HorizonMark } from "@roll/brand";
import { useCallback, useEffect, useState } from "react";
import { tasks, TASKS_API, TasksApiError, type TasksHealth } from "@/lib/tasks-api";
import { AppShell } from "../app-shell";
import { DlqTab } from "./dlq";
import { EventDrawer, EventsTab } from "./events";
import { FunctionsTab, RunDrawer } from "./functions";
import { MessageDrawer, MessagesTab } from "./messages";
import { OverviewTab } from "./overview";
import { QueuesTab } from "./queues";
import { SchedulesTab } from "./schedules";
import { SettingsTab } from "./settings";
import { TopicsTab } from "./topics";
import { ActionsProvider, DetailProvider, useActions, type Detail } from "./ui";

const TABS = [
  { id: "overview", label: "Overview" },
  { id: "messages", label: "Messages" },
  { id: "queues", label: "Queues" },
  { id: "topics", label: "Topics" },
  { id: "schedules", label: "Schedules" },
  { id: "dlq", label: "Dead letters" },
  { id: "functions", label: "Functions" },
  { id: "events", label: "Events" },
  { id: "settings", label: "Settings" },
] as const;

export type Tab = (typeof TABS)[number]["id"];

const HEALTH_MS = 5000;

type Conn = "connecting" | "online" | "offline" | "unauthorized";

export function Tasks() {
  return (
    <ActionsProvider>
      <TasksConsole />
    </ActionsProvider>
  );
}

function TasksConsole() {
  const { error, setError } = useActions();
  const [tab, setTabState] = useState<Tab>("overview");
  const [conn, setConn] = useState<Conn>("connecting");
  const [health, setHealth] = useState<TasksHealth | null>(null);
  const [detail, setDetail] = useState<Detail | null>(null);

  // The tab lives in the URL hash so reloads and links keep it.
  useEffect(() => {
    const read = () => {
      const h = window.location.hash.slice(1);
      if (TABS.some((t) => t.id === h)) setTabState(h as Tab);
    };
    read();
    window.addEventListener("hashchange", read);
    return () => window.removeEventListener("hashchange", read);
  }, []);

  const setTab = useCallback(
    (t: Tab) => {
      setTabState(t);
      setError(null);
      window.history.replaceState(null, "", `#${t}`);
    },
    [setError],
  );

  useEffect(() => {
    let alive = true;
    let timer: ReturnType<typeof setTimeout>;
    const loop = async () => {
      try {
        const h = await tasks.health();
        // /health is open; probe an authenticated route when the server wants a token.
        if (h.auth) await tasks.stats();
        if (!alive) return;
        setHealth(h);
        setConn("online");
      } catch (e) {
        if (!alive) return;
        setConn(e instanceof TasksApiError && e.status === 401 ? "unauthorized" : "offline");
      }
      if (alive) timer = setTimeout(loop, HEALTH_MS);
    };
    loop();
    return () => {
      alive = false;
      clearTimeout(timer);
    };
  }, []);

  const close = useCallback(() => setDetail(null), []);

  return (
    <DetailProvider value={setDetail}>
      <AppShell crumb="tasks">
        <div className="flex flex-wrap items-end justify-between gap-3">
          <div>
            <h1 className="text-2xl font-semibold tracking-[-0.04em]">Tasks</h1>
            <p className="mt-1 text-[13px] text-muted">
              Durable messages, queues, schedules and workflows over HTTP — retried until they land.
            </p>
          </div>
          {health && (
            <p className="font-mono text-[11px] text-subtle">
              tasks {health.version} · {health.store.split(" ")[0]} · {health.auth ? "token auth" : "open"}
            </p>
          )}
        </div>

        <nav className="mt-6 flex gap-1 overflow-x-auto border-b border-line" aria-label="Tasks sections">
          {TABS.map((t) => (
            <button
              key={t.id}
              onClick={() => setTab(t.id)}
              aria-current={tab === t.id ? "page" : undefined}
              className={`-mb-px shrink-0 border-b px-2.5 pt-1 pb-2.5 text-[13px] transition ${
                tab === t.id ? "border-fg text-fg" : "border-transparent text-muted hover:text-fg"
              }`}
            >
              {t.label}
            </button>
          ))}
        </nav>

        {error && (
          <p
            onClick={() => setError(null)}
            className="rise mt-4 cursor-pointer rounded-lg border border-danger/40 bg-danger/10 px-3 py-2 text-[13px] text-danger"
          >
            {error}
          </p>
        )}

        {conn === "offline" ? (
          <Offline />
        ) : conn === "unauthorized" ? (
          <Unauthorized />
        ) : conn === "connecting" ? (
          <p className="mt-10 text-[13px] text-subtle">
            <span className="shimmer">Connecting to roll tasks…</span>
          </p>
        ) : (
          <div className="mt-6" key={tab}>
            {tab === "overview" && <OverviewTab go={setTab} />}
            {tab === "messages" && <MessagesTab />}
            {tab === "queues" && <QueuesTab />}
            {tab === "topics" && <TopicsTab />}
            {tab === "schedules" && <SchedulesTab />}
            {tab === "dlq" && <DlqTab />}
            {tab === "functions" && <FunctionsTab />}
            {tab === "events" && <EventsTab />}
            {tab === "settings" && <SettingsTab />}
          </div>
        )}

        {detail?.kind === "message" && <MessageDrawer key={detail.id} id={detail.id} onClose={close} />}
        {detail?.kind === "run" && <RunDrawer key={detail.id} id={detail.id} onClose={close} />}
        {detail?.kind === "event" && <EventDrawer key={detail.id} id={detail.id} onClose={close} />}
      </AppShell>
    </DetailProvider>
  );
}

function Offline() {
  return (
    <section className="relative isolate mt-6 overflow-hidden rounded-2xl border border-line bg-panel">
      <DotField spacing={18} radius={90} className="-z-10 text-fg" />
      <div className="flex flex-col items-start gap-3 p-8">
        <HorizonMark state="sleep" size={40} />
        <h2 className="text-[15px] font-medium">roll tasks isn&apos;t reachable</h2>
        <p className="max-w-md text-[13px] text-muted">
          The dashboard talks to the tasks API at <code className="font-mono text-fg">{TASKS_API}</code>, served by
          rolld. Start it from the repo root:
        </p>
        <pre className="rounded-lg border border-line bg-bg px-3 py-2 font-mono text-[12px]">cargo run -p rolld</pre>
        <p className="max-w-md text-[12px] text-subtle">
          Or run tasks on its own with <code className="font-mono">cargo run -p tasks --bin roll-tasks</code> and point{" "}
          <code className="font-mono">NEXT_PUBLIC_ROLL_API</code> at it.
        </p>
      </div>
    </section>
  );
}

function Unauthorized() {
  return (
    <section className="relative isolate mt-6 overflow-hidden rounded-2xl border border-line bg-panel">
      <DotField spacing={18} radius={90} className="-z-10 text-fg" />
      <div className="flex flex-col items-start gap-3 p-8">
        <HorizonMark state="error" size={40} />
        <h2 className="text-[15px] font-medium">The tasks API wants a token</h2>
        <p className="max-w-md text-[13px] text-muted">
          rolld was started with <code className="font-mono text-fg">ROLL_TASKS_TOKEN</code>. Give the dashboard the
          same token (or an API key) and restart it:
        </p>
        <pre className="rounded-lg border border-line bg-bg px-3 py-2 font-mono text-[12px]">
          NEXT_PUBLIC_ROLL_TASKS_TOKEN=… pnpm dev
        </pre>
      </div>
    </section>
  );
}
