"use client";

/**
 * A provider's logo (T3 Code's `ProviderInstanceIcon`, MIT): its mark from `provider-logos.tsx`, or
 * its initial when it has none (T3's initials badge). One colour, the text's, so it reads on every
 * surface and never competes with the accent.
 */

import type { ProviderInfo } from "@/lib/agents-api";
import type { Glyph } from "./brand-icons";
import { ClaudeLogo, CursorLogo, FxLogo, GeminiLogo, GrokLogo, KiroLogo, OpenAILogo, OpenCodeLogo } from "./provider-logos";
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
};

export const providerLogo = (id: string): Glyph | undefined => LOGOS[id];

/** The provider's mark, `size-4` by default; a monogram for one without. */
export function ProviderLogo({ provider, className = "size-4" }: { provider: Pick<ProviderInfo, "id" | "name">; className?: string }) {
  const Logo = LOGOS[provider.id];
  if (Logo) return <Logo className={cn("shrink-0", className)} aria-hidden />;
  const name = provider.name || provider.id.replace(/^acp:/, "");
  return (
    <span aria-hidden className={cn("grid shrink-0 place-items-center rounded-[4px] bg-fg/[0.08] font-mono text-[9px] leading-none font-semibold text-muted uppercase", className)}>
      {name.slice(0, 1)}
    </span>
  );
}
