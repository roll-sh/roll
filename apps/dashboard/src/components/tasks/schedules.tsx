"use client";

import { Pause, Play, Plus, Trash2, Zap } from "lucide-react";
import { useState } from "react";
import { cronHint, formatTime, rel, tasks, type Schedule, type ScheduleRequest } from "@/lib/tasks-api";
import {
  btn,
  Chip,
  Empty,
  ICON,
  iconBtn,
  IdLink,
  InlineError,
  input,
  Loading,
  Panel,
  parseJsonInput,
  primary,
  Segmented,
  Spinner,
  StateDot,
  useActions,
  useNow,
  usePoll,
} from "./ui";

const PRESETS = ["*/5 * * * *", "0 * * * *", "0 9 * * *", "0 9 * * 1-5", "@every 30s"];

function targetLabel(s: Schedule) {
  const t = s.target;
  switch (t.type) {
    case "publish": {
      const r = t.request;
      return { kind: "publish", text: r.url ?? (r.topic ? `topic ${r.topic}` : r.queue ? `queue ${r.queue}` : "—") };
    }
    case "event":
      return { kind: "event", text: t.name };
    case "function":
      return { kind: "function", text: t.function_id };
  }
}

export function SchedulesTab() {
  const { run, busy } = useActions();
  const list = usePoll(() => tasks.schedules(), 5000);
  const [creating, setCreating] = useState(false);
  const [expanded, setExpanded] = useState<string | null>(null);
  const now = useNow(1000);
  const items = list.data ?? [];

  const act = async (key: string, fn: () => Promise<unknown>) => {
    const out = await run(key, fn);
    if (out !== undefined) list.reload();
    return out;
  };

  return (
    <Panel
      title="Schedules"
      meta={list.data ? items.length : undefined}
      actions={
        <button onClick={() => setCreating((c) => !c)} className={btn}>
          <Plus {...ICON} /> New schedule
        </button>
      }
    >
      {creating && (
        <NewSchedule
          busy={busy === "sched-new"}
          onCancel={() => setCreating(false)}
          onCreate={async (req) => {
            if ((await act("sched-new", () => tasks.createSchedule(req))) !== undefined) setCreating(false);
          }}
        />
      )}
      <InlineError error={list.error} />
      {!list.data ? (
        <Loading />
      ) : items.length === 0 ? (
        <Empty title="No schedules yet">Cron or @every, in any time zone. Each fire publishes a message, sends an event or starts a run.</Empty>
      ) : (
        <ul>
          {items.map((s) => {
            const t = targetLabel(s);
            const hint = cronHint(s.cron);
            const owned = s.id.startsWith("fn-");
            const open = expanded === s.id;
            return (
              <li key={s.id} className="border-b border-line last:border-0">
                <div
                  className="flex cursor-pointer items-center gap-3 px-5 py-3 transition hover:bg-panel-2/60"
                  onClick={() => setExpanded(open ? null : s.id)}
                >
                  <StateDot state={s.paused ? "paused" : "pending"} title={s.paused ? "paused" : "armed"} />
                  <div className="w-44 shrink-0">
                    <p className="font-mono text-[13px]">{s.cron}</p>
                    <p className="truncate text-[11px] text-subtle">
                      {hint ?? "custom"}
                      {s.tz && ` · ${s.tz}`}
                    </p>
                  </div>
                  <div className="min-w-0 flex-1">
                    <p className="flex items-center gap-2 truncate text-[13px]">
                      <Chip>{t.kind}</Chip>
                      <span className="truncate">{t.text}</span>
                    </p>
                    <p className="truncate font-mono text-[11px] text-subtle">
                      {s.id}
                      {owned && " · function trigger"}
                      {s.label && ` · ${s.label}`}
                    </p>
                  </div>
                  <div className="hidden w-32 text-right text-[12px] sm:block">
                    <p className={s.paused ? "text-subtle" : "text-accent"} title={formatTime(s.next_at)}>
                      {s.paused ? "paused" : s.next_at ? rel(s.next_at, now) : "—"}
                    </p>
                    <p className="text-[11px] text-subtle" title={formatTime(s.last_at)}>
                      {s.last_at ? `last ${rel(s.last_at, now)}` : "never fired"}
                    </p>
                  </div>
                  <div className="flex items-center gap-0.5" onClick={(e) => e.stopPropagation()}>
                    <button
                      title="Trigger now"
                      disabled={busy !== null}
                      onClick={() => act(`fire-${s.id}`, () => tasks.triggerSchedule(s.id))}
                      className={iconBtn}
                    >
                      {busy === `fire-${s.id}` ? <Spinner /> : <Zap {...ICON} />}
                    </button>
                    <button
                      title={s.paused ? "Resume" : "Pause"}
                      disabled={busy !== null}
                      onClick={() => act(`pause-${s.id}`, () => (s.paused ? tasks.resumeSchedule(s.id) : tasks.pauseSchedule(s.id)))}
                      className={iconBtn}
                    >
                      {s.paused ? <Play {...ICON} /> : <Pause {...ICON} />}
                    </button>
                    <button
                      title="Delete"
                      disabled={busy !== null}
                      onClick={() => {
                        const warn = owned ? " It belongs to a function trigger and comes back on the next app sync." : "";
                        if (confirm(`Delete schedule ${s.id}?${warn}`)) act(`del-${s.id}`, () => tasks.deleteSchedule(s.id));
                      }}
                      className={`${iconBtn} hover:text-danger`}
                    >
                      <Trash2 {...ICON} />
                    </button>
                  </div>
                </div>
                {open && <ScheduleDetail s={s} now={now} />}
              </li>
            );
          })}
        </ul>
      )}
    </Panel>
  );
}

