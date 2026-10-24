"use client";

import { CircleCheck, CircleMinus, CircleX, Plus, X } from "lucide-react";
import { useState } from "react";
import {
  bool,
  CONTEXT_TYPES,
  decisionLabel,
  iamApi,
  members,
  simulationError,
  simulationRequest,
  sourceDocument,
  splitList,
  starterPolicy,
  statementLines,
  statementRange,
  type ContextDraft,
  type ContextType,
  type EvaluationResult,
  type IamGroup,
  type IamRole,
  type IamUser,
  type MatchedStatement,
  type SimulationDraft,
} from "@/lib/storage-iam";
import { errorMessage } from "@/lib/storage-session";
import { useMarkState } from "../../mark-state";
import { iconBtn } from "../../tasks/ui";
import { btn, ErrorNote, input, Panel, primary } from "../../ui";
import { Chip, Field, Segmented } from "../shared";
import { Muted, PolicyEditor, useLoad } from "./common";

const COMMON_ACTIONS = ["s3:GetObject", "s3:PutObject", "s3:DeleteObject", "s3:ListBucket", "s3:ListAllMyBuckets", "s3:PutBucketPolicy"];

const EMPTY: SimulationDraft = {
  mode: "principal",
  sourceArn: "",
  policies: [],
  boundaries: [],
  actions: "s3:GetObject",
  resources: "",
  context: [],
  resourcePolicy: "",
  callerArn: "",
};

