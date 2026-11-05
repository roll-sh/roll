"use client";

/**
 * Upstreams (docs/code/gateway.md, "Upstreams"): the editor (kind presets, the fields each kind
 * needs, its key) and, in the Gateway section, each upstream with its health (circuit, cooling,
 * last error, p50, requests per hour), priority, on/off and key. A key typed here is sealed to the
 * gateway's computer in this browser (`gateway.key`, `roll-gateway-upstream:<id>`) and emptied once
 * sent; a set key only shows as "Key set (never shown)". API keys and cloud credentials only:
 * never a Claude.ai or ChatGPT login.
 */

import { ArrowDown, ArrowUp, Check, KeyRound, Pencil, RefreshCw, Trash2, X } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { UPSTREAM_KINDS, type KeyState, type UpstreamCheck, type UpstreamHealth, type UpstreamKind, type UpstreamSpec } from "@/lib/agents-api";
import { checkUpstream, KIND_PRESETS, upstreamId, upstreamUrl } from "@/lib/agents-gateway";
import { useEscapeLayer } from "../escape-layers";
import { field, Note } from "../settings-ui";
import { ago } from "../shared";
import { cn, Switch } from "../ui";
import { dangerBtn, iconBtn, KIND_ICON, KindTile, Labeled, message, primaryBtn, Problem, textBtn } from "./shared";

export interface UpstreamResult {
  upstream: UpstreamSpec;
  /** A typed key (sealed by the caller at once), `null` to clear it, `undefined` to keep it. */
  key: string | null | undefined;
}

/** A key field: a password input (a textarea for a service account's JSON) browsers don't fill or keep. */
function KeyInput({ label, value, onChange, placeholder, multiline }: { label: string; value: string; onChange: (v: string) => void; placeholder?: string; multiline?: boolean }) {
  const common = { "aria-label": label, value, placeholder, spellCheck: false, autoComplete: "off", "data-1p-ignore": true, "data-lpignore": "true" } as const;
  return multiline ? (
    <textarea {...common} rows={3} onChange={(e) => onChange(e.target.value)} className={cn(field, "h-auto w-full resize-y py-1.5 font-mono text-[11.5px] [-webkit-text-security:disc]")} />
  ) : (
    <input {...common} type="password" onChange={(e) => onChange(e.target.value)} className={cn(field, "w-full font-mono text-[12px]")} />
  );
}

