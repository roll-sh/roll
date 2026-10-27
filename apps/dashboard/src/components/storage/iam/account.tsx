"use client";

import { Download, FileText } from "lucide-react";
import { useState } from "react";
import {
  credentialReport,
  decodeBase64Text,
  iamApi,
  passwordPolicyDraft,
  passwordPolicyError,
  passwordPolicyParams,
  passwordProblems,
  reportCell,
  summaryStats,
  type CredentialReport,
  type PasswordPolicy,
  type PasswordPolicyDraft,
} from "@/lib/storage-iam";
import { errorMessage, type ConsoleSession } from "@/lib/storage-session";
import { btn, ErrorNote, input, Panel, primary, Stats } from "../../ui";
import { Check, Field, SaveBar, useAction } from "../shared";
import { DeleteButton, Loading, Muted, Row, useLoad, When } from "./common";

/** Account-wide IAM settings: summary, alias, password policy, credential report; your own password when signed in. */
export function AccountTab({ accountId, session }: { accountId: string; session: ConsoleSession | null }) {
  return (
    <div className="mt-4 space-y-4">
      <Summary />
      <div className="grid items-start gap-4 lg:grid-cols-2">
        <div className="space-y-4">
          <Alias accountId={accountId} />
          {session && <ChangePassword />}
          <UnassignedDevices />
        </div>
        <PasswordPolicyPanel />
      </div>
      <CredentialReportPanel />
    </div>
  );
}

function Summary() {
  const summary = useLoad("summary", iamApi.summary);
  if (summary.error) return <ErrorNote error={summary.error} />;
  const s = summary.data ? summaryStats(summary.data) : null;
  return (
    <Stats
      items={[
        { label: "Users", value: s ? s.users : "—", hint: s?.usersQuota ? `of ${s.usersQuota}` : undefined },
        { label: "Groups", value: s ? s.groups : "—" },
        { label: "Roles", value: s ? s.roles : "—" },
        { label: "Policies", value: s ? s.policies : "—", hint: s ? `${s.mfaDevices} MFA device${s.mfaDevices === 1 ? "" : "s"} in use` : undefined },
      ]}
    />
  );
}

function Alias({ accountId }: { accountId: string }) {
  const aliases = useLoad("aliases", iamApi.listAliases);
  const [alias, setAlias] = useState("");
  const { busy, error, run } = useAction(aliases.reload);
  const current = aliases.data?.[0] ?? null;
  const valid = /^[a-z0-9](([a-z0-9]|-(?!-))*[a-z0-9])?$/.test(alias) && alias.length >= 3 && alias.length <= 63;
  return (
    <Panel title="Account alias" aside={<span className="font-mono text-[12px] text-subtle">{accountId}</span>}>
      <div className="space-y-3 px-5 py-4">
        <ErrorNote error={error ?? aliases.error} />
        {!aliases.loaded ? (
          !aliases.error && <Loading />
        ) : current ? (
          <ul>
            <Row actions={<DeleteButton label={`Delete alias ${current}`} busy={busy === "delete"} onConfirm={() => run("delete", () => iamApi.deleteAlias(current))} />}>
              <p className="font-mono text-[13px]">{current}</p>
              <p className="text-[11px] text-subtle">IAM users can sign in with it instead of the account id.</p>
            </Row>
          </ul>
        ) : (
          <form
            className="space-y-2"
            onSubmit={async (e) => {
              e.preventDefault();
              if (!valid) return;
              const r = await run("create", () => iamApi.createAlias(alias));
              if (r) setAlias("");
            }}
          >
            <Muted>A friendly name for signing in instead of the 12-digit id. Unique across the deployment.</Muted>
            <div className="flex gap-2">
              <input
                value={alias}
                onChange={(e) => setAlias(e.target.value.toLowerCase())}
                placeholder="acme-prod"
                spellCheck={false}
                aria-label="Account alias"
                className={`${input} min-w-0 flex-1 font-mono text-[12px]`}
              />
              <button type="submit" disabled={!valid || busy !== null} className={primary}>
                {busy === "create" ? <span className="shimmer">Saving…</span> : "Set alias"}
              </button>
            </div>
            {alias && !valid && <p className="text-[12px] text-danger">3–63 lowercase letters, digits and single hyphens, not at either end.</p>}
          </form>
        )}
      </div>
    </Panel>
  );
}

