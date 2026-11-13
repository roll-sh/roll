import { Trash2 } from "lucide-react";
import { useEffect, useMemo, useState, type ReactNode } from "react";
import { appsApi, SERVICE_NAME, type Build, type Reset, type Run, type Service, type ServicePatch, type Source } from "@/lib/apps-api";
import { Field, Segmented } from "../redis/shared";
import { btn, Confirm, danger, input, Panel, primary } from "../ui";
import { NotificationsPanel } from "./notifications";
import type { Runner } from "./shared";

const CPU = [0.25, 0.5, 1, 2, 4, 8];
const MEMORY = [256, 512, 1024, 2048, 4096, 8192, 16384];
const mb = (n: number) => (n >= 1024 ? `${n / 1024} GB` : `${n} MB`);

/** The form's values, as text (empty: the default). */
interface Form {
  name: string;
  sourceKind: Source["kind"];
  repo: string;
  root: string;
  image: string;
  builder: Build["builder"];
  dockerfile: string;
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
  timeout: string;
  min: string;
  max: string;
  concurrency: string;
  cpu: number;
  memory: number;
  healthPath: string;
  healthTimeout: string;
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
    install: str(s.build.install),
    build: str(s.build.build),
    buildStart: str(s.build.start),
    watch: (s.build.watch ?? []).join(", "),
    type: s.run.type,
    port: str(s.run.port),
    start: str(s.run.start),
    preDeploy: str(s.run.pre_deploy),
    schedule: str(s.run.schedule),
    timeout: str(s.run.timeout_s),
    min: str(s.run.min_instances),
    max: str(s.run.max_instances),
    concurrency: str(s.run.concurrency),
    cpu: s.run.cpu,
    memory: s.run.memory_mb,
    healthPath: str(s.run.health_path),
    healthTimeout: str(s.run.health_timeout_s),
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
  if (f.timeout !== before.timeout) run.timeout_s = num(f.timeout);
  if (f.min !== before.min) run.min_instances = num(f.min);
  if (f.max !== before.max) run.max_instances = num(f.max);
  if (f.concurrency !== before.concurrency) run.concurrency = num(f.concurrency);
  if (f.cpu !== before.cpu) run.cpu = f.cpu;
  if (f.memory !== before.memory) run.memory_mb = f.memory;
  if (f.healthPath !== before.healthPath) run.health_path = text(f.healthPath);
  if (f.healthTimeout !== before.healthTimeout) run.health_timeout_s = num(f.healthTimeout);
  if (f.suspendAfter !== before.suspendAfter) run.suspend_after_s = num(f.suspendAfter);
  if (f.sleepAfter !== before.sleepAfter) run.sleep_after_s = num(f.sleepAfter);
  if (f.volumeMount !== before.volumeMount || f.volumeSize !== before.volumeSize)
    run.volume = f.volumeMount.trim() ? { mount: f.volumeMount.trim(), size_gb: Number(f.volumeSize) || 1 } : null;
  if (Object.keys(run).length) patch.run = run;
  return patch;
}

const NUMERIC: (keyof Form)[] = ["port", "timeout", "min", "max", "concurrency", "healthTimeout", "suspendAfter", "sleepAfter", "volumeSize"];

/** Source, build and run settings, saved together; deleting the service at the bottom. */
export function SettingsTab({ service, projectName, runner, onDeleted }: { service: Service; projectName: string; runner: Runner; onDeleted: () => void }) {
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
              <Field label="Schedule" hint="five fields, UTC">
                {t("schedule", "*/15 * * * *")}
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
          {web && (
            <>
              <Field label="Health check path" hint="none: the port accepting">
                {t("healthPath", "/healthz")}
              </Field>
              <Field label="Health timeout (s)">{t("healthTimeout", "60")}</Field>
              <Field label="Suspend after (s)" hint="idle; 0 never suspends">
                {t("suspendAfter", "30")}
              </Field>
              <Field label="Sleep after (s)" hint="idle; stopped costs nothing">
                {t("sleepAfter", "600")}
              </Field>
            </>
          )}
          <Field label="Volume mount" hint="optional; one instance at most">
            {t("volumeMount", "/data")}
          </Field>
          {form.volumeMount.trim() && <Field label="Volume size (GB)">{t("volumeSize", "1")}</Field>}
        </Grid>
      </Panel>

      <div className={`sticky bottom-4 z-10 flex items-center justify-end gap-2 transition-opacity duration-150 ${dirty ? "opacity-100" : "pointer-events-none opacity-0"}`}>
        <span className="mr-auto text-[12px] text-muted">Changes apply with the next deploy.</span>
        <button className={btn} onClick={() => setForm(formOf(service))}>
          Discard
        </button>
        <button
          className={primary}
          disabled={!dirty || !SERVICE_NAME.test(form.name) || runner.busy !== null}
          onClick={() => runner.run("settings", () => appsApi.update(service.id, patch))}
        >
          {runner.busy === "settings" ? <span className="shimmer">Saving…</span> : "Save"}
        </button>
      </div>

      <NotificationsPanel project={service.project} projectName={projectName} runner={runner} />

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
