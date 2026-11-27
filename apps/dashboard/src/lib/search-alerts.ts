/**
 * roll search's health alerts (`/v1/search/clusters/{id}/alerts`, docs/search/README.md, "Health
 * alerts"): rules over a cluster's request metrics and this month's cost, kept by roll, and what
 * the Performance tab says about them.
 */

import { formatLatency, formatRate } from "./search-performance";

export type AlertMetric = "p99_ms" | "p95_ms" | "error_rate" | "throttle_rate" | "request_rate" | "month_cost_usd";

export type AlertTarget = { type: "webhook"; url: string } | { type: "slack" | "discord"; install: string; channel: string };

export interface AlertRule {
  id: string;
  name: string;
  metric: AlertMetric;
  op: ">" | ">=" | "<" | "<=";
  threshold: number;
  /** `5m`, `2h`; null for `month_cost_usd` (the month so far). */
  window: string | null;
  /** A webhook shows its origin only, never its path or secret. */
  targets: AlertTarget[];
  state: "ok" | "firing";
  /** Unix seconds of the last change (the creation at first). */
  since: number;
  /** The last evaluation's value; null when the window had nothing to measure. */
  value: number | null;
  evaluated_at: number | null;
  created_at: number;
}

export interface AlertDelivery {
  id: string;
  /** The target's place in the rule's `targets`. */
  target: number;
  event: "search.alert.firing" | "search.alert.resolved" | "search.alert.test";
  status: "pending" | "delivered" | "failed";
  attempts: number;
  /** Unix seconds of the next try (pending only). */
  next_at: number | null;
  last_status: number | null;
  last_error: string | null;
  created_at: number;
  delivered_at: number | null;
}

/** Which Performance chart a metric belongs beside (none for the month's cost). */
export type AlertChart = "rate" | "latency" | "errors" | "rejections";

export function chartOf(m: AlertMetric): AlertChart | null {
  switch (m) {
    case "request_rate":
      return "rate";
    case "p99_ms":
    case "p95_ms":
      return "latency";
    case "error_rate":
      return "errors";
    case "throttle_rate":
      return "rejections";
    default:
      return null;
  }
}

/** A metric's value as its chart shows it. */
export function formatAlertValue(m: AlertMetric, v: number | null): string {
  if (v == null) return "—";
  switch (m) {
    case "p99_ms":
    case "p95_ms":
      return formatLatency(v);
    case "error_rate":
    case "throttle_rate":
      return `${v < 0.1 && v > 0 ? v.toFixed(2) : v.toFixed(1)}%`;
    case "request_rate":
      return `${formatRate(v)}/s`;
    case "month_cost_usd":
      return `$${v.toFixed(2)}`;
  }
}

const LABELS: Record<AlertMetric, string> = {
  p99_ms: "p99",
  p95_ms: "p95",
  error_rate: "5xx",
  throttle_rate: "429s",
  request_rate: "requests",
  month_cost_usd: "this month's cost",
};

/** `p99 > 500 ms over 5m`, `this month's cost > $50.00`. */
export function ruleText(r: Pick<AlertRule, "metric" | "op" | "threshold" | "window">): string {
  const over = r.window ? ` over ${r.window}` : "";
  return `${LABELS[r.metric]} ${r.op} ${formatAlertValue(r.metric, r.threshold)}${over}`;
}