function PasswordPolicyPanel() {
  const policy = useLoad("password-policy-panel", iamApi.getPasswordPolicy);
  return (
    <Panel title="Password policy" aside={<span className="text-[12px] text-subtle">{policy.loaded ? (policy.data ? "custom" : "IAM defaults") : ""}</span>}>
      <div className="px-5 py-4">
        <ErrorNote error={policy.error} />
        {!policy.loaded ? (
          !policy.error && <Loading />
        ) : (
          <PolicyForm key={JSON.stringify(policy.data)} policy={policy.data} onChanged={policy.reload} />
        )}
      </div>
    </Panel>
  );
}

function PolicyForm({ policy, onChanged }: { policy: PasswordPolicy | null; onChanged: () => Promise<void> }) {
  const saved = passwordPolicyDraft(policy);
  const [d, setD] = useState<PasswordPolicyDraft>(saved);
  const { busy, error, run } = useAction(onChanged);
  const set = <K extends keyof PasswordPolicyDraft>(k: K, v: PasswordPolicyDraft[K]) => setD((x) => ({ ...x, [k]: v }));
  const err = passwordPolicyError(d);
  const dirty = !policy || JSON.stringify(d) !== JSON.stringify(saved);
  const intField = (k: "min" | "maxAge" | "reuse", label: string, hint: string, max: number) => (
    <Field label={label} hint={hint}>
      <input
        type="number"
        min={0}
        max={max}
        value={d[k]}
        onChange={(e) => set(k, Math.floor(Number(e.target.value) || 0))}
        className={`${input} w-full font-mono text-[12px] tabular-nums`}
      />
    </Field>
  );
  return (
    <form
      className="space-y-4"
      onSubmit={(e) => {
        e.preventDefault();
        if (!err) run("save", () => iamApi.updatePasswordPolicy(passwordPolicyParams(d)));
      }}
    >
      {!policy && (
        <Muted>
          None set: passwords need 8–128 characters with three of uppercase, lowercase, digits and symbols. Save to set your own rules.
        </Muted>
      )}
      <div className="grid gap-3 sm:grid-cols-3">
        {intField("min", "Minimum length", "6–128", 128)}
        {intField("maxAge", "Expire after (days)", "0: never", 1095)}
        {intField("reuse", "Remember last", "0: off", 24)}
      </div>
      <div className="grid gap-3 sm:grid-cols-2">
        <Check checked={d.upper} onChange={(v) => set("upper", v)} label="An uppercase letter" />
        <Check checked={d.lower} onChange={(v) => set("lower", v)} label="A lowercase letter" />
        <Check checked={d.numbers} onChange={(v) => set("numbers", v)} label="A digit" />
        <Check checked={d.symbols} onChange={(v) => set("symbols", v)} label="A symbol" hint="! @ # $ % ^ & * ( ) _ + - = [ ] { } | '" />
        <Check checked={d.allowChange} onChange={(v) => set("allowChange", v)} label="Users may change their own password" />
        <Check
          checked={d.hardExpiry}
          disabled={d.maxAge === 0}
          onChange={(v) => set("hardExpiry", v)}
          label="Expired passwords lock the user out"
          hint="Otherwise they choose a new one at sign-in."
        />
      </div>
      {err && <p className="text-[12px] text-danger">{err}</p>}
      <ErrorNote error={error} />
      <SaveBar dirty={dirty} busy={busy === "save"} onReset={() => setD(saved)} label={policy ? "Save" : "Set policy"} />
      {policy && (
        <button type="button" disabled={busy !== null} onClick={() => run("delete", () => iamApi.deletePasswordPolicy())} className={btn}>
          {busy === "delete" ? <span className="shimmer">Removing…</span> : "Remove the policy (back to IAM defaults)"}
        </button>
      )}
    </form>
  );
}

