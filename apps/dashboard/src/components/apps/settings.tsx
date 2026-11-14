import { Trash2 } from "lucide-react";
import { useEffect, useMemo, useState, type ReactNode } from "react";
import { appsApi, SERVICE_NAME, type Build, type Environment, type Prices, type Reset, type Run, type Service, type ServicePatch, type Source } from "@/lib/apps-api";
import { Field, Segmented } from "../redis/shared";
import { btn, Confirm, danger, input, Panel, primary } from "../ui";
import { ActivityPanel } from "./activity";
import { DrainsPanel } from "./drains";
import { EnvironmentPanel } from "./environment";
import { NotificationsPanel } from "./notifications";
import { SpendPanel } from "./spend";
import { resourceKey } from "@/lib/workspace-resources";
import { useWorkspace } from "@/lib/workspace-store";
import { CPU, MEMORY, mb, type Runner } from "./shared";


/** The form's values, as text (empty: the default). */
interface Form {
  name: string;
  sourceKind: Source["kind"];
  repo: string;
  root: string;
  image: string;
  builder: Build["builder"];
  dockerfile: string;
  /** The Dockerfile's stage to build. */
  target: string;
  install: string;
  build: string;
  buildStart: string;
  /** Watch paths, comma-separated. */
  watch: string;
  type: Run["type"];
  port: string;
  start: string;
  preDeploy: string;
  schedule: string;
  timezone: string;
  timeout: string;
  min: string;
  max: string;
  concurrency: string;
  cpu: number;
  memory: number;
  healthPath: string;
  healthTimeout: string;
  drain: string;
  cpuTarget: string;
  alertMemory: string;
  alertErrors: string;
  alertP95: string;
  alertLog: string;
  alertLogCount: string;
  rolloutSteps: string;
  rolloutStep: string;
  rolloutErrors: string;
  h2c: boolean;
  public: boolean;
  restart: "always" | "on_failure" | "never";
  restartTries: string;
  affinity: boolean;
  tcp: boolean;
  rollbackOnCrash: boolean;
  scaleStream: string;
  scaleGroup: string;
  scalePer: string;
  suspendAfter: string;
  sleepAfter: string;
  volumeMount: string;
  volumeSize: string;
}

const str = (v: string | number | undefined | null) => (v == null ? "" : String(v));

export function formOf(s: Service): Form {
  return {
    name: s.name,
    sourceKind: s.source.kind,
    repo: s.source.kind === "repo" ? s.source.repo : "",
    root: s.source.kind === "repo" ? str(s.source.root) : "",
    image: s.source.kind === "image" ? s.source.image : "",
    builder: s.build.builder,
    dockerfile: str(s.build.dockerfile),
    target: str(s.build.target),
    install: str(s.build.install),
    build: str(s.build.build),
    buildStart: str(s.build.start),
    watch: (s.build.watch ?? []).join(", "),
    type: s.run.type,
    port: str(s.run.port),
    start: str(s.run.start),
    preDeploy: str(s.run.pre_deploy),
    schedule: str(s.run.schedule),
    timezone: str(s.run.timezone),
    timeout: str(s.run.timeout_s),
    min: str(s.run.min_instances),
    max: str(s.run.max_instances),
    concurrency: str(s.run.concurrency),
    cpu: s.run.cpu,
    memory: s.run.memory_mb,
    healthPath: str(s.run.health_path),
    healthTimeout: str(s.run.health_timeout_s),
    drain: str(s.run.drain_s),
    cpuTarget: str(s.run.cpu_target),
    alertMemory: str(s.run.alerts?.memory_pct),
    alertErrors: str(s.run.alerts?.error_pct),
    alertP95: str(s.run.alerts?.p95_ms),
    alertLog: s.run.alerts?.log_match ?? "",
    alertLogCount: str(s.run.alerts?.log_count ?? 10),
    rolloutSteps: (s.run.rollout?.steps ?? []).join(", "),
    rolloutStep: str(s.run.rollout?.step_s),
    rolloutErrors: str(s.run.rollout?.error_pct),
    h2c: !!s.run.h2c,
    public: s.run.public !== false,
    restart: s.run.restart ?? "always",
    restartTries: str(s.run.restart_tries),
    affinity: !!s.run.affinity,
    tcp: !!s.run.tcp,
    rollbackOnCrash: !!s.run.rollback_on_crash,
    scaleStream: s.run.scale?.stream ?? "",
    scaleGroup: s.run.scale?.group ?? "",
    scalePer: str(s.run.scale?.per_instance),
    suspendAfter: str(s.run.suspend_after_s),
    sleepAfter: str(s.run.sleep_after_s),
    volumeMount: s.run.volume?.mount ?? "",
    volumeSize: str(s.run.volume?.size_gb),
  };
}

