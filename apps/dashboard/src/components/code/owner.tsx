"use client";

import { HorizonMark } from "@roll/brand";
import { ChevronDown, ChevronRight, ExternalLink, Plus, RotateCw, Trash2 } from "lucide-react";
import Link from "next/link";
import { useCallback, useEffect, useState } from "react";
import { codeApi, isNotFound, type CodeInfo, type Owner, type Repo, type ReviewApp, type ReviewAppSecrets } from "@/lib/code-api";
import { canWrite } from "@/lib/queue";
import { ago, btn, Confirm, Connecting, CopyButton, EmptyPanel, ErrorNote, input, Offline, panel, Panel, primary, useCopy } from "../ui";
import { Monogram } from "../repo/avatar";
import { BillingUsage, PricingPanel, ProviderCard } from "./billing";
import { AccessTokens, ConnectAgent } from "./connect-agent";
import { DeliveryLog } from "./deliveries";
import { CodeShell } from "./shell";
import { Field, GitHubMark, POLL_MS, useAction, VisibilityBadge, type Run } from "./shared";

const EVENTS: { value: string; hint: string }[] = [
  { value: "change.opened", hint: "a change is opened" },
  { value: "change.revised", hint: "a change gets a new revision" },
  { value: "thread.comment", hint: "someone comments in a thread" },
  { value: "review.requested", hint: "a review is requested from the app (always sent to required apps)" },
];
const DEFAULT_EVENTS = ["change.opened", "change.revised", "thread.comment"];

/** An owner in the dashboard: their repositories and the review apps they've registered. */
export function CodeOwner({ name }: { name: string }) {
  const [info, setInfo] = useState<CodeInfo | null>(null);
  const [owner, setOwner] = useState<(Owner & { repos: Repo[] }) | null>(null);
  const [offline, setOffline] = useState(false);
  const [missing, setMissing] = useState(false);
  const [tokensTick, setTokensTick] = useState(0);

  const refresh = useCallback(async () => {
    const [i, o] = await Promise.allSettled([codeApi.info(), codeApi.owner(name)]);
    if (i.status === "rejected") return setOffline(true);
    setOffline(false);
    setInfo(i.value);
    if (o.status === "fulfilled") (setOwner(o.value), setMissing(false));
    else if (isNotFound(o.reason)) setMissing(true);
  }, [name]);

  useEffect(() => {
    let alive = true;
    let timer: ReturnType<typeof setTimeout>;
    const loop = async () => {
      await refresh();
      if (alive) timer = setTimeout(loop, POLL_MS * 4);
    };
    loop();
    return () => {
      alive = false;
      clearTimeout(timer);
    };
  }, [refresh]);

  const { busy, error, run } = useAction(refresh);
  const crumb = (
    <>
      <Link href="/dashboard/code" className="text-muted hover:text-fg">
        code
      </Link>{" "}
      <span className="text-subtle">/</span> {name}
    </>
  );

  if (offline)
    return (
      <CodeShell crumb={crumb}>
        <Offline />
      </CodeShell>
    );
  if (missing)
    return (
      <CodeShell crumb={crumb}>
        <EmptyPanel>
          <HorizonMark state="sleep" size={40} />
          <h2 className="text-[15px] font-medium">No owner {name}</h2>
          <p className="max-w-md text-[13px] text-muted">Owner names are case-insensitive; check the spelling.</p>
        </EmptyPanel>
      </CodeShell>
    );
  if (!owner || !info)
    return (
      <CodeShell crumb={crumb}>
        <Connecting />
      </CodeShell>
    );

  const manage = canWrite(info, { owner: owner.name });
  return (
    <CodeShell
      crumb={crumb}
      actions={
        <Link href={`/${owner.name}`} className={btn}>
          <ExternalLink className="size-3.5" /> View on roll.sh
        </Link>
      }
    >
      <div className="flex items-center gap-4">
        <Monogram name={owner.name} size={48} org={owner.kind === "org"} />
        <div className="min-w-0">
          <h1 className="truncate text-2xl font-semibold tracking-[-0.04em]">{owner.display_name || owner.name}</h1>
          <p className="font-mono text-[12px] text-subtle">
            {owner.name} · {owner.kind === "org" ? "organisation" : "user"} · {owner.repos_count} repositor
            {owner.repos_count === 1 ? "y" : "ies"}
          </p>
        </div>
      </div>
      <ErrorNote error={error} />

      <div className="mt-6 space-y-4">
        <Panel title="Repositories" aside={<span className="font-mono text-[12px] text-subtle">{owner.repos.length}</span>}>
          {owner.repos.length === 0 ? (
            <p className="px-5 py-4 text-[12px] text-subtle">No repositories yet.</p>
          ) : (
            <ul>
              {owner.repos.map((r) => (
                <li key={r.id} className="border-b border-line last:border-0">
                  <Link href={`/dashboard/code/${r.full_name}`} className="flex items-center gap-3 px-5 py-3 transition hover:bg-panel-2/60">
                    <span className="grid size-6 place-items-center rounded-md border border-line text-muted">
                      {r.github ? <GitHubMark className="size-3.5" /> : <HorizonMark size={14} interactive={false} />}
                    </span>
                    <span className="min-w-0 flex-1 truncate text-[13px]">{r.name}</span>
                    <VisibilityBadge visibility={r.visibility} />
                    <ChevronRight className="size-3.5 text-subtle" />
                  </Link>
                </li>
              ))}
            </ul>
          )}
        </Panel>

        <ConnectAgent owner={owner.name} info={info} manage={manage} onTokensChanged={() => setTokensTick((n) => n + 1)} />
        {manage && <AccessTokens owner={owner.name} reload={tokensTick} />}
        <ReviewApps owner={owner.name} repos={owner.repos} manage={manage} busy={busy} run={run} />
        {manage && (
          <>
            <BillingUsage owner={owner.name} busy={busy} run={run} />
            <ProviderCard owner={owner.name} busy={busy} run={run} />
          </>
        )}
        <PricingPanel />
      </div>
    </CodeShell>
  );
}