function ScheduleDetail({ s, now }: { s: Schedule; now: number }) {
  return (
    <div className="rise grid gap-4 border-t border-line bg-panel-2/40 px-5 py-4 md:grid-cols-2">
      <div>
        <p className="mb-1.5 text-[12px] text-subtle">Upcoming</p>
        {s.upcoming?.length ? (
          <ol className="space-y-0.5 font-mono text-[11px]">
            {s.upcoming.map((t) => (
              <li key={t} className="flex justify-between gap-4">
                <span>{formatTime(t)}</span>
                <span className="text-subtle">{rel(t, now)}</span>
              </li>
            ))}
          </ol>
        ) : (
          <p className="text-[12px] text-subtle">{s.paused ? "Paused — resume to arm it." : "No upcoming fires."}</p>
        )}
        {s.last_message_id && (
          <p className="mt-3 flex items-center gap-1.5 text-[12px] text-subtle">
            Last message <IdLink kind="message" id={s.last_message_id} />
          </p>
        )}
      </div>
      <div>
        <p className="mb-1.5 text-[12px] text-subtle">Target</p>
        <pre className="max-h-48 overflow-auto rounded-lg border border-line bg-bg px-3 py-2 font-mono text-[11px] whitespace-pre-wrap break-all">
          {JSON.stringify(s.target, null, 2)}
        </pre>
      </div>
    </div>
  );
}

type Kind = "publish" | "event";

