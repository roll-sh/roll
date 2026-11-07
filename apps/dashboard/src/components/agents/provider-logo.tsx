"use client";

/**
 * A provider's logo (T3 Code's `ProviderInstanceIcon`, MIT): its mark from `provider-logos.tsx`, or
 * its initial when it has none (T3's initials badge). One colour, the text's, so it reads on every
 * surface and never competes with the accent.
 */

import type { ProviderInfo } from "@/lib/agents-api";
import type { Glyph } from "./brand-icons";
import { AntigravityLogo, ClaudeLogo, CursorLogo, FxLogo, GeminiLogo, GrokLogo, KiroLogo, MuseLogo, OpenAILogo, OpenCodeLogo, PiLogo } from "./provider-logos";
import { cn } from "./ui";

/** Marks by provider id: the built-in adapters and the ACP agents roll knows (`acp::known_agents`). */
const LOGOS: Record<string, Glyph> = {
  codex: OpenAILogo,
  claude: ClaudeLogo,
  "acp:codex-acp": OpenAILogo,
  "acp:claude-acp": ClaudeLogo,
  "acp:gemini": GeminiLogo,
  "acp:cursor": CursorLogo,
  "acp:grok": GrokLogo,
  "acp:opencode": OpenCodeLogo,
  "acp:kiro": KiroLogo,
  "acp:fx": FxLogo,
  // Native drivers (T3's OpenCode, Pi and Muse packages) and their ACP or registry flavours.
  opencode: OpenCodeLogo,
  pi: PiLogo,
  "acp:pi-acp": PiLogo,
  muse: MuseLogo,
  "acp:antigravity": AntigravityLogo,
  "acp:antigravity-acp": AntigravityLogo,
  "acp:cursor-agent": CursorLogo,
  "acp:gemini-cli": GeminiLogo,
  "acp:grok-build": GrokLogo,
};

export const providerLogo = (id: string): Glyph | undefined => LOGOS[id];

/**
 * Two letters for a provider without a mark (T3's `providerInstanceInitials`): the first two words'
 * initials, or a lone word's first two letters, so "Fake echo" and "Fake fake" read FE and FF, not F
 * and F. Punctuation splits words ("Codex (hub)" is CH).
 */
export function providerInitials(label: string): string {
  const words = label
    .replace(/[^\p{L}\p{N}'’]+/gu, " ")
    .split(/\s+/u)
    .map((w) => w.replace(/^['’]+|['’]+$/gu, ""))
    .filter(Boolean);
  if (words.length === 0) return "";
  if (words.length === 1) return Array.from(words[0]).slice(0, 2).join("").toUpperCase();
  return words
    .slice(0, 2)
    .map((w) => Array.from(w)[0])
    .join("")
    .toUpperCase();
}

/** The provider's mark, `size-4` by default; its initials on a tile for one without. */
export function ProviderLogo({ provider, className = "size-4" }: { provider: Pick<ProviderInfo, "id" | "name">; className?: string }) {
  const Logo = LOGOS[provider.id];
  if (Logo) return <Logo className={cn("shrink-0", className)} aria-hidden />;
  return (
    <span aria-hidden className={cn("grid shrink-0 place-items-center rounded-[4px] bg-fg/[0.08] font-mono text-[7.5px] leading-none font-semibold tracking-[-0.04em] text-muted uppercase", className)}>
      {providerInitials(provider.name || provider.id.replace(/^acp:/, ""))}
    </span>
  );
}
