import { KeyRound } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import {
  BODY_MB_STEPS,
  formatDuration,
  IDLE_OPTIONS,
  IN_FLIGHT_STEPS,
  searchApi,
  urlWithPassword,
  type SearchCluster,
  type SearchInfo,
  type SearchSettings,
  type SearchStatus,
} from "@/lib/search-api";
import { btn, CopyButton, input, Panel, primary, SecretField, useCopy } from "../ui";
import { Field, POLL_MS, Segmented } from "../redis/shared";

export { useAction, type Run } from "../redis/shared";

/** Poll the control plane: search info and every cluster. */
export function useSearchClusters() {
  const [info, setInfo] = useState<SearchInfo | null>(null);
  const [clusters, setClusters] = useState<SearchCluster[] | null>(null);
  const [offline, setOffline] = useState(false);

  const refresh = useCallback(async () => {
    try {
      const [i, c] = await Promise.all([searchApi.info(), searchApi.list()]);
      setInfo(i);
      setClusters(c);
      setOffline(false);
    } catch {
      setOffline(true);
    }
  }, []);

  useEffect(() => {
    let alive = true;
    let timer: ReturnType<typeof setTimeout>;
    const loop = async () => {
      await refresh();
      if (alive) timer = setTimeout(loop, POLL_MS);
    };
    loop();
    return () => {
      alive = false;
      clearTimeout(timer);
    };
  }, [refresh]);

  return { info, clusters, offline, refresh };
}

/** Loaded (filled) or asleep (hollow). */
export function SearchStateDot({ state }: { state: SearchStatus["state"] | undefined }) {
  return (
    <span
      className={`inline-block size-2 shrink-0 rounded-full ${
        state === "loaded" ? "bg-ok" : "border border-line-2 bg-transparent"
      }`}
    />
  );
}

export function stateLabel(state: SearchStatus["state"] | undefined) {
  return state === "loaded" ? "loaded" : "asleep";
}

export function settingsOf(c: SearchCluster): SearchSettings {
  return {
    flavor: c.flavor,
    idle_timeout_s: c.idle_timeout_s,
    max_in_flight: c.max_in_flight,
    max_body_mb: c.max_body_mb,
    destructive_requires_name: c.destructive_requires_name,
  };
}

function withCurrent(steps: number[], v: number) {
  return steps.includes(v) ? steps : [...steps, v].sort((a, b) => a - b);
}

/** The editable settings shared by the create form and the settings tab. */
export function SettingsFields({ value, onChange }: { value: SearchSettings; onChange: (v: SearchSettings) => void }) {
  const set = <K extends keyof SearchSettings>(k: K, v: SearchSettings[K]) => onChange({ ...value, [k]: v });
  const idle = IDLE_OPTIONS.some((o) => o.value === value.idle_timeout_s)
    ? IDLE_OPTIONS
    : [...IDLE_OPTIONS, { label: formatDuration(value.idle_timeout_s), value: value.idle_timeout_s }];
  return (
    <div className="grid gap-x-3 gap-y-4 sm:grid-cols-2 lg:grid-cols-3">
      <Field label="API" hint={value.flavor === "opensearch" ? "OpenSearch 3 dialect" : "Elasticsearch 9 dialect"}>
        <Segmented
          value={value.flavor}
          options={[
            { label: "elasticsearch", value: "elasticsearch" },
            { label: "opensearch", value: "opensearch" },
          ]}
          onChange={(v) => set("flavor", v)}
        />
      </Field>
      <Field label="Sleep after" hint={value.idle_timeout_s === 0 ? "never unloads" : "without requests"}>
        <select
          value={value.idle_timeout_s}
          onChange={(e) => set("idle_timeout_s", Number(e.target.value))}
          className={`${input} w-full`}
        >
          {idle.map((o) => (
            <option key={o.value} value={o.value}>
              {o.label}
            </option>
          ))}
        </select>
      </Field>
      <Field label="Requests in flight" hint="as many more queue; then 429">
        <select
          value={value.max_in_flight}
          onChange={(e) => set("max_in_flight", Number(e.target.value))}
          className={`${input} w-full font-mono text-[12px]`}
        >
          {withCurrent(IN_FLIGHT_STEPS, value.max_in_flight).map((n) => (
            <option key={n} value={n}>
              {n}
            </option>
          ))}
        </select>
      </Field>
      <Field label="Largest request" hint="http.max_content_length">
        <select
          value={value.max_body_mb}
          onChange={(e) => set("max_body_mb", Number(e.target.value))}
          className={`${input} w-full font-mono text-[12px]`}
        >
          {withCurrent(BODY_MB_STEPS, value.max_body_mb).map((n) => (
            <option key={n} value={n}>
              {n >= 1024 ? `${n / 1024} GB` : `${n} MB`}
            </option>
          ))}
        </select>
      </Field>
      <Field label="Wildcard deletes" hint="action.destructive_requires_name">
        <Segmented
          value={value.destructive_requires_name ? "blocked" : "allowed"}
          options={[
            { label: "blocked", value: "blocked" },
            { label: "allowed", value: "allowed" },
          ]}
          onChange={(v) => set("destructive_requires_name", v === "blocked")}
        />
      </Field>
    </div>
  );
}