/** The token and secret, shown once, with copy. */
function SecretsOnce({ title, secrets, onDone }: { title: string; secrets: ReviewAppSecrets; onDone: () => void }) {
  const { copied, copy } = useCopy();
  return (
    <div className="rise space-y-3 rounded-lg border border-accent/40 bg-accent/5 p-4">
      <p className="text-[13px]">{title}</p>
      <p className="text-[12px] text-muted">
        Shown once. The app sends the token as <span className="font-mono">Authorization: Bearer</span> and checks{" "}
        <span className="font-mono">X-Roll-Signature-256</span> with the secret.
      </p>
      {(
        [
          ["token", secrets.token],
          ["secret", secrets.secret],
        ] as const
      ).map(([k, v]) => (
        <div key={k} className="flex items-center gap-2">
          <span className="w-12 shrink-0 text-[11px] text-subtle">{k}</span>
          <code className="min-w-0 flex-1 truncate rounded-lg border border-line bg-bg px-2.5 py-1.5 font-mono text-[11px]">{v}</code>
          <CopyButton id={k} text={v} copied={copied} onCopy={copy} />
        </div>
      ))}
      <button type="button" onClick={onDone} className={btn}>
        I&apos;ve saved them
      </button>
    </div>
  );
}

/**
 * Review apps: external reviewers (any vendor) that get signed webhooks for changes and post
 * reviews and comments with their token.
 */
