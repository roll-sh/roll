"use client";

import { KeyRound } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { codeApi, type AiProvider, type Billing, type OnCap, type Pricing, type ReviewProvider, type Usage } from "@/lib/code-api";
import {
  capUse,
  competitorRow,
  formatTokens,
  formatUsd,
  MODE_LABEL,
  modeBars,
  monthKey,
  monthLabel,
  pricingRows,
  recentMonths,
  repoBars,
  type Bar,
} from "@/lib/ai-review";
import { formatDateTime } from "@/lib/time";
import { btn, input, Meter, Panel, primary, Stats } from "../ui";
import { Field, Segmented, type Run } from "./shared";

const ON_CAP: { value: OnCap; label: string; hint: string }[] = [
  { value: "pause", label: "Pause reviews", hint: "roll stops reviewing until next month or a higher cap" },
  { value: "byok_fallback", label: "Use your key", hint: "keep reviewing with your own provider key (below)" },
  { value: "standard_only", label: "Standard only", hint: "deep and max reviews drop to standard" },
];

/** Monochrome horizontal bars, largest first. */
function Bars({ rows, empty }: { rows: Bar[]; empty: string }) {
  if (!rows.length) return <p className="text-[12px] text-subtle">{empty}</p>;
  return (
    <ul className="space-y-2">
      {rows.map((b) => (
        <li key={b.key} className="grid grid-cols-[minmax(0,10rem)_1fr_auto] items-center gap-3 text-[12px]">
          <span className="truncate font-mono text-muted" title={b.label}>
            {b.label}
          </span>
          <span className="h-1.5 overflow-hidden rounded-full bg-line" aria-hidden>
            <span className="block h-full rounded-full bg-muted transition-[width] duration-300 motion-reduce:transition-none" style={{ width: `${Math.max(2, b.frac * 100)}%` }} />
          </span>
          <span className="font-mono text-[11px] text-subtle tabular-nums">{b.detail}</span>
        </li>
      ))}
    </ul>
  );
}

/**
 * An owner's spend on roll's reviewer: the month's totals, by repo and by mode, recent reviews,
 * and the spending cap. The owner (or an admin) only.
 */