/** IAM's policy simulator: would these actions be allowed for a principal, or under a policy? */
export function SimulatorTab() {
  const principals = useLoad("sim-principals", async () => {
    const [users, groups, roles] = await Promise.all([
      iamApi.listUsers().catch(() => [] as IamUser[]),
      iamApi.listGroups().catch(() => [] as IamGroup[]),
      iamApi.listRoles().catch(() => [] as IamRole[]),
    ]);
    return { users, groups, roles };
  });
  const [draft, setDraft] = useState<SimulationDraft>(EMPTY);
  const [custom, setCustom] = useState(starterPolicy());
  const [boundary, setBoundary] = useState("");
  const [more, setMore] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [results, setResults] = useState<{ draft: SimulationDraft; results: EvaluationResult[] } | null>(null);
  const { setState, play } = useMarkState();

  const full: SimulationDraft = {
    ...draft,
    policies: draft.mode === "custom" ? [custom] : [],
    boundaries: boundary.trim() ? [boundary] : [],
  };
  const problem = simulationError(full);
  const set = <K extends keyof SimulationDraft>(k: K, v: SimulationDraft[K]) => setDraft((d) => ({ ...d, [k]: v }));
  const setCtx = (i: number, patch: Partial<ContextDraft>) => set("context", draft.context.map((c, j) => (j === i ? { ...c, ...patch } : c)));
  const actions = splitList(draft.actions);
  const toggleAction = (a: string) => set("actions", (actions.includes(a) ? actions.filter((x) => x !== a) : [...actions, a]).join(", "));

  const simulate = async () => {
    if (problem || busy) return;
    setBusy(true);
    setError(null);
    setState("thinking");
    try {
      const r = await iamApi.simulate(simulationRequest(full));
      setResults({ draft: full, results: r });
      play([["success", 1200]]);
    } catch (e) {
      setError(errorMessage(e));
      play([["error", 1400]]);
    } finally {
      setBusy(false);
    }
  };

  const p = principals.data;
  return (
    <div className="mt-4 grid items-start gap-4 lg:grid-cols-2">
      <Panel title="Simulate">
        <form
          className="space-y-4 px-5 py-4"
          onSubmit={(e) => {
            e.preventDefault();
            simulate();
          }}
        >
          <Segmented
            value={draft.mode}
            options={[
              { label: "user, group or role", value: "principal" },
              { label: "custom policy", value: "custom" },
            ]}
            onChange={(m) => set("mode", m)}
          />
          {draft.mode === "principal" ? (
            <Field label="Principal" hint="Its own, group and attached policies are evaluated together.">
              <select value={draft.sourceArn} onChange={(e) => set("sourceArn", e.target.value)} className={`${input} w-full font-mono text-[12px]`}>
                <option value="">{p ? "Choose…" : "Loading…"}</option>
                {p && p.users.length > 0 && (
                  <optgroup label="Users">
                    {p.users.map((u) => (
                      <option key={u.Arn} value={u.Arn}>
                        {u.UserName}
                      </option>
                    ))}
                  </optgroup>
                )}
                {p && p.groups.length > 0 && (
                  <optgroup label="Groups">
                    {p.groups.map((g) => (
                      <option key={g.Arn} value={g.Arn}>
                        {g.GroupName}
                      </option>
                    ))}
                  </optgroup>
                )}
                {p && p.roles.length > 0 && (
                  <optgroup label="Roles">
                    {p.roles.map((r) => (
                      <option key={r.Arn} value={r.Arn}>
                        {r.RoleName}
                      </option>
                    ))}
                  </optgroup>
                )}
              </select>
            </Field>
          ) : (
            <div className="space-y-1">
              <p className="text-[11px] text-subtle">Policy</p>
              <PolicyEditor value={custom} onChange={setCustom} label="Policy to simulate" />
            </div>
          )}

          <Field label="Actions" hint="Separated by commas or spaces; wildcards like s3:* work.">
            <input value={draft.actions} onChange={(e) => set("actions", e.target.value)} spellCheck={false} className={`${input} w-full font-mono text-[12px]`} />
          </Field>
          <div className="flex flex-wrap gap-1.5">
            {COMMON_ACTIONS.map((a) => (
              <Chip key={a} on={actions.includes(a)} onClick={() => toggleAction(a)}>
                {a}
              </Chip>
            ))}
          </div>
          <Field label="Resources" hint="ARNs, one per line; empty: *">
            <textarea
              value={draft.resources}
              onChange={(e) => set("resources", e.target.value)}
              rows={2}
              spellCheck={false}
              placeholder="arn:aws:s3:::my-bucket/*"
              className={`${input} block w-full resize-y font-mono text-[12px]`}
            />
          </Field>

          <div className="space-y-2">
            <div className="flex items-center justify-between">
              <p className="text-[11px] text-subtle">Context keys</p>
              <button type="button" onClick={() => set("context", [...draft.context, { key: "", type: "string", values: "" }])} className={btn}>
                <Plus className="size-3.5" /> Add
              </button>
            </div>
            {draft.context.length === 0 && <Muted>None. Conditions on keys you don&apos;t give are reported as missing.</Muted>}
            {draft.context.map((c, i) => (
              <div key={i} className="rise flex flex-wrap gap-1.5">
                <input
                  value={c.key}
                  onChange={(e) => setCtx(i, { key: e.target.value })}
                  placeholder="aws:SourceIp"
                  spellCheck={false}
                  aria-label="Context key"
                  className={`${input} w-36 font-mono text-[12px]`}
                />
                <select value={c.type} onChange={(e) => setCtx(i, { type: e.target.value as ContextType })} aria-label="Type" className={`${input} font-mono text-[12px]`}>
                  {CONTEXT_TYPES.map((t) => (
                    <option key={t} value={t}>
                      {t}
                    </option>
                  ))}
                </select>
                <input
                  value={c.values}
                  onChange={(e) => setCtx(i, { values: e.target.value })}
                  placeholder={c.type.endsWith("List") ? "a, b" : "value"}
                  spellCheck={false}
                  aria-label="Values"
                  className={`${input} min-w-0 flex-1 font-mono text-[12px]`}
                />
                <button
                  type="button"
                  onClick={() => set("context", draft.context.filter((_, j) => j !== i))}
                  className={iconBtn}
                  aria-label="Remove context key"
                  title="Remove"
                >
                  <X className="size-3.5" />
                </button>
              </div>
            ))}
          </div>

          <button type="button" onClick={() => setMore((m) => !m)} className="text-[12px] text-muted transition hover:text-fg">
            {more ? "Fewer options" : "Resource policy, caller and permissions boundary…"}
          </button>
          {more && (
            <div className="rise space-y-4">
              <div className="space-y-1">
                <p className="text-[11px] text-subtle">Resource policy (a bucket policy, say) — optional</p>
                <PolicyEditor value={draft.resourcePolicy} onChange={(v) => set("resourcePolicy", v)} label="Resource policy" minRows={4} />
              </div>
              <Field label="Caller ARN" hint="Who calls, for the resource policy's Principal; optional">
                <input
                  value={draft.callerArn}
                  onChange={(e) => set("callerArn", e.target.value)}
                  spellCheck={false}
                  placeholder="arn:aws:iam::123456789012:user/alice"
                  className={`${input} w-full font-mono text-[12px]`}
                />
              </Field>
              <div className="space-y-1">
                <p className="text-[11px] text-subtle">Permissions boundary — optional</p>
                <PolicyEditor value={boundary} onChange={setBoundary} label="Permissions boundary" minRows={4} />
              </div>
            </div>
          )}

          <ErrorNote error={error ?? principals.error} />
          <div className="flex items-center gap-3">
            <button type="submit" disabled={!!problem || busy} className={primary}>
              {busy ? <span className="shimmer">Simulating…</span> : "Simulate"}
            </button>
            {problem && <span className="text-[12px] text-subtle">{problem}</span>}
          </div>
        </form>
      </Panel>

      <Panel title="Results" aside={results && <span className="font-mono text-[12px] text-subtle">{results.results.length}</span>}>
        {!results ? (
          <p className="px-5 py-6 text-[13px] text-muted">
            Each action is evaluated against every policy in play — identity, group, resource and boundary — the way S3 requests are.
          </p>
        ) : results.results.length === 0 ? (
          <p className="px-5 py-6 text-[13px] text-muted">No results.</p>
        ) : (
          <ul>
            {results.results.map((r, i) => (
              <Result key={`${r.EvalActionName}-${r.EvalResourceName}-${i}`} r={r} draft={results.draft} />
            ))}
          </ul>
        )}
      </Panel>
    </div>
  );
}

