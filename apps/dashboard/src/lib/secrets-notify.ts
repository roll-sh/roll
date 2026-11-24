import type { NewNotification, NotificationChannel, SecretsNotification } from "@roll/secrets/types";
import type { ChatInstall, ChatPlatform } from "./integrations-api";
import { FAMILIES, kindsOf, type Family } from "./secrets-events";

/** Where the "Notify me" menu sends: the person's own address, or a channel through one of their chat installs. */
export type NotifyTarget = { kind: "email" } | { kind: ChatPlatform; install: string; channel: string };

/** What the menu holds: the kind chips, the environment (none: every one) and the channel. */
export type NotifyChoice = { families: Family[]; environment?: string; target: NotifyTarget };

/** A platform's channel id (Slack `C0123ABCD`, a Discord snowflake), as the server takes it. */
export const isChannelId = (s: string) => /^[A-Za-z0-9]{1,40}$/.test(s);

/** Why the menu can't send yet, or null. */
export function notifyProblem(c: NotifyChoice): string | null {
  if (c.target.kind === "email") return null;
  if (!c.target.install) return `Pick a ${c.target.kind === "slack" ? "Slack workspace" : "Discord server"}.`;
  if (!isChannelId(c.target.channel.trim())) return "The channel is its id (letters and digits, like C0123ABCD).";
  return null;
}

/** The `POST …/notifications` body for what the menu holds: the chips' `kinds` (none: every kind), the environment, the channel. */
export function notifyBody(c: NotifyChoice): NewNotification {
  const channel: NotificationChannel =
    c.target.kind === "email"
      ? { email: "own" }
      : c.target.kind === "slack"
        ? { slack: { install: c.target.install, channel: c.target.channel.trim() } }
        : { discord: { install: c.target.install, channel: c.target.channel.trim() } };
  const kinds = kindsOf(c.families);
  return { ...(kinds && { kinds }), ...(c.environment && { environment: c.environment }), channel };
}

/** What a subscription asks for, in the chips' words where they match ("Secrets, Leaks & decoys"), else its kinds. */
export function kindsLabel(kinds: string[]): string {
  if (!kinds.length) return "Everything";
  const left = new Set(kinds);
  const names: string[] = [];
  for (const f of FAMILIES) {
    if (f.kinds.every((k) => left.has(k))) {
      names.push(f.label);
      f.kinds.forEach((k) => left.delete(k));
    }
  }
  return [...names, ...left].join(", ");
}

/** Where a subscription goes, for its row: "Email", "Slack · Acme #C0123". */
export function channelLabel(n: Pick<SecretsNotification, "channel">, installs: ChatInstall[]): string {
  const ch = n.channel;
  if ("email" in ch) return "Email";
  const [platform, c] = "slack" in ch ? (["Slack", ch.slack] as const) : (["Discord", ch.discord] as const);
  const team = installs.find((i) => i.id === c.install)?.team_name ?? c.install;
  return `${platform} · ${team} #${c.channel}`;
}