/** ChangePassword: the signed-in user's own password. */
function ChangePassword() {
  const policy = useLoad("own-password-policy", () => iamApi.getPasswordPolicy().catch(() => undefined));
  const [old, setOld] = useState("");
  const [pw, setPw] = useState("");
  const [confirm, setConfirm] = useState("");
  const [done, setDone] = useState(false);
  const { busy, error, run } = useAction(async () => {});
  const problems = pw && policy.data !== undefined ? passwordProblems(pw, policy.data) : [];
  const mismatch = confirm !== "" && confirm !== pw;
  return (
    <Panel title="Your password">
      <form
        className="space-y-3 px-5 py-4"
        onSubmit={async (e) => {
          e.preventDefault();
          if (!old || !pw || pw !== confirm) return;
          const r = await run("change", () => iamApi.changePassword(old, pw));
          if (r) {
            setOld("");
            setPw("");
            setConfirm("");
            setDone(true);
          }
        }}
      >
        <Field label="Current password">
          <input type="password" value={old} onChange={(e) => setOld(e.target.value)} autoComplete="current-password" className={`${input} w-full`} />
        </Field>
        <div className="grid gap-3 sm:grid-cols-2">
          <Field label="New password">
            <input
              type="password"
              value={pw}
              onChange={(e) => {
                setPw(e.target.value);
                setDone(false);
              }}
              autoComplete="new-password"
              className={`${input} w-full`}
            />
          </Field>
          <Field label="Confirm">
            <input type="password" value={confirm} onChange={(e) => setConfirm(e.target.value)} autoComplete="new-password" className={`${input} w-full`} />
          </Field>
        </div>
        {mismatch ? (
          <p className="text-[12px] text-danger">The passwords don&apos;t match.</p>
        ) : problems.length > 0 ? (
          <p className="text-[12px] text-subtle">Needs {problems.join(", ")}.</p>
        ) : null}
        <ErrorNote error={error} />
        {done && <p className="rise text-[12px] text-ok">Password changed.</p>}
        <button type="submit" disabled={!old || !pw || pw !== confirm || problems.length > 0 || busy !== null} className={primary}>
          {busy ? <span className="shimmer">Changing…</span> : "Change password"}
        </button>
      </form>
    </Panel>
  );
}

/** Virtual devices created but never enabled (an abandoned setup): clean them up. */
function UnassignedDevices() {
  const devices = useLoad("unassigned-mfa", () => iamApi.listVirtualMfaDevices("Unassigned"));
  const { busy, error, run } = useAction(devices.reload);
  if (!devices.data?.length && !devices.error) return null;
  return (
    <Panel title="Unassigned MFA devices">
      <div className="px-5 py-3">
        <ErrorNote error={error ?? devices.error} />
        <ul>
          {(devices.data ?? []).map((d) => (
            <Row
              key={d.SerialNumber}
              actions={
                <DeleteButton
                  label={`Delete ${d.SerialNumber}`}
                  busy={busy === d.SerialNumber}
                  onConfirm={() => run(d.SerialNumber, () => iamApi.deleteVirtualMfaDevice(d.SerialNumber))}
                />
              }
            >
              <p className="truncate font-mono text-[12px]">{d.SerialNumber}</p>
            </Row>
          ))}
        </ul>
      </div>
    </Panel>
  );
}