export function UpstreamEditor({
  initial,
  keyState,
  taken,
  onSave,
  onCancel,
  replaceKey = false,
}: {
  initial?: UpstreamSpec;
  keyState?: KeyState;
  /** Ids in use (a new one is made from the name). */
  taken: readonly string[];
  onSave: (r: UpstreamResult) => Promise<string | null>;
  onCancel: () => void;
  replaceKey?: boolean;
}) {
  const [kind, setKind] = useState<UpstreamKind>(initial?.kind ?? "anthropic");
  const preset = KIND_PRESETS[kind];
  const [name, setName] = useState(initial?.name ?? "");
  const [url, setUrl] = useState(initial?.base_url ?? "");
  const [region, setRegion] = useState(initial?.region ?? "");
  const [project, setProject] = useState(initial?.project ?? "");
  const [models, setModels] = useState((initial?.models ?? []).join(", "));
  const [responses, setResponses] = useState(!!initial?.responses);
  const [priv, setPriv] = useState(!!initial?.allow_private);
  const [headers, setHeaders] = useState((initial?.headers ?? []).map(([k, v]) => `${k}: ${v}`).join("\n"));
  const [fromOp, setFromOp] = useState(!!initial?.key_ref);
  const [ref, setRef] = useState(initial?.key_ref ?? "");
  const sealed = keyState === "sealed";
  const [typing, setTyping] = useState(!sealed || replaceKey);
  const [clearKey, setClearKey] = useState(false);
  const [key, setKey] = useState("");
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const box = useRef<HTMLFormElement>(null);
  useEscapeLayer(true, onCancel);
  useEffect(() => {
    box.current?.scrollIntoView?.({ block: "nearest", behavior: "smooth" });
  }, []);

  const save = async () => {
    const parsedHeaders: [string, string][] = [];
    for (const line of headers.split("\n").map((l) => l.trim()).filter(Boolean)) {
      const at = line.indexOf(":");
      if (at <= 0) return setProblem(`${JSON.stringify(line)}: write a header as name: value`);
      parsedHeaders.push([line.slice(0, at).trim(), line.slice(at + 1).trim()]);
    }
    const spec: UpstreamSpec = {
      id: initial?.id ?? upstreamId(name || preset.label, taken),
      name: name.trim() || preset.label,
      kind,
      ...(url.trim() ? { base_url: url } : {}),
      ...(preset.region && region.trim() ? { region } : {}),
      ...(preset.project && project.trim() ? { project } : {}),
      ...(models.trim() ? { models: models.split(/[\s,]+/).filter(Boolean) } : {}),
      ...(kind === "openai_compatible" && responses ? { responses: true } : {}),
      ...(priv ? { allow_private: true } : {}),
      ...(parsedHeaders.length ? { headers: parsedHeaders } : {}),
      ...(fromOp ? { key_ref: ref } : {}),
      ...(initial?.disabled ? { disabled: true } : {}),
    };
    if (fromOp && !ref.trim()) return setProblem("Enter the op:// reference, or turn off From 1Password.");
    const checked = checkUpstream(spec);
    if ("error" in checked) return setProblem(checked.error);
    const change = fromOp ? undefined : clearKey ? null : key.trim() ? key : undefined;
    setBusy(true);
    setProblem(null);
    const refused = await onSave({ upstream: checked.upstream, key: change });
    setBusy(false);
    // The typed key is gone from here either way: it was sealed, or it has to be typed again.
    setKey("");
    if (refused) setProblem(refused);
  };

  return (
    <form
      ref={box}
      aria-label={initial ? `Edit ${initial.name}` : "New upstream"}
      className="space-y-3.5 bg-panel-2/60 px-3.5 py-3.5"
      onSubmit={(e) => {
        e.preventDefault();
        void save();
      }}
    >
      <div className="flex items-center gap-2">
        <h4 className="flex-1 text-[13px] font-[590] tracking-[-0.01em]">{initial ? `Edit ${initial.name}` : "New upstream"}</h4>
        <button type="button" onClick={onCancel} aria-label="Cancel" className={iconBtn}>
          <X className="size-3.5" />
        </button>
      </div>
      <Labeled label="Kind" hint={`Speaks ${KIND_PRESETS[kind].speaks}.`}>
        <div role="radiogroup" aria-label="Upstream kind" className="grid grid-cols-2 gap-1.5 sm:grid-cols-4">
          {UPSTREAM_KINDS.map((k) => {
            const Icon = KIND_ICON[k];
            const on = k === kind;
            return (
              <button
                key={k}
                type="button"
                role="radio"
                aria-checked={on}
                onClick={() => {
                  setKind(k);
                  setProblem(null);
                }}
                className={cn(
                  "flex h-9 min-w-0 items-center gap-2 rounded-lg border px-2.5 text-left text-[12px] transition-colors duration-150",
                  on ? "border-accent/50 bg-accent/[0.07] text-fg" : "border-line-2 bg-raised text-muted hover:border-line-3 hover:text-fg",
                )}
              >
                <Icon className="size-3.5 shrink-0" strokeWidth={1.75} aria-hidden />
                <span className="truncate">{KIND_PRESETS[k].label}</span>
              </button>
            );
          })}
        </div>
      </Labeled>
      <div className="grid gap-3 sm:grid-cols-[minmax(0,2fr)_minmax(0,3fr)]">
        <Labeled label="Name">
          <input aria-label="Upstream name" value={name} onChange={(e) => setName(e.target.value)} placeholder={preset.label} spellCheck={false} className={cn(field, "w-full")} />
        </Labeled>
        <Labeled label={preset.needsUrl ? "Base URL" : "Base URL (optional)"}>
          <input
            aria-label="Base URL"
            value={url}
            onChange={(e) => setUrl(e.target.value)}
            placeholder={preset.url ?? upstreamUrl({ id: "", name: "", kind })}
            spellCheck={false}
            inputMode="url"
            disabled={kind === "bedrock" || kind === "vertex"}
            className={cn(field, "w-full font-mono text-[12px]")}
          />
        </Labeled>
      </div>
      {(preset.region || preset.project) && (
        <div className="grid gap-3 sm:grid-cols-2">
          {preset.region && (
            <Labeled label="Region">
              <input aria-label="Region" value={region} onChange={(e) => setRegion(e.target.value)} placeholder={preset.region} spellCheck={false} className={cn(field, "w-full font-mono text-[12px]")} />
            </Labeled>
          )}
          {preset.project && (
            <Labeled label="Project">
              <input aria-label="Project" value={project} onChange={(e) => setProject(e.target.value)} placeholder="my-project" spellCheck={false} className={cn(field, "w-full font-mono text-[12px]")} />
            </Labeled>
          )}
        </div>
      )}
      <Labeled label={preset.models ? "Models" : "Models (optional)"} hint={preset.models ? "Bedrock can't list its models: name them, exact or a prefix ending in *." : "Exact names or prefixes ending in *. Empty: whatever it lists."}>
        <input aria-label="Models" value={models} onChange={(e) => setModels(e.target.value)} placeholder={preset.models ?? "claude-*, gpt-5.5"} spellCheck={false} className={cn(field, "w-full font-mono text-[12px]")} />
      </Labeled>
      <div className="flex flex-wrap gap-x-5 gap-y-2 text-[12px] text-muted">
        {kind === "openai_compatible" && (
          <label className="flex items-center gap-2">
            <Switch size="sm" checked={responses} onCheckedChange={setResponses} aria-label="It speaks Responses" />
            It speaks Responses too
          </label>
        )}
        <label className="flex items-center gap-2" title="A model server on your own network (private, loopback or tailnet addresses). Off: such addresses are refused.">
          <Switch size="sm" checked={priv} onCheckedChange={setPriv} aria-label="Allow a private address" />
          On my network
        </label>
      </div>
      <Labeled label="Headers (optional)" hint="One per line, name: value. Sent with every request to it; never a key.">
        <textarea aria-label="Headers" value={headers} onChange={(e) => setHeaders(e.target.value)} rows={1} placeholder="openai-organization: org-…" spellCheck={false} className={cn(field, "h-auto w-full resize-y py-1.5 font-mono text-[12px]")} />
      </Labeled>
      <Labeled
        label={fromOp ? "Key from 1Password" : preset.key.label}
        hint={fromOp ? "Read on the gateway's computer with the op CLI. A reference isn't a secret." : typing && !clearKey ? `${preset.key.hint} Sealed to the gateway's computer in your browser; roll only carries it encrypted and never shows it again.` : null}
      >
        <div className="flex flex-wrap items-center gap-2">
          {fromOp ? (
            <input aria-label="1Password reference" value={ref} onChange={(e) => setRef(e.target.value)} placeholder="op://vault/item/field" spellCheck={false} className={cn(field, "min-w-48 flex-1 font-mono text-[12px]")} />
          ) : typing && !clearKey ? (
            <div className="min-w-48 flex-1">
              <KeyInput label={preset.key.label} value={key} onChange={setKey} placeholder={sealed ? "New key" : preset.key.placeholder} multiline={preset.key.multiline} />
            </div>
          ) : (
            <span className="flex min-w-0 flex-1 items-center gap-2 text-[12.5px]">
              <KeyRound className="size-3.5 shrink-0 text-subtle" aria-hidden />
              <span className={cn(clearKey && "text-subtle line-through")}>Key set (never shown)</span>
              {clearKey ? (
                <button type="button" className={textBtn} onClick={() => setClearKey(false)}>
                  Keep it
                </button>
              ) : (
                <>
                  <button type="button" className={textBtn} onClick={() => setTyping(true)}>
                    Replace
                  </button>
                  <button type="button" className={textBtn} onClick={() => setClearKey(true)}>
                    Remove
                  </button>
                </>
              )}
            </span>
          )}
          <label className="ml-auto flex shrink-0 items-center gap-2 text-[12px] text-muted">
            From 1Password
            <Switch size="sm" checked={fromOp} onCheckedChange={setFromOp} aria-label="Key from 1Password" />
          </label>
        </div>
      </Labeled>
      {problem && <Problem>{problem}</Problem>}
      <div className="flex items-center justify-end gap-2">
        <button type="button" onClick={onCancel} className="h-8 rounded-lg px-3 text-[12.5px] text-muted hover:text-fg">
          Cancel
        </button>
        <button type="submit" disabled={busy} className={primaryBtn}>
          {busy ? <span className="shimmer">Saving…</span> : initial ? "Save" : "Add upstream"}
        </button>
      </div>
    </form>
  );
}