function NewSchedule({ busy, onCreate, onCancel }: { busy: boolean; onCreate: (r: ScheduleRequest) => void; onCancel: () => void }) {
  const { setError } = useActions();
  const [cron, setCron] = useState("*/5 * * * *");
  const [tz, setTz] = useState("");
  const [kind, setKind] = useState<Kind>("publish");
  const [url, setUrl] = useState("");
  const [eventName, setEventName] = useState("");
  const [data, setData] = useState("");
  const [id, setId] = useState("");
  const hint = cronHint(cron);
  const localTz = typeof Intl !== "undefined" ? Intl.DateTimeFormat().resolvedOptions().timeZone : "";

  const submit = () => {
    try {
      const body = parseJsonInput(data, kind === "publish" ? "body" : "data");
      const req: ScheduleRequest = { cron: cron.trim(), tz: tz.trim() || undefined, id: id.trim() || undefined };
      if (kind === "publish") req.publish = { url: url.trim(), body };
      else req.event = { name: eventName.trim(), data: body ?? {} };
      onCreate(req);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  const ready = cron.trim() && (kind === "publish" ? url.trim() : eventName.trim());

  return (
    <form
      className="rise space-y-3 border-b border-line bg-panel-2/40 px-5 py-4"
      onSubmit={(e) => {
        e.preventDefault();
        if (ready) submit();
      }}
    >
      <div className="grid gap-3 md:grid-cols-3">
        <label className="space-y-1">
          <span className="block text-[11px] text-subtle">Cron</span>
          <input autoFocus value={cron} onChange={(e) => setCron(e.target.value)} className={`${input} w-full font-mono text-[12px]`} />
          <span className={`block text-[11px] ${hint ? "text-muted" : "text-subtle"}`}>{hint ?? "5 or 6 fields, @daily, or @every 10m"}</span>
        </label>
        <label className="space-y-1">
          <span className="block text-[11px] text-subtle">Time zone</span>
          <input value={tz} onChange={(e) => setTz(e.target.value)} placeholder="UTC" list="tz-suggest" className={`${input} w-full font-mono text-[12px]`} />
          <datalist id="tz-suggest">
            {[localTz, "UTC", "America/New_York", "Europe/London", "Europe/Berlin", "Asia/Tokyo"].filter(Boolean).map((z) => (
              <option key={z} value={z} />
            ))}
          </datalist>
          <span className="block text-[11px] text-subtle">IANA name; cron runs in it, DST included</span>
        </label>
        <label className="space-y-1">
          <span className="block text-[11px] text-subtle">Id (optional)</span>
          <input value={id} onChange={(e) => setId(e.target.value)} placeholder="nightly-report" className={`${input} w-full font-mono text-[12px]`} />
        </label>
      </div>
      <div className="flex flex-wrap gap-1.5">
        {PRESETS.map((p) => (
          <button key={p} type="button" onClick={() => setCron(p)} className="rounded-md border border-line px-1.5 py-0.5 font-mono text-[11px] text-subtle transition hover:text-fg">
            {p}
          </button>
        ))}
      </div>
      <div className="flex items-center gap-3">
        <Segmented
          value={kind}
          options={[
            { value: "publish", label: "Publish to URL" },
            { value: "event", label: "Send event" },
          ]}
          onChange={setKind}
        />
      </div>
      <div className="grid gap-3 md:grid-cols-2">
        {kind === "publish" ? (
          <label className="space-y-1">
            <span className="block text-[11px] text-subtle">URL</span>
            <input value={url} onChange={(e) => setUrl(e.target.value)} placeholder="https://example.com/api/cron" className={`${input} w-full font-mono text-[12px]`} />
          </label>
        ) : (
          <label className="space-y-1">
            <span className="block text-[11px] text-subtle">Event name</span>
            <input value={eventName} onChange={(e) => setEventName(e.target.value)} placeholder="report.due" className={`${input} w-full font-mono text-[12px]`} />
          </label>
        )}
        <label className="space-y-1">
          <span className="block text-[11px] text-subtle">{kind === "publish" ? "Body (JSON, optional)" : "Data (JSON, optional)"}</span>
          <textarea
            value={data}
            onChange={(e) => setData(e.target.value)}
            rows={2}
            spellCheck={false}
            placeholder='{"report": "daily"}'
            className={`${input} block w-full resize-y font-mono text-[12px]`}
          />
        </label>
      </div>
      <div className="flex justify-end gap-2">
        <button type="button" onClick={onCancel} className={btn}>
          Cancel
        </button>
        <button type="submit" disabled={busy || !ready} className={primary}>
          {busy ? <span className="shimmer">Creating…</span> : "Create schedule"}
        </button>
      </div>
    </form>
  );
}
