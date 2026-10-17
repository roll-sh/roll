export const colors = {
  ink: "#0a0a0a",
  paper: "#fafafa",
  /** Used sparingly: highlights, the "your app, live" node, the cursor glow. */
  accent: "#4c8dff",
  danger: "#ff3b30",
} as const;

/**
 * Mark density presets (grid step in unit space). All read as the same round globe;
 * fewer nodes at small sizes so dots don't blur together.
 */
export const densities = {
  bold: 0.25,
  small: 0.22,
  regular: 0.18,
  fine: 0.14,
} as const;

export type Density = keyof typeof densities;

export function densityForSize(px: number): Density {
  if (px < 24) return "bold";
  if (px < 64) return "small";
  if (px < 200) return "regular";
  return "fine";
}