function ReviewApps({ owner, repos, manage, busy, run }: { owner: string; repos: Repo[]; manage: boolean; busy: string | null; run: Run }) {
  const [apps, setApps] = useState<ReviewApp[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);
  const [secrets, setSecrets] = useState<{ title: string; value: ReviewAppSecrets } | null>(null);
  const [version, setVersion] = useState(0);

  useEffect(() => {
    if (!manage) return;
    let alive = true;
    codeApi.reviewApps(owner).then(
      (a) => alive && (setApps(a), setError(null)),
      (e) => alive && setError(e instanceof Error ? e.message : String(e)),
    );
    return () => {
      alive = false;
    };
  }, [owner, manage, version]);

  const act = async <T,>(key: string, fn: () => Promise<T>) => {
    const out = await run(key, fn);
    setVersion((v) => v + 1);
    return out;
  };

  return (
    <section id="review-apps" className="scroll-mt-24">
      <Panel
        title="Review apps"
        aside={
          manage &&
          !adding && (
            <button onClick={() => setAdding(true)} className={btn}>
              <Plus className="size-3.5" /> Register
            </button>
          )
        }
      >
        <div className="space-y-4 px-5 py-4">
          <p className="max-w-2xl text-[12px] text-subtle">
            Any reviewer can plug into roll: it gets signed webhooks with diff and blob links for every change, and posts reviews and
            inline comments back with its token, like a person would. A required app owes every new revision a review before it
            lands.
          </p>
          {!manage && <p className="text-[12px] text-subtle">Only {owner} (or an admin) can see and manage its review apps.</p>}
          {error && <p className="text-[12px] text-danger">{error}</p>}
          {secrets && <SecretsOnce title={secrets.title} secrets={secrets.value} onDone={() => setSecrets(null)} />}
          {adding && (
            <RegisterApp
              repos={repos}
              busy={busy}
              onCancel={() => setAdding(false)}
              onCreate={async (input) => {
                const out = await act("app-create", () => codeApi.createReviewApp(owner, input));
                if (out) {
                  setAdding(false);
                  setSecrets({ title: `Registered ${out.app.name}.`, value: { token: out.token, secret: out.secret } });
                }
              }}
            />
          )}
          {manage && apps && apps.length === 0 && !adding && <p className="text-[12px] text-subtle">No review apps yet.</p>}
          {apps && apps.length > 0 && (
            <ul className={`${panel} bg-bg`}>
              {apps.map((a) => (
                <AppRow
                  key={a.id}
                  app={a}
                  busy={busy}
                  act={act}
                  onRotated={(s) => setSecrets({ title: `New credentials for ${a.name}. The old ones stopped working.`, value: s })}
                />
              ))}
            </ul>
          )}
        </div>
      </Panel>
    </section>
  );
}

function RegisterApp({
  repos,
  busy,
  onCreate,
  onCancel,
}: {
  repos: Repo[];
  busy: string | null;
  onCreate: (input: { name: string; webhook_url: string; events: string[]; repos?: string[]; required: boolean }) => void;
  onCancel: () => void;
}) {
  const [name, setName] = useState("");
  const [url, setUrl] = useState("https://");
  const [events, setEvents] = useState<string[]>(DEFAULT_EVENTS);
  const [all, setAll] = useState(true);
  const [picked, setPicked] = useState<string[]>([]);
  const [required, setRequired] = useState(false);
  const urlOk = /^https?:\/\/[^\s/]+/.test(url);
  const ok = name.trim().length > 0 && urlOk && events.length > 0 && (all || picked.length > 0);
  const toggle = (list: string[], v: string) => (list.includes(v) ? list.filter((x) => x !== v) : [...list, v]);
  return (
    <form
      className="rise space-y-4 rounded-lg border border-line bg-bg p-4"
      onSubmit={(e) => {
        e.preventDefault();
        if (ok) onCreate({ name: name.trim(), webhook_url: url.trim(), events, ...(all ? {} : { repos: picked }), required });
      }}
    >
      <div className="grid gap-4 sm:grid-cols-2">
        <Field label="Name" hint="shown on its reviews, e.g. greptile">
          <input value={name} onChange={(e) => setName(e.target.value)} autoFocus spellCheck={false} className={`${input} w-full font-mono text-[12px]`} />
        </Field>
        <Field label="Webhook URL" hint={urlOk ? "public https endpoint; private addresses are refused" : "an http(s) URL"}>
          <input value={url} onChange={(e) => setUrl(e.target.value)} spellCheck={false} className={`${input} w-full font-mono text-[12px]`} />
        </Field>
      </div>
      <fieldset className="space-y-1.5">
        <legend className="text-[11px] text-subtle">Events</legend>
        {EVENTS.map((ev) => (
          <label key={ev.value} className="flex items-center gap-2 text-[12px]">
            <input type="checkbox" checked={events.includes(ev.value)} onChange={() => setEvents((l) => toggle(l, ev.value))} className="accent-[var(--accent)]" />
            <span className="font-mono">{ev.value}</span>
            <span className="text-subtle">{ev.hint}</span>
          </label>
        ))}
      </fieldset>
      <fieldset className="space-y-1.5">
        <legend className="text-[11px] text-subtle">Repositories</legend>
        <label className="flex items-center gap-2 text-[12px]">
          <input type="checkbox" checked={all} onChange={(e) => setAll(e.target.checked)} className="accent-[var(--accent)]" />
          All repositories, including future ones
        </label>
        {!all && (
          <div className="ml-5 max-h-40 space-y-1 overflow-y-auto">
            {repos.length === 0 && <p className="text-[12px] text-subtle">No repositories yet.</p>}
            {repos.map((r) => (
              <label key={r.id} className="flex items-center gap-2 text-[12px]">
                <input type="checkbox" checked={picked.includes(r.full_name)} onChange={() => setPicked((l) => toggle(l, r.full_name))} className="accent-[var(--accent)]" />
                <span className="font-mono">{r.full_name}</span>
              </label>
            ))}
          </div>
        )}
      </fieldset>
      <label className="flex items-center gap-2 text-[12px]">
        <input type="checkbox" checked={required} onChange={(e) => setRequired(e.target.checked)} className="accent-[var(--accent)]" />
        Required — every new revision waits for its review before it can land
      </label>
      <div className="flex gap-2">
        <button type="submit" disabled={!ok || busy !== null} className={primary}>
          {busy === "app-create" ? <span className="shimmer">Registering…</span> : "Register app"}
        </button>
        <button type="button" onClick={onCancel} className={btn}>
          Cancel
        </button>
      </div>
    </form>
  );
}