export function BillingUsage({ owner, busy, run }: { owner: string; busy: string | null; run: Run }) {
  const months = useMemo(() => recentMonths(new Date(), 12), []);
  const [month, setMonth] = useState(() => monthKey(new Date()));
  const [usage, setUsage] = useState<Usage | null>(null);
  const [billing, setBilling] = useState<Billing | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [cap, setCap] = useState("");
  const [onCap, setOnCap] = useState<OnCap>("pause");
  const [version, setVersion] = useState(0);

  useEffect(() => {
    let alive = true;
    Promise.all([codeApi.usage(owner, month), codeApi.billing(owner)]).then(
      ([u, b]) => {
        if (!alive) return;
        setUsage(u);
        setBilling(b);
        setCap(b.monthly_cap_usd == null ? "" : String(b.monthly_cap_usd));
        setOnCap(b.on_cap);
        setError(null);
      },
      (e) => alive && setError(e instanceof Error ? e.message : String(e)),
    );
    return () => {
      alive = false;
    };
  }, [owner, month, version]);

  const capValue = cap.trim() === "" ? null : Number(cap);
  const capValid = capValue === null || (Number.isFinite(capValue) && capValue >= 0);
  const capDirty = billing && (capValue !== billing.monthly_cap_usd || onCap !== billing.on_cap);

  return (
    <section id="billing" className="scroll-mt-24">
      <Panel
        title="Billing & usage"
        aside={
          <select value={month} onChange={(e) => setMonth(e.target.value)} className={`${input} py-1 text-[12px]`} aria-label="Month">
            {months.map((m) => (
              <option key={m} value={m}>
                {monthLabel(m)}
              </option>
            ))}
          </select>
        }
      >
        {!usage || !billing ? (
          <p className={`px-5 py-4 text-[12px] ${error ? "text-danger" : "text-subtle"}`}>{error ?? <span className="shimmer">Loading usage…</span>}</p>
        ) : (
          <div className="space-y-6 px-5 py-4">
            <Stats
              items={[
                { label: "Spent", value: formatUsd(usage.totals.price_usd), hint: usage.totals.list_usd > usage.totals.price_usd ? `list ${formatUsd(usage.totals.list_usd)}` : monthLabel(usage.month) },
                { label: "Reviews", value: usage.totals.reviews.toLocaleString(), hint: `${usage.totals.free_reviews} free · ${usage.totals.capped_reviews} capped` },
                { label: "Chat replies", value: usage.totals.chats.toLocaleString(), hint: "@roll answers and fixes" },
                {
                  label: "Tokens",
                  value: formatTokens(usage.totals.tokens.input + usage.totals.tokens.output + usage.totals.tokens.cache_read + usage.totals.tokens.cache_write),
                  hint: `${formatTokens(usage.totals.tokens.cache_read)} read from cache`,
                },
              ]}
            />

            {billing.monthly_cap_usd != null && (
              <div className="space-y-1.5">
                <p className="flex items-center justify-between text-[12px]">
                  <span className="text-muted">
                    {formatUsd(billing.spent_usd)} of {formatUsd(billing.monthly_cap_usd)} this month
                  </span>
                  <span className="text-subtle">{ON_CAP.find((o) => o.value === billing.on_cap)?.hint}</span>
                </p>
                <Meter value={(capUse(billing.spent_usd, billing.monthly_cap_usd) ?? 0) * 100} max={100} />
              </div>
            )}

            <div className="grid gap-6 lg:grid-cols-2">
              <div className="space-y-2">
                <p className="text-[11px] text-subtle">By repository</p>
                <Bars rows={repoBars(usage)} empty="No reviews this month." />
              </div>
              <div className="space-y-2">
                <p className="text-[11px] text-subtle">By depth</p>
                <Bars rows={modeBars(usage)} empty="No reviews this month." />
                {(usage.by_key.hosted || usage.by_key.byok) && (
                  <p className="pt-1 text-[11px] text-subtle">
                    {usage.by_key.hosted && `hosted ${usage.by_key.hosted.count} · ${formatUsd(usage.by_key.hosted.price_usd)}`}
                    {usage.by_key.hosted && usage.by_key.byok && " · "}
                    {usage.by_key.byok && `your key ${usage.by_key.byok.count} · ${formatUsd(usage.by_key.byok.price_usd)}`}
                  </p>
                )}
              </div>
            </div>

            {usage.recent.length > 0 && (
              <div className="overflow-x-auto">
                <table className="w-full text-[12px]">
                  <thead className="text-left text-[11px] text-subtle">
                    <tr className="border-b border-line">
                      <th className="py-1.5 pr-3 font-normal">When</th>
                      <th className="py-1.5 pr-3 font-normal">Kind</th>
                      <th className="py-1.5 pr-3 font-normal">Model</th>
                      <th className="py-1.5 pr-3 text-right font-normal">Files</th>
                      <th className="py-1.5 pr-3 font-normal">Author</th>
                      <th className="py-1.5 text-right font-normal">Price</th>
                    </tr>
                  </thead>
                  <tbody>
                    {usage.recent.map((r) => (
                      <tr key={r.id} className="border-b border-line last:border-0">
                        <td className="py-1.5 pr-3 whitespace-nowrap text-subtle">{formatDateTime(r.created_at)}</td>
                        <td className="py-1.5 pr-3 whitespace-nowrap">
                          {r.kind === "chat" ? "chat" : r.mode ? MODE_LABEL[r.mode] : "review"}
                          {r.chosen_by?.startsWith("auto") && <span className="ml-1 text-subtle">auto</span>}
                          {r.speed && r.speed !== "balanced" && <span className="ml-1 text-subtle">{r.speed}</span>}
                        </td>
                        <td className="py-1.5 pr-3 font-mono text-[11px] text-muted">
                          {r.model}
                          <span className="ml-1 text-subtle">{r.key_source === "byok" ? "your key" : "hosted"}</span>
                        </td>
                        <td className="py-1.5 pr-3 text-right font-mono tabular-nums">{r.files_reviewed}</td>
                        <td className="py-1.5 pr-3 text-muted">
                          {r.author ?? "—"}
                          {r.human_author ? "" : <span className="ml-1 text-subtle">agent</span>}
                        </td>
                        <td className="py-1.5 text-right font-mono tabular-nums">
                          {r.free ? <span className="text-subtle">free</span> : formatUsd(r.price_usd)}
                          {r.capped && <span className="ml-1 text-subtle">capped</span>}
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )}

            <form
              className="grid gap-4 border-t border-line pt-4 sm:grid-cols-[12rem_1fr_auto] sm:items-end"
              onSubmit={async (e) => {
                e.preventDefault();
                if (!capValid) return;
                const out = await run("billing", () => codeApi.setBilling(owner, { monthly_cap_usd: capValue, on_cap: onCap }));
                if (out !== undefined) setVersion((v) => v + 1);
              }}
            >
              <Field label="Monthly cap (USD)" hint={capValid ? "Empty for no cap" : "A number of dollars, 0 or more"}>
                <input value={cap} onChange={(e) => setCap(e.target.value)} inputMode="decimal" placeholder="no cap" className={`${input} w-full font-mono`} />
              </Field>
              <Field label="When the cap is reached" hint={ON_CAP.find((o) => o.value === onCap)?.hint}>
                <Segmented value={onCap} options={ON_CAP.map((o) => ({ label: o.label, value: o.value }))} onChange={setOnCap} />
              </Field>
              <button type="submit" disabled={!capDirty || !capValid || busy !== null} className={primary}>
                {busy === "billing" ? <span className="shimmer">Saving…</span> : "Save"}
              </button>
            </form>
          </div>
        )}
      </Panel>
    </section>
  );
}

/**
 * The owner's own model provider (bring your own key): roll calls it instead of the hosted one,
 * at a much lower platform fee. The key is write-only; only its last four characters come back.
 */
export function ProviderCard({ owner, busy, run }: { owner: string; busy: string | null; run: Run }) {
  const [current, setCurrent] = useState<ReviewProvider | null | undefined>(undefined);
  const [editing, setEditing] = useState(false);
  const [provider, setProvider] = useState<AiProvider>("anthropic");
  const [key, setKey] = useState("");
  const [baseUrl, setBaseUrl] = useState("");
  const [model, setModel] = useState("");
  const [secondary, setSecondary] = useState("");
  const [version, setVersion] = useState(0);

  useEffect(() => {
    let alive = true;
    codeApi.reviewProvider(owner).then(
      (p) => alive && setCurrent(p),
      () => alive && setCurrent(null),
    );
    return () => {
      alive = false;
    };
  }, [owner, version]);

  const compat = provider === "openai_compatible";
  const valid = key.trim().length > 0 && (!compat || (baseUrl.trim() && model.trim()));

  return (
    <Panel title="AI provider" aside={<span className="text-[11px] text-subtle">{current ? "your key" : "hosted by roll"}</span>}>
      <div className="space-y-4 px-5 py-4 text-[13px]">
        {current === undefined ? (
          <p className="text-[12px] text-subtle">
            <span className="shimmer">Loading…</span>
          </p>
        ) : current && !editing ? (
          <div className="flex flex-wrap items-center gap-3">
            <KeyRound className="size-4 text-muted" />
            <div className="min-w-0">
              <p>
                {current.provider === "anthropic" ? "Anthropic" : "OpenAI-compatible"} · <span className="font-mono text-[12px]">…{current.key_last4}</span>
              </p>
              <p className="text-[11px] text-subtle">
                {current.model ?? "provider default model"}
                {current.base_url && <span className="font-mono"> · {current.base_url}</span>} · updated {formatDateTime(current.updated_at)}
              </p>
            </div>
            <div className="ml-auto flex gap-2">
              <button type="button" onClick={() => setEditing(true)} className={btn}>
                Replace key
              </button>
              <button
                type="button"
                onClick={async () => {
                  if ((await run("provider-clear", () => codeApi.clearReviewProvider(owner))) !== undefined) setVersion((v) => v + 1);
                }}
                disabled={busy !== null}
                className={btn}
              >
                {busy === "provider-clear" ? <span className="shimmer">Removing…</span> : "Use hosted"}
              </button>
            </div>
          </div>
        ) : !editing ? (
          <div className="flex flex-wrap items-center gap-3">
            <p className="min-w-0 flex-1 text-[12px] text-muted">
              Reviews run on roll&apos;s hosted models and are billed per review. Bring your own key to pay your provider directly plus a small platform fee.
            </p>
            <button type="button" onClick={() => setEditing(true)} className={btn}>
              <KeyRound className="size-3.5" /> Use your own key
            </button>
          </div>
        ) : (
          <form
            className="rise space-y-4"
            onSubmit={async (e) => {
              e.preventDefault();
              if (!valid) return;
              const out = await run("provider", () =>
                codeApi.setReviewProvider(owner, {
                  provider,
                  api_key: key.trim(),
                  base_url: baseUrl.trim() || undefined,
                  model: model.trim() || undefined,
                  secondary_model: secondary.trim() || undefined,
                }),
              );
              if (out !== undefined) {
                setKey("");
                setEditing(false);
                setVersion((v) => v + 1);
              }
            }}
          >
            <Segmented
              value={provider}
              options={[
                { label: "Anthropic", value: "anthropic" },
                { label: "OpenAI-compatible", value: "openai_compatible" },
              ]}
              onChange={setProvider}
            />
            <div className="grid gap-4 sm:grid-cols-2">
              <Field label="API key" hint="Encrypted at rest; never shown again">
                <input type="password" autoComplete="off" value={key} onChange={(e) => setKey(e.target.value)} spellCheck={false} className={`${input} w-full font-mono text-[12px]`} />
              </Field>
              {compat && (
                <Field label="Base URL" hint="OpenAI, OpenRouter or your own endpoint">
                  <input value={baseUrl} onChange={(e) => setBaseUrl(e.target.value)} spellCheck={false} placeholder="https://openrouter.ai/api/v1" className={`${input} w-full font-mono text-[12px]`} />
                </Field>
              )}
              <Field label="Model" hint={compat ? "Required" : "Empty for roll's default"}>
                <input value={model} onChange={(e) => setModel(e.target.value)} spellCheck={false} className={`${input} w-full font-mono text-[12px]`} />
              </Field>
              <Field label="Fast and classifier model" hint="Optional">
                <input value={secondary} onChange={(e) => setSecondary(e.target.value)} spellCheck={false} className={`${input} w-full font-mono text-[12px]`} />
              </Field>
            </div>
            <div className="flex items-center gap-2">
              <button type="submit" disabled={!valid || busy !== null} className={primary}>
                {busy === "provider" ? <span className="shimmer">Saving…</span> : "Save key"}
              </button>
              <button type="button" onClick={() => (setEditing(false), setKey(""))} className="text-[12px] text-subtle hover:text-fg">
                Cancel
              </button>
            </div>
          </form>
        )}
      </div>
    </Panel>
  );
}

/** roll's reviewer rates, hosted and with your own key, beside what Greptile and CodeRabbit charge. */
export function PricingPanel() {
  const [pricing, setPricing] = useState<Pricing | null>(null);
  useEffect(() => {
    let alive = true;
    codeApi.pricing().then(
      (p) => alive && setPricing(p),
      () => {},
    );
    return () => {
      alive = false;
    };
  }, []);
  if (!pricing) return null;
  return (
    <Panel title="Pricing" aside={<span className="text-[11px] text-subtle">{pricing.currency}</span>}>
      <div className="space-y-5 overflow-x-auto px-5 py-4 text-[12px]">
        <table className="w-full">
          <thead className="text-left text-[11px] text-subtle">
            <tr className="border-b border-line">
              <th className="py-1.5 pr-3 font-normal">roll review</th>
              <th className="py-1.5 pr-3 font-normal">Hosted</th>
              <th className="py-1.5 font-normal">Your key</th>
            </tr>
          </thead>
          <tbody>
            {pricingRows(pricing).map((r) => (
              <tr key={r.label} className="border-b border-line last:border-0">
                <td className="py-1.5 pr-3">{r.label}</td>
                <td className="py-1.5 pr-3 font-mono text-muted">{r.hosted}</td>
                <td className="py-1.5 font-mono text-muted">{r.byok}</td>
              </tr>
            ))}
          </tbody>
        </table>
        <p className="text-[11px] text-subtle">
          A person never pays more than {formatUsd(pricing.human_monthly_cap.standard)} a month for hosted standard reviews; agents pay per review. Public repos get{" "}
          {pricing.public_repos.free_standard_reviews_per_month} standard reviews a month free.
        </p>
        <table className="w-full">
          <thead className="text-left text-[11px] text-subtle">
            <tr className="border-b border-line">
              <th className="py-1.5 pr-3 font-normal">For reference</th>
              <th className="py-1.5 pr-3 font-normal">Seats</th>
              <th className="py-1.5 font-normal">Usage</th>
            </tr>
          </thead>
          <tbody>
            {pricing.competitors.map(competitorRow).map((c) => (
              <tr key={c.name} className="border-b border-line last:border-0 text-muted">
                <td className="py-1.5 pr-3">{c.name}</td>
                <td className="py-1.5 pr-3 font-mono">{c.seat}</td>
                <td className="py-1.5">{c.usage}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </Panel>
  );
}