/** What changed between the service and the form, as a PATCH body (a cleared field is `null`: its default). */
export function patchOf(s: Service, f: Form): ServicePatch {
  const before = formOf(s);
  const patch: ServicePatch = {};
  const text = (v: string) => (v.trim() ? v.trim() : null);
  const num = (v: string) => (v.trim() ? Number(v) : null);
  if (f.name !== before.name) patch.name = f.name;
  if (f.sourceKind !== before.sourceKind || f.repo !== before.repo || f.root !== before.root || f.image !== before.image)
    patch.source =
      f.sourceKind === "repo"
        ? { kind: "repo", repo: f.repo.trim(), ...(f.root.trim() ? { root: f.root.trim() } : {}) }
        : f.sourceKind === "image"
          ? { kind: "image", image: f.image.trim() }
          : { kind: "upload" };
  const build: Reset<Build> = {};
  if (f.builder !== before.builder) build.builder = f.builder;
  if (f.dockerfile !== before.dockerfile) build.dockerfile = text(f.dockerfile);
  if (f.target !== before.target) build.target = text(f.target);
  if (f.install !== before.install) build.install = text(f.install);
  if (f.build !== before.build) build.build = text(f.build);
  if (f.buildStart !== before.buildStart) build.start = text(f.buildStart);
  if (f.watch !== before.watch) {
    const list = f.watch.split(/[,\n]/).map((p) => p.trim()).filter(Boolean);
    build.watch = list.length ? list : null;
  }
  if (Object.keys(build).length) patch.build = build;
  const run: Reset<Run> = {};
  if (f.type !== before.type) run.type = f.type;
  if (f.port !== before.port) run.port = num(f.port);
  if (f.start !== before.start) run.start = text(f.start);
  if (f.preDeploy !== before.preDeploy) run.pre_deploy = text(f.preDeploy);
  if (f.schedule !== before.schedule) run.schedule = text(f.schedule);
  if (f.timezone !== before.timezone) run.timezone = text(f.timezone);
  if (f.timeout !== before.timeout) run.timeout_s = num(f.timeout);
  if (f.min !== before.min) run.min_instances = num(f.min);
  if (f.max !== before.max) run.max_instances = num(f.max);
  if (f.concurrency !== before.concurrency) run.concurrency = num(f.concurrency);
  if (f.cpu !== before.cpu) run.cpu = f.cpu;
  if (f.memory !== before.memory) run.memory_mb = f.memory;
  if (f.healthPath !== before.healthPath) run.health_path = text(f.healthPath);
  if (f.healthTimeout !== before.healthTimeout) run.health_timeout_s = num(f.healthTimeout);
  if (f.drain !== before.drain) run.drain_s = num(f.drain);
  if (f.cpuTarget !== before.cpuTarget) run.cpu_target = num(f.cpuTarget);
  if (f.alertMemory !== before.alertMemory || f.alertErrors !== before.alertErrors || f.alertP95 !== before.alertP95 || f.alertLog !== before.alertLog || f.alertLogCount !== before.alertLogCount)
    run.alerts = {
      memory_pct: Number(f.alertMemory) || 0,
      error_pct: Number(f.alertErrors) || 0,
      p95_ms: Number(f.alertP95) || 0,
      ...(f.alertLog.trim() ? { log_match: f.alertLog.trim() } : {}),
      log_count: Number(f.alertLogCount) || 10,
    };
  if (f.rolloutSteps !== before.rolloutSteps || f.rolloutStep !== before.rolloutStep || f.rolloutErrors !== before.rolloutErrors)
    run.rollout = { steps: rolloutSteps(f.rolloutSteps) ?? [], step_s: Number(f.rolloutStep) || 120, error_pct: Number(f.rolloutErrors) || 5 };
  if (f.h2c !== before.h2c) run.h2c = f.h2c ? true : null;
  if (f.public !== before.public) run.public = f.public ? null : false;
  if (f.affinity !== before.affinity) run.affinity = f.affinity ? true : null;
  if (f.tcp !== before.tcp) run.tcp = f.tcp ? true : null;
  if (f.rollbackOnCrash !== before.rollbackOnCrash) run.rollback_on_crash = f.rollbackOnCrash ? true : null;
  if (f.restart !== before.restart || f.restartTries !== before.restartTries) {
    run.restart = f.restart === "always" ? null : f.restart;
    run.restart_tries = f.restart === "on_failure" ? num(f.restartTries) : null;
  }
  if (f.scaleStream !== before.scaleStream || f.scaleGroup !== before.scaleGroup || f.scalePer !== before.scalePer)
    run.scale = f.scaleStream && f.scaleGroup.trim() ? { stream: f.scaleStream, group: f.scaleGroup.trim(), ...(f.scalePer.trim() ? { per_instance: Number(f.scalePer) } : {}) } : null;
  if (f.suspendAfter !== before.suspendAfter) run.suspend_after_s = num(f.suspendAfter);
  if (f.sleepAfter !== before.sleepAfter) run.sleep_after_s = num(f.sleepAfter);
  if (f.volumeMount !== before.volumeMount || f.volumeSize !== before.volumeSize)
    run.volume = f.volumeMount.trim() ? { mount: f.volumeMount.trim(), size_gb: Number(f.volumeSize) || 1 } : null;
  if (Object.keys(run).length) patch.run = run;
  return patch;
}

