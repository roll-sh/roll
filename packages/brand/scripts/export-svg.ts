// Writes static SVG assets for the mark. Run: pnpm --filter @roll/brand export
import { mkdirSync, writeFileSync } from "node:fs";
import { avatarSvg, horizonSvg, markSvg } from "../src/geometry.ts";

const out = new URL("../assets/", import.meta.url);
const icon = new URL("../../../apps/dashboard/src/app/icon.svg", import.meta.url);
const accent = "#4c8dff";

const files: Record<string, string> = {
  "mark-light.svg": markSvg({ color: "#fafafa" }),
  "mark-dark.svg": markSvg({ color: "#0a0a0a" }),
  "mark-accent-light.svg": markSvg({ color: "#fafafa", accent: accent }),
  "mark-accent-dark.svg": markSvg({ color: "#0a0a0a", accent: accent }),
  "mark-fine-light.svg": markSvg({ color: "#fafafa", step: 0.14, accent: accent }),
  "horizon-light.svg": horizonSvg({ color: "#fafafa" }),
  "horizon-dark.svg": horizonSvg({ color: "#0a0a0a" }),
  "avatar-dark.svg": avatarSvg(),
  "avatar-light.svg": avatarSvg({ background: "#fafafa", color: "#0a0a0a" }),
  "avatar-blue.svg": avatarSvg({ background: "#2f6fe8", color: "#fafafa" }),
};
const publicDir = new URL("../../../apps/dashboard/public/brand/", import.meta.url);
mkdirSync(publicDir, { recursive: true });
for (const [name, svg] of Object.entries(files)) {
  writeFileSync(new URL(name, out), svg + "\n");
  writeFileSync(new URL(name, publicDir), svg + "\n");
}

// Favicon: the Horizon logo (same as the profile picture) on a rounded dark tile.
const inner = horizonSvg().replace(/^<svg[^>]*>|<\/svg>$/g, "");
writeFileSync(
  icon,
  `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100"><rect width="100" height="100" rx="24" fill="#0a0a0a"/><g fill="#fafafa" transform="translate(50 50) scale(.64) translate(-50 -49.5)">${inner}</g></svg>\n`,
);
console.log(`wrote ${Object.keys(files).length} assets + favicon`);