/** "Healthy", "Circuit open until 14:02", "Cooling down (429) until …", "Trying again". */
export function healthLabel(h: UpstreamHealth | undefined, now = Date.now()): { text: string; tone: "ok" | "bad" | "quiet" } {
  if (!h) return { text: "No requests yet", tone: "quiet" };
  const at = (t: number) => new Date(t).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  if (h.cooling_until && h.cooling_until > now) return { text: `Cooling down until ${at(h.cooling_until)} (the provider asked to wait)`, tone: "bad" };
  if (h.circuit === "open") return { text: h.open_until ? `Circuit open until ${at(h.open_until)} · ${h.failures} failures in a row` : `Circuit open · ${h.failures} failures in a row`, tone: "bad" };
  if (h.circuit === "half_open") return { text: "Trying one request again", tone: "quiet" };
  return { text: "Healthy", tone: "ok" };
}

/** One upstream in the Gateway section: health, priority, on/off, edit, key, check, remove. */
export function UpstreamItem({
  upstream: u,
  keyState,
  health,
  check,
  first,
  last,
  locked,
  onMove,
  onToggle,
  onEdit,
  onKey,
  onCheck,
  onRemove,
}: {
  upstream: UpstreamSpec;
  keyState: KeyState;
  health?: UpstreamHealth;
  check?: UpstreamCheck | "busy";
  first: boolean;
  last: boolean;
  locked: boolean;
  onMove: (by: -1 | 1) => void;
  onToggle: (on: boolean) => void;
  onEdit: (key: boolean) => void;
  onKey: (key: null) => Promise<void>;
  onCheck: () => void;
  onRemove: () => Promise<void>;
}) {
  const [asking, setAsking] = useState(false);
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const h = healthLabel(health);
  const act = async (fn: () => Promise<void>) => {
    setBusy(true);
    setProblem(null);
    try {
      await fn();
    } catch (e) {
      setProblem(message(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div role="group" aria-label={`Upstream ${u.name}`} className={cn("px-3.5 py-3 text-[13px]", u.disabled && "opacity-60")}>
      <div className="flex flex-wrap items-start gap-x-3 gap-y-1.5">
        <KindTile kind={u.kind} className="mt-0.5" />
        <div className="min-w-52 flex-1">
          <div className="flex min-w-0 flex-wrap items-center gap-1.5">
            <h4 className="truncate font-medium tracking-[-0.01em]">{u.name}</h4>
            <Note>{KIND_PRESETS[u.kind].label}</Note>
            {u.disabled && <Note>off</Note>}
          </div>
          <p className="truncate font-mono text-[11.5px] text-muted" title={upstreamUrl(u)}>
            {upstreamUrl(u)}
            {u.models?.length ? ` · ${u.models.join(", ")}` : ""}
          </p>
          <p className="mt-1 flex min-w-0 flex-wrap items-center gap-x-2 gap-y-0.5 text-[12px]">
            <span className={cn("inline-flex items-center gap-1.5", h.tone === "bad" ? "text-danger" : "text-muted")}>
              <span className={cn("size-1.5 rounded-full", h.tone === "ok" ? "bg-ok" : h.tone === "bad" ? "bg-danger" : "bg-faint")} aria-hidden />
              {h.text}
            </span>
            {health && (
              <span className="font-mono text-[11px] tabular-nums text-subtle">
                {health.requests_hour}/h{health.p50_ms !== undefined ? ` · p50 ${health.p50_ms} ms` : ""}
              </span>
            )}
          </p>
          {health?.last_error && h.tone !== "ok" && (
            <p className="mt-0.5 truncate text-[11.5px] text-subtle" title={health.last_error}>
              Last error{health.last_error_at ? ` ${ago(health.last_error_at)}` : ""}: {health.last_error}
            </p>
          )}
        </div>
        {!asking && (
          <div className="-mr-1 ml-auto flex shrink-0 items-center">
            <button type="button" disabled={locked || first} onClick={() => onMove(-1)} className={iconBtn} aria-label={`Move ${u.name} up`} title="Higher priority">
              <ArrowUp className="size-3.5" />
            </button>
            <button type="button" disabled={locked || last} onClick={() => onMove(1)} className={iconBtn} aria-label={`Move ${u.name} down`} title="Lower priority">
              <ArrowDown className="size-3.5" />
            </button>
            <button type="button" disabled={locked || check === "busy"} onClick={onCheck} className={iconBtn} aria-label={`Check ${u.name}`} title="Check: its key opens, it lists its models (never generates)">
              <RefreshCw className={cn("size-3.5", check === "busy" && "animate-spin motion-reduce:animate-none")} />
            </button>
            <button type="button" disabled={locked} onClick={() => onEdit(false)} className={iconBtn} aria-label={`Edit ${u.name}`} title="Edit">
              <Pencil className="size-3.5" />
            </button>
            <button type="button" disabled={locked} onClick={() => setAsking(true)} className={cn(iconBtn, "hover:text-danger")} aria-label={`Remove ${u.name}`} title="Remove">
              <Trash2 className="size-3.5" />
            </button>
            <Switch size="sm" className="ml-1.5" checked={!u.disabled} disabled={locked} onCheckedChange={onToggle} aria-label={`Use ${u.name}`} />
          </div>
        )}
      </div>
      <div className="mt-2 flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 pl-9 text-[12px]">
        <KeyRound className="size-3.5 shrink-0 text-subtle" aria-hidden />
        <span className="min-w-0 truncate text-muted" title={keyState === "reference" ? u.key_ref : undefined}>
          {keyState === "reference" ? `1Password: ${u.key_ref ?? ""}` : keyState === "sealed" ? "Key set (never shown)" : "No key"}
        </span>
        {keyState === "sealed" && (
          <>
            <button type="button" disabled={locked} className={textBtn} onClick={() => onEdit(true)} aria-label={`Replace ${u.name}'s key`}>
              Replace
            </button>
            <button type="button" disabled={locked || busy} className={textBtn} onClick={() => void act(() => onKey(null))} aria-label={`Remove ${u.name}'s key`}>
              Remove
            </button>
          </>
        )}
        {keyState === "none" && (
          <button type="button" disabled={locked} className={textBtn} onClick={() => onEdit(true)}>
            Add a key
          </button>
        )}
      </div>
      {check && (
        <p className={cn("mt-1.5 flex items-center gap-1.5 pl-9 text-[12px]", check !== "busy" && !check.ok ? "text-danger" : "text-muted")} role="status">
          {check === "busy" ? (
            <span className="shimmer">Checking…</span>
          ) : check.ok ? (
            <>
              <Check className="size-3.5 text-ok" aria-hidden /> {check.models} models · {check.ms} ms
            </>
          ) : (
            (check.error ?? "The check failed")
          )}
        </p>
      )}
      {asking && (
        <div className="mt-2 flex flex-wrap items-center gap-2 rounded-lg border border-line-2 bg-panel-2 px-2.5 py-2 text-[12px]">
          <span className="min-w-0 flex-1 text-muted">Remove {u.name}? Its key is forgotten, and routes through it lose that step.</span>
          <button type="button" disabled={busy} onClick={() => void act(onRemove)} className={dangerBtn}>
            {busy ? <span className="shimmer">Removing…</span> : `Remove ${u.name}`}
          </button>
          <button type="button" onClick={() => setAsking(false)} className={iconBtn} aria-label="Keep it">
            <X className="size-3.5" />
          </button>
        </div>
      )}
      {problem && (
        <div className="mt-1.5 pl-9">
          <Problem>{problem}</Problem>
        </div>
      )}
    </div>
  );
}