const NUMERIC: (keyof Form)[] = ["scalePer", "port", "timeout", "min", "max", "concurrency", "healthTimeout", "drain", "cpuTarget", "suspendAfter", "sleepAfter", "volumeSize", "alertMemory", "alertErrors", "alertP95", "alertLogCount", "rolloutStep", "rolloutErrors", "restartTries"];

/** A rollout's steps as typed (`10, 50`): increasing percents 1–99, at most five; null when they aren't. */
export function rolloutSteps(v: string): number[] | null {
  const parts = v.split(/[,\s%]+/).filter(Boolean);
  const steps = parts.map(Number);
  const ok = steps.length <= 5 && steps.every((n, i) => Number.isInteger(n) && n >= 1 && n <= 99 && (i === 0 || n > steps[i - 1]!));
  return ok ? steps : null;
}

/** Source, build and run settings, saved together; deleting the service at the bottom. */
export function SettingsTab({
  service,
  env,
  projectName,
  runner,
  onDeleted,
  onEnvDeleted,
  prices,
}: {
  service: Service;
  env: Environment | null;
  /** roll's prices, for the estimate under the size. */
  prices?: Prices | null;
  projectName: string;
  runner: Runner;
  onDeleted: () => void;
  onEnvDeleted: () => void;
}) {
  const [form, setForm] = useState<Form>(() => formOf(service));
  const [deleting, setDeleting] = useState(false);
  // A save (or someone else's) brings the form back to what rolld has; polls that change nothing don't.
  const saved = JSON.stringify([service.name, service.source, service.build, service.run]);
  useEffect(() => setForm(formOf(service)), [saved]); // eslint-disable-line react-hooks/exhaustive-deps
  const patch = useMemo(() => patchOf(service, form), [service, form]);
  const dirty = Object.keys(patch).length > 0;
  const set = <K extends keyof Form>(k: K, v: Form[K]) => setForm((f) => ({ ...f, [k]: NUMERIC.includes(k) && typeof v === "string" ? v.replace(/[^\d.]/g, "") : v }));
  const t = (k: keyof Form, placeholder?: string, mono = true) => (
    <input value={form[k] as string} onChange={(e) => set(k, e.target.value)} placeholder={placeholder} className={`${input} w-full ${mono ? "font-mono text-[12px]" : ""}`} spellCheck={false} />
  );
  const web = form.type === "web";
  // A worker can follow a stream of its own project.
  const ws = useWorkspace();
  const streams = (ws.projects.find((p) => p.id === service.project)?.resources ?? [])
    .filter((r) => r.kind === "stream")
    .map((r) => ({ ref: r.ref, name: ws.inventory.get(resourceKey(r))?.name ?? r.ref }));

  return (
    <div className="space-y-6">
      <Panel title="Service">
        <Grid>
          <Field label="Name" hint={SERVICE_NAME.test(form.name) ? "part of its URL" : "1–32 of a–z, 0–9 and -"}>
            {t("name")}
          </Field>
          <Field label="Source">
            <Segmented
              value={form.sourceKind}
              options={[
                { label: "repo", value: "repo" },
                { label: "image", value: "image" },
                { label: "upload", value: "upload" },
              ]}
              onChange={(v) => set("sourceKind", v)}
            />
          </Field>
          {form.sourceKind === "repo" && (
            <>
              <Field label="Repository" hint="owner/name in roll code">
                {t("repo", "acme/site")}
              </Field>
              <Field label="Root directory" hint="optional">
                {t("root", "apps/web")}
              </Field>
            </>
          )}
          {form.sourceKind === "image" && (
            <Field label="Image" hint="its digest is pinned per deploy">
              {t("image", "docker.io/library/nginx:1.27")}
            </Field>
          )}
          {form.sourceKind === "upload" && <p className="self-end text-[12px] text-subtle">Deploys come from `roll up`.</p>}
        </Grid>
        {service.source.kind === "image" && <RegistryLogin service={service} runner={runner} />}
      </Panel>

      {form.sourceKind !== "image" && (
        <Panel title="Build">
          <Grid>
            <Field label="Builder" hint={form.builder === "auto" ? "detect; a Dockerfile wins" : "use the Dockerfile below"}>
              <Segmented
                value={form.builder}
                options={[
                  { label: "auto", value: "auto" },
                  { label: "dockerfile", value: "dockerfile" },
                ]}
                onChange={(v) => set("builder", v)}
              />
            </Field>
            {form.builder === "dockerfile" && <Field label="Dockerfile">{t("dockerfile", "Dockerfile")}</Field>}
            {form.builder === "dockerfile" && (
              <Field label="Stage" hint="a multi-stage Dockerfile's stage to build (--target); empty: its last">
                {t("target", "its last stage")}
              </Field>
            )}
            <Field label="Install command" hint="overrides the detected one">
              {t("install", "detected")}
            </Field>
            <Field label="Build command">{t("build", "detected")}</Field>
            <Field label="Start command" hint="baked into the image">
              {t("buildStart", "detected")}
            </Field>
            {form.sourceKind === "repo" && (
              <Field label="Watch paths" hint="a push deploys only when it changes one">
                {t("watch", "apps/web/**, packages/ui/**")}
              </Field>
            )}
          </Grid>
          {service.source.kind === "repo" && env && (
            <div className="flex flex-wrap items-center justify-between gap-3 border-t border-line px-5 py-4">
              <p className="text-[13px] text-muted">For a build a stale cached layer keeps breaking: no cached layers, base images pulled again, the dependency caches empty.</p>
              <button type="button" className={btn} disabled={runner.busy !== null} onClick={() => void runner.run("deploy", () => appsApi.deploy(service.id, env.name, { no_cache: true }))}>
                Deploy without the cache
              </button>
            </div>
          )}
        </Panel>
      )}

      <Panel title="Run">
        <Grid>
          <Field label="Type" hint={web ? "serves HTTP, scales to zero" : form.type === "worker" ? "always running" : "runs on a schedule"}>
            <Segmented
              value={form.type}
              options={[
                { label: "web", value: "web" },
                { label: "worker", value: "worker" },
                { label: "cron", value: "cron" },
              ]}
              onChange={(v) => set("type", v)}
            />
          </Field>
          {web && (
            <Field label="Port" hint="PORT is set to it">
              {t("port", "from the plan, else 8080")}
            </Field>
          )}
          <Field label="Start command" hint="overrides the image's command">
            {t("start", "the image's")}
          </Field>
          {form.type !== "cron" && (
            <Field label="Pre-deploy command" hint="runs before each switch: a failure stops the deploy">
              {t("preDeploy", "npx prisma migrate deploy")}
            </Field>
          )}
          {form.type === "cron" && (
            <>
              <Field label="Schedule" hint="five fields, in its time zone">
                {t("schedule", "*/15 * * * *")}
              </Field>
              <Field label="Time zone" hint="IANA, its daylight saving kept; empty: UTC">
                {t("timezone", "Europe/Budapest")}
              </Field>
              <Field label="Timeout (s)" hint="a run's limit">
                {t("timeout", "3600")}
              </Field>
            </>
          )}
          {form.type !== "cron" && (
            <>
              <Field label="Min instances" hint={web ? "0 scales to zero" : "at least 1"}>
                {t("min", "0")}
              </Field>
              <Field label="Max instances" hint={form.volumeMount ? "1 with a volume" : undefined}>
                {t("max", "3")}
              </Field>
            </>
          )}
          {web && (
            <Field label="Concurrency" hint="requests in flight per instance">
              {t("concurrency", "50")}
            </Field>
          )}
          <Field label="CPU" hint="vCPU per instance">
            <select value={form.cpu} onChange={(e) => set("cpu", Number(e.target.value))} className={`${input} w-full`}>
              {(CPU.includes(form.cpu) ? CPU : [...CPU, form.cpu].sort((a, b) => a - b)).map((c) => (
                <option key={c} value={c}>
                  {c} vCPU
                </option>
              ))}
            </select>
          </Field>
          <Field label="Memory" hint="per instance">
            <select value={form.memory} onChange={(e) => set("memory", Number(e.target.value))} className={`${input} w-full`}>
              {(MEMORY.includes(form.memory) ? MEMORY : [...MEMORY, form.memory].sort((a, b) => a - b)).map((m) => (
                <option key={m} value={m}>
                  {mb(m)}
                </option>
              ))}
            </select>
          </Field>
          {prices && <Estimate prices={prices} cpu={form.cpu} memoryMb={form.memory} min={form.type === "cron" ? 0 : Number(form.min) || 0} />}
          {web && (
            <>
              <Field label="Health check path" hint="checked at each deploy and every 10 s while running; none: the port accepting">
                {t("healthPath", "/healthz")}
              </Field>
              <Field label="Health timeout (s)">{t("healthTimeout", "60")}</Field>
              <Field label="Request timeout (s)" hint="a request unanswered by then is a 504; empty: no limit">
                {t("timeout", "300")}
              </Field>
              <Field label="Drain (s)" hint="a replaced instance finishes its requests, then gets SIGTERM; killed when it's up">
                {t("drain", "30")}
              </Field>
              <Field label="CPU target (%)" hint="another instance while they average over it for a minute; empty: requests only">
                {t("cpuTarget", "70")}
              </Field>
              <Field label="Protocol" hint="TCP: a port of its own per environment, no HTTP names (a game server, MQTT)">
                <select value={form.tcp ? "tcp" : "http"} onChange={(e) => set("tcp", e.target.value === "tcp")} className={`${input} w-full`}>
                  <option value="http">HTTP</option>
                  <option value="tcp">TCP</option>
                </select>
              </Field>
              <Field label="Session affinity" hint="each visitor stays on one instance while it's up (a cookie)">
                <select value={form.affinity ? "on" : "off"} onChange={(e) => set("affinity", e.target.value === "on")} className={`${input} w-full`}>
                  <option value="off">Off</option>
                  <option value="on">On</option>
                </select>
              </Field>
              <Field label="Suspend after (s)" hint="idle; 0 never suspends">
                {t("suspendAfter", "30")}
              </Field>
              <Field label="Sleep after (s)" hint="idle; stopped costs nothing">
                {t("sleepAfter", "600")}
              </Field>
              <Field label="Alert: 5xx share (%)" hint="of five minutes' requests; 0 off">
                {t("alertErrors", "5")}
              </Field>
              <Field label="Alert: p95 latency (ms)" hint="in three of five minutes; 0 off">
                {t("alertP95", "0")}
              </Field>
              {!form.volumeMount.trim() && (
                <Field label="Gradual rollout (%)" hint={rolloutSteps(form.rolloutSteps) ? "shares of requests a new deployment takes in turn; empty: at once" : "increasing percents, 1–99, up to five"}>
                  {t("rolloutSteps", "10, 50")}
                </Field>
              )}
              {!form.volumeMount.trim() && form.rolloutSteps.trim() && (
                <>
                  <Field label="Each step (s)">{t("rolloutStep", "120")}</Field>
                  <Field label="Roll back at 5xx (%)" hint="of its requests in a step, and more than the old one">
                    {t("rolloutErrors", "5")}
                  </Field>
                </>
              )}
            </>
          )}
          <Field label="Alert: memory (%)" hint="of its limit, five minutes; 0 off">
            {t("alertMemory", "90")}
          </Field>
          <Field label="A crash loop after a deploy" hint="within ten minutes of going live">
            <select value={form.rollbackOnCrash ? "back" : "stay"} onChange={(e) => set("rollbackOnCrash", e.target.value === "back")} className={`${input} w-full`}>
              <option value="stay">Stays (an alert)</option>
              <option value="back">Rolls back to the one before</option>
            </select>
          </Field>
          <Field label="Alert: its log says" hint="text in its output (any case); empty: off">
            {t("alertLog", "panic")}
          </Field>
          {form.alertLog.trim() && (
            <Field label="Times in five minutes" hint="how often before it's an alert">
              {t("alertLogCount", "10")}
            </Field>
          )}
          {web && (
            <Field label="Reachable" hint={form.public ? "its URL and custom domains answer anyone" : "no public URL: only this project's services reach it"}>
              <select value={form.public ? "public" : "private"} onChange={(e) => set("public", e.target.value === "public")} className={`${input} w-full`}>
                <option value="public">Public URL</option>
                <option value="private">Private network only</option>
              </select>
            </Field>
          )}
          {web && (
            <Field label="Protocol" hint="h2c: HTTP/2 without TLS, as a gRPC server speaks">
              <select value={form.h2c ? "h2c" : "http1"} onChange={(e) => set("h2c", e.target.value === "h2c")} className={`${input} w-full`}>
                <option value="http1">HTTP/1.1</option>
                <option value="h2c">HTTP/2 (h2c)</option>
              </select>
            </Field>
          )}
          {form.type === "worker" && (
            <>
              <Field label="Restart" hint="when it stops; a deploy or a restart starts fresh ones whatever it says">
                <select value={form.restart} onChange={(e) => set("restart", e.target.value as Form["restart"])} className={`${input} w-full`}>
                  <option value="always">Always</option>
                  <option value="on_failure">On failure</option>
                  <option value="never">Never</option>
                </select>
              </Field>
              {form.restart === "on_failure" && (
                <Field label="Tries" hint="failures in a row before it's left stopped">
                  {t("restartTries", "10")}
                </Field>
              )}
              <Field label="Follow a stream" hint={streams.length ? "instances follow a consumer group's lag" : "a stream on this project can drive it"}>
                <select value={form.scaleStream} onChange={(e) => set("scaleStream", e.target.value)} className={`${input} w-full`} disabled={!streams.length && !form.scaleStream}>
                  <option value="">No: run min instances</option>
                  {streams.map((s) => (
                    <option key={s.ref} value={s.ref}>
                      {s.name}
                    </option>
                  ))}
                </select>
              </Field>
              {form.scaleStream && (
                <>
                  <Field label="Consumer group">{t("scaleGroup", "billing")}</Field>
                  <Field label="Messages per instance" hint="behind, before another starts">
                    {t("scalePer", "1000")}
                  </Field>
                </>
              )}
            </>
          )}
          <Field label="Volume mount" hint="optional; one instance at most">
            {t("volumeMount", "/data")}
          </Field>
          {form.volumeMount.trim() && (
            <Field label="Volume size (GB)" hint="what it's billed and planned at; a host without disk quotas doesn't stop it growing past">
              {t("volumeSize", "1")}
            </Field>
          )}
        </Grid>
      </Panel>

      <div className={`sticky bottom-4 z-10 flex items-center justify-end gap-2 transition-opacity duration-150 ${dirty ? "opacity-100" : "pointer-events-none opacity-0"}`}>
        <span className="mr-auto text-[12px] text-muted">Changes apply with the next deploy.</span>
        <button className={btn} onClick={() => setForm(formOf(service))}>
          Discard
        </button>
        <button
          className={primary}
          disabled={!dirty || !SERVICE_NAME.test(form.name) || rolloutSteps(form.rolloutSteps) === null || runner.busy !== null}
          onClick={() => runner.run("settings", () => appsApi.update(service.id, patch))}
        >
          {runner.busy === "settings" ? <span className="shimmer">Saving…</span> : "Save"}
        </button>
      </div>

      {env && <EnvironmentPanel key={env.name} service={service} env={env} runner={runner} onDeleted={onEnvDeleted} />}

      <NotificationsPanel project={service.project} projectName={projectName} runner={runner} />

      <DrainsPanel project={service.project} projectName={projectName} runner={runner} />

      <SpendPanel project={service.project} projectName={projectName} runner={runner} />

      <ActivityPanel service={service.id} />

      <Panel title="Danger zone">
        <div className="p-5">
          {deleting ? (
            <Confirm
              word={service.name}
              action="Delete service"
              busy={runner.busy === "delete"}
              onCancel={() => setDeleting(false)}
              onConfirm={async () => {
                if (await runner.run("delete", () => appsApi.remove(service.id).then(() => true))) onDeleted();
              }}
            >
              Deletes every environment, instance, deployment and image of <span className="font-mono text-fg">{service.name}</span>. Its URLs stop answering.
            </Confirm>
          ) : (
            <div className="flex flex-wrap items-center justify-between gap-3">
              <p className="text-[13px] text-muted">Delete the service, its environments, deployments and images.</p>
              <button className={danger} onClick={() => setDeleting(true)}>
                <Trash2 className="size-3.5" /> Delete
              </button>
            </div>
          )}
        </div>
      </Panel>
    </div>
  );
}