function AppRow({
  app,
  busy,
  act,
  onRotated,
}: {
  app: ReviewApp;
  busy: string | null;
  act: <T>(key: string, fn: () => Promise<T>) => Promise<T | undefined>;
  onRotated: (s: ReviewAppSecrets) => void;
}) {
  const [open, setOpen] = useState(false);
  const [deleting, setDeleting] = useState(false);
  return (
    <li className="border-b border-line last:border-0">
      <div className="flex flex-wrap items-center gap-3 px-4 py-3">
        <button type="button" onClick={() => setOpen((o) => !o)} className="text-subtle hover:text-fg" aria-label={open ? "Hide deliveries" : "Show deliveries"}>
          {open ? <ChevronDown className="size-3.5" /> : <ChevronRight className="size-3.5" />}
        </button>
        <Monogram name={app.name} size={22} org />
        <div className="min-w-0 flex-1">
          <p className="flex items-center gap-2 text-[13px]">
            <span className="truncate">{app.name}</span>
            {app.required && <span className="rounded-full border border-line px-1.5 py-px text-[10px] text-subtle">required</span>}
            {!app.active && <span className="rounded-full border border-line px-1.5 py-px text-[10px] text-subtle">paused</span>}
          </p>
          <p className="truncate font-mono text-[11px] text-subtle">
            {app.webhook_url} · {app.all_repos ? "all repos" : `${app.repos.length} repo${app.repos.length === 1 ? "" : "s"}`} · {app.events.join(", ")}
          </p>
        </div>
        <span className="flex items-center gap-1.5">
          <button
            type="button"
            onClick={() => act(`app-active-${app.id}`, () => codeApi.updateReviewApp(app.id, { active: !app.active }))}
            disabled={busy !== null}
            className={btn}
          >
            {app.active ? "Pause" : "Resume"}
          </button>
          <button
            type="button"
            onClick={async () => {
              const s = await act(`app-rotate-${app.id}`, () => codeApi.rotateReviewApp(app.id));
              if (s) onRotated(s);
            }}
            disabled={busy !== null}
            className={btn}
            title="New token and secret; the old ones stop working"
          >
            <RotateCw className="size-3.5" /> Rotate
          </button>
          <button type="button" onClick={() => setDeleting(true)} disabled={busy !== null} className={btn} aria-label={`Delete ${app.name}`}>
            <Trash2 className="size-3.5" />
          </button>
        </span>
      </div>
      {deleting && (
        <div className="px-4 pb-3">
          <Confirm
            word={app.name}
            action="Delete app"
            busy={busy === `app-delete-${app.id}`}
            onCancel={() => setDeleting(false)}
            onConfirm={() => act(`app-delete-${app.id}`, () => codeApi.deleteReviewApp(app.id))}
          >
            Deletes <span className="font-mono text-fg">{app.name}</span>: its token stops working and no more webhooks are sent. Its past
            reviews stay.
          </Confirm>
        </div>
      )}
      {open && (
        <DeliveryLog
          load={() => codeApi.deliveries(app.id)}
          redeliver={(d) => codeApi.redeliver(app.id, d)}
          deps={app.id}
          busy={busy}
          act={act}
        />
      )}
    </li>
  );
}
