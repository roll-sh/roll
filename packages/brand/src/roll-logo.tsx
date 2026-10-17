"use client";

import type { CSSProperties } from "react";
import type { MarkState } from "./engine";
import { HorizonMark } from "./horizon-mark";

export interface RollLogoProps {
  state?: MarkState;
  /** Wordmark font size in px; the mark scales with it. */
  size?: number;
  interactive?: boolean;
  accent?: string | null;
  className?: string;
  style?: CSSProperties;
}

/**
 * The roll logo: Horizon mark + "roll" wordmark (same mark as the profile picture).
 * The wordmark inherits the host font (Geist in the dashboard).
 */
export function RollLogo({ state = "idle", size = 20, interactive = true, accent, className, style }: RollLogoProps) {
  return (
    <span
      className={className}
      style={{ display: "inline-flex", alignItems: "center", gap: size * 0.34, lineHeight: 1, ...style }}
    >
      <HorizonMark state={state} size={Math.round(size * 1.5)} interactive={interactive} accent={accent ?? undefined} label="" />
      <span style={{ fontSize: size, fontWeight: 600, letterSpacing: "-0.05em" }}>roll</span>
    </span>
  );
}