function Result({ r, draft }: { r: EvaluationResult; draft: SimulationDraft }) {
  const d = decisionLabel(r.EvalDecision);
  const Icon = d.tone === "ok" ? CircleCheck : d.tone === "danger" ? CircleX : CircleMinus;
  const tone = d.tone === "ok" ? "text-ok" : d.tone === "danger" ? "text-danger" : "text-muted";
  const matched = members<MatchedStatement>(r.MatchedStatements);
  const missing = members<string>(r.MissingContextValues);
  const details = r.EvalDecisionDetails && typeof r.EvalDecisionDetails === "object" ? Object.entries(r.EvalDecisionDetails) : [];
  const boundary = r.PermissionsBoundaryDecisionDetail?.AllowedByPermissionsBoundary;
  return (
    <li className="rise space-y-2 border-b border-line px-5 py-3 last:border-0">
      <div className="flex items-start gap-3">
        <Icon className={`mt-0.5 size-4 shrink-0 ${tone}`} />
        <div className="min-w-0 flex-1">
          <p className="flex flex-wrap items-baseline gap-x-2 text-[13px]">
            <span className="font-mono">{r.EvalActionName}</span>
            <span className={`text-[12px] ${tone}`}>{d.label}</span>
          </p>
          <p className="truncate font-mono text-[11px] text-subtle">{r.EvalResourceName || "*"}</p>
        </div>
      </div>
      {matched.length > 0 && (
        <div className="space-y-1.5 pl-7">
          {matched.map((m, i) => (
            <Statement key={i} m={m} draft={draft} />
          ))}
        </div>
      )}
      {missing.length > 0 && (
        <p className="pl-7 text-[12px] text-muted">
          Missing context: <span className="font-mono text-[11px]">{missing.join(", ")}</span>
        </p>
      )}
      {(details.length > 0 || boundary != null) && (
        <p className="flex flex-wrap gap-x-3 pl-7 font-mono text-[11px] text-subtle">
          {details.map(([k, v]) => (
            <span key={k}>
              {k}: {v}
            </span>
          ))}
          {boundary != null && <span>{bool(boundary) ? "within the permissions boundary" : "outside the permissions boundary"}</span>}
        </p>
      )}
    </li>
  );
}

/** A matched statement: its policy and place, with the lines when the document was typed here. */
function Statement({ m, draft }: { m: MatchedStatement; draft: SimulationDraft }) {
  const range = statementRange(m);
  const doc = sourceDocument(m, draft);
  const lines = statementLines(m);
  const snippet = doc && lines ? doc.split("\n").slice(lines[0] - 1, lines[1]) : null;
  return (
    <div className="space-y-1">
      <p className="flex flex-wrap gap-x-2 font-mono text-[11px] text-muted">
        <span className="text-fg">{m.SourcePolicyId || "policy"}</span>
        {m.SourcePolicyType && <span className="text-subtle">{m.SourcePolicyType}</span>}
        {range && <span className="text-subtle">line {range}</span>}
      </p>
      {snippet && snippet.length > 0 && (
        <pre className="max-h-40 overflow-auto rounded-md border border-line bg-bg px-3 py-1.5 font-mono text-[11px] leading-relaxed text-muted">
          {snippet.map((l, i) => (
            <span key={i} className="block">
              <span className="mr-3 inline-block w-6 text-right text-subtle select-none">{lines![0] + i}</span>
              {l}
            </span>
          ))}
        </pre>
      )}
    </div>
  );
}