/** The password, the one time the API returns it. */
export function CredentialsOnce({ cluster, onDone, title = "Save the password" }: { cluster: SearchCluster; onDone: () => void; title?: string }) {
  const { copied, copy } = useCopy();
  const password = cluster.password ?? "";
  const full = urlWithPassword(cluster.url, cluster.username, password);
  const env = `ELASTICSEARCH_URL=${cluster.url}\nELASTICSEARCH_USERNAME=${cluster.username}\nELASTICSEARCH_PASSWORD=${password}\n`;
  return (
    <Panel
      title={
        <span className="flex items-center gap-2">
          <KeyRound className="size-4 text-accent" /> {title}
        </span>
      }
      aside={<span className="text-[12px] text-subtle">shown once</span>}
      className="rise mt-6"
    >
      <div className="grid gap-4 px-5 py-4 lg:grid-cols-2">
        <div className="space-y-3">
          <Row label="URL" value={cluster.url} id="url" copied={copied} onCopy={copy} />
          <Row label="Username" value={cluster.username} id="user" copied={copied} onCopy={copy} />
          <div className="space-y-1">
            <p className="text-[11px] text-subtle">Password</p>
            <SecretField id="pw" value={password} masked="••••••••••••••••" copied={copied} onCopy={copy} />
          </div>
        </div>
        <div className="space-y-3">
          <div className="space-y-1">
            <p className="text-[11px] text-subtle">URL with credentials</p>
            <SecretField id="full" value={full} masked={urlWithPassword(cluster.url, cluster.username, "").replace(":@", ":••••••••@")} copied={copied} onCopy={copy} />
          </div>
          <p className="text-[12px] text-muted">
            roll keeps no way to show this password again: rotate it in Settings if it&apos;s lost. Clients may also send
            it as <span className="font-mono">Bearer {cluster.username}_&lt;password&gt;</span> or an{" "}
            <span className="font-mono">ApiKey</span> of <span className="font-mono">{cluster.username}:&lt;password&gt;</span>.
          </p>
          <div className="flex flex-wrap gap-2">
            <button onClick={() => copy("env", env)} className={btn}>
              {copied === "env" ? "Copied .env" : "Copy as .env"}
            </button>
            <button onClick={onDone} className={primary}>
              I&apos;ve saved it
            </button>
          </div>
        </div>
      </div>
    </Panel>
  );
}

function Row({ label, value, id, copied, onCopy }: { label: string; value: string; id: string; copied: string | null; onCopy: (k: string, t: string) => void }) {
  return (
    <div className="space-y-1">
      <p className="text-[11px] text-subtle">{label}</p>
      <div className="flex items-center gap-1 rounded-lg border border-line bg-bg px-2.5 py-2">
        <code className="min-w-0 flex-1 truncate font-mono text-[11px]">{value}</code>
        <CopyButton id={id} text={value} copied={copied} onCopy={onCopy} />
      </div>
    </div>
  );
}