/** What the size costs: an instance's hour at its limits (CPU is metered as used, so this is
 *  the most), and the month the minimum kept up costs. */
function Estimate({ prices, cpu, memoryMb, min }: { prices: Prices; cpu: number; memoryMb: number; min: number }) {
  const hour = (cpu * prices.cpu_second + (memoryMb / 1024) * prices.gb_second) * 3600;
  const month = hour * 730 * min;
  const usd = (v: number) => (v < 0.01 ? `$${v.toFixed(4)}` : `$${v.toFixed(2)}`);
  return (
    <div className="flex flex-col justify-end gap-1 text-[11px] text-subtle sm:col-span-2 lg:col-span-1">
      <span>
        At most <span className="font-mono text-muted">{usd(hour)}</span> an hour per running instance
      </span>
      <span>
        {min > 0 ? (
          <>
            {min} kept up: about <span className="font-mono text-muted">{usd(month)}</span> a month, before traffic
          </>
        ) : (
          "Scales to zero: nothing while it's asleep"
        )}
      </span>
    </div>
  );
}

const Grid = ({ children }: { children: ReactNode }) => <div className="grid gap-x-3 gap-y-4 p-5 sm:grid-cols-2 lg:grid-cols-3">{children}</div>;

/** A private registry's login for the image: write-only (the password never comes back). */
function RegistryLogin({ service, runner }: { service: Service; runner: Runner }) {
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const r = service.registry;
  return (
    <div className="border-t border-line px-5 py-4">
      <p className="text-[13px] font-medium">Private registry</p>
      {r ? (
        <div className="mt-2 flex flex-wrap items-center gap-3 text-[13px] text-muted">
          <span>
            Pulls as <span className="font-mono text-fg">{r.username}</span> at <span className="font-mono text-fg">{r.server}</span>
          </span>
          <button type="button" className={btn} disabled={runner.busy !== null} onClick={() => void runner.run("registry", () => appsApi.deleteRegistry(service.id))}>
            Remove
          </button>
        </div>
      ) : (
        <form
          className="mt-2 flex flex-wrap items-end gap-2"
          onSubmit={(e) => {
            e.preventDefault();
            void runner.run("registry", () => appsApi.putRegistry(service.id, { username, password })).then((ok) => {
              if (ok) {
                setUsername("");
                setPassword("");
              }
            });
          }}
        >
          <input value={username} onChange={(e) => setUsername(e.target.value)} placeholder="username" aria-label="Registry username" autoComplete="off" spellCheck={false} className={`${input} w-44 font-mono text-[12px]`} />
          <input value={password} onChange={(e) => setPassword(e.target.value)} type="password" placeholder="password or token" aria-label="Registry password" autoComplete="new-password" className={`${input} w-56 font-mono text-[12px]`} />
          <button type="submit" className={primary} disabled={!username || !password || runner.busy !== null}>
            Save login
          </button>
          <span className="text-[12px] text-subtle">Sealed; only pulls use it.</span>
        </form>
      )}
    </div>
  );
}