/** The columns shown (the CSV download has every column). */
const REPORT_COLUMNS: { key: string; label: string; date?: boolean }[] = [
  { key: "user", label: "User" },
  { key: "password_enabled", label: "Password" },
  { key: "password_last_used", label: "Signed in", date: true },
  { key: "mfa_active", label: "MFA" },
  { key: "access_key_1_active", label: "Key 1" },
  { key: "access_key_1_last_used_date", label: "Key 1 used", date: true },
  { key: "access_key_2_active", label: "Key 2" },
  { key: "access_key_2_last_used_date", label: "Key 2 used", date: true },
];

function CredentialReportPanel() {
  const [report, setReport] = useState<{ parsed: CredentialReport; csv: string; generated: string | null } | null>(null);
  const [state, setState] = useState<string | null>(null);
  const { busy, error, setError, run } = useAction(async () => {});

  const generate = () =>
    run("generate", async () => {
      // IAM generates in the background: ask until it's COMPLETE (or give up after ~30 s).
      for (let i = 0; i < 30; i++) {
        const g = await iamApi.generateCredentialReport();
        setState(g.State);
        if (g.State === "COMPLETE") break;
        await new Promise((r) => setTimeout(r, 1000));
      }
      const r = await iamApi.getCredentialReport();
      try {
        const csv = decodeBase64Text(r.Content);
        setReport({ parsed: credentialReport(r.Content), csv, generated: r.GeneratedTime ?? null });
      } catch (e) {
        setError(`The report couldn't be read: ${errorMessage(e)}`);
      }
      return r;
    });

  const download = () => {
    if (!report) return;
    const url = URL.createObjectURL(new Blob([report.csv], { type: "text/csv" }));
    const a = document.createElement("a");
    a.href = url;
    a.download = "credential-report.csv";
    a.click();
    setTimeout(() => URL.revokeObjectURL(url), 10_000);
  };

  return (
    <Panel
      title="Credential report"
      aside={
        <span className="flex items-center gap-2">
          {report && (
            <button type="button" onClick={download} className={btn}>
              <Download className="size-3.5" /> CSV
            </button>
          )}
          <button type="button" onClick={generate} disabled={busy !== null} className={btn}>
            {busy ? (
              <span className="shimmer">{state === "STARTED" || state === "INPROGRESS" ? "Generating…" : "Fetching…"}</span>
            ) : (
              <>
                <FileText className="size-3.5" /> {report ? "Regenerate" : "Generate"}
              </>
            )}
          </button>
        </span>
      }
    >
      {error && (
        <div className="px-5 pb-3">
          <ErrorNote error={error} />
        </div>
      )}
      {!report ? (
        <p className="px-5 py-4 text-[13px] text-muted">Every user with their password, MFA and access keys, and when each was last used.</p>
      ) : report.parsed.rows.length === 0 ? (
        <p className="px-5 py-4 text-[13px] text-muted">The report is empty.</p>
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full text-left text-[12px]">
            <thead>
              <tr className="border-b border-line text-[11px] text-subtle">
                {REPORT_COLUMNS.map((c) => (
                  <th key={c.key} className="px-5 py-2 font-normal whitespace-nowrap">
                    {c.label}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {report.parsed.rows.map((row, i) => (
                <tr key={`${row.arn ?? row.user}-${i}`} className="border-b border-line last:border-0">
                  {REPORT_COLUMNS.map((c) => {
                    const v = reportCell(row[c.key]);
                    return (
                      <td key={c.key} className={`px-5 py-2 font-mono whitespace-nowrap ${v === "true" ? "text-fg" : "text-muted"}`} title={row[c.key]}>
                        {c.date && v !== "—" ? <When iso={v} /> : v === "true" ? "yes" : v === "false" ? "no" : v}
                      </td>
                    );
                  })}
                </tr>
              ))}
            </tbody>
          </table>
          {report.generated && (
            <p className="border-t border-line px-5 py-2 text-[11px] text-subtle">
              generated <When iso={report.generated} />
            </p>
          )}
        </div>
      )}
    </Panel>
  );
}
