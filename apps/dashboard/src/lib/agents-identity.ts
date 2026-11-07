/**
 * What makes projects and machines recognisable at a glance (docs/code/agents.md, "Identity";
 * docs/code/agents-design.md, "Identity"): a colour each, a project's logo or monogram, a
 * machine's kind. Pure; `components/agents/identity.tsx` draws them.
 *
 * Ported from T3 Code (MIT): the monogram and the colour hash from `apps/web/src/projectIdentity.ts`,
 * the icon rules from `packages/contracts/src/project.ts`. roll's palette is its own (T3's has
 * orange, amber and yellow, which roll never uses, and blue, which on roll means busy).
 */

import { IDENTITY_COLORS, type IdentityColor, type Machine, type MachineKind, type Project, type ProjectIcon } from "./agents-api";

export const COLOR_LABEL: Record<IdentityColor, string> = {
  slate: "Slate",
  violet: "Violet",
  purple: "Purple",
  pink: "Pink",
  rose: "Rose",
  teal: "Teal",
  cyan: "Cyan",
  // The stored name is older than the colour: green read as done (`--id-green` in globals.css).
  green: "Stone",
};

/** The colour as CSS (the `--id-<name>` token: light and dark). */
export function colorVar(c: IdentityColor): string {
  return `var(--id-${c})`;
}

/** The token's name, for the canvas engine (which resolves custom properties itself). */
export function colorToken(c: IdentityColor): string {
  return `--id-${c}`;
}

/** T3's hash (`projectColor`): the same seed is the same colour on every device. */
export function seedColor(seed: string): IdentityColor {
  // Every card asks on every render: remember the answers (bounded).
  const known = seeded.get(seed);
  if (known) return known;
  const s = seed.normalize("NFKC").trim().toLocaleLowerCase("en-US") || "project";
  let i = 0;
  for (const g of s) i = (i * 31 + (g.codePointAt(0) ?? 0)) % IDENTITY_COLORS.length;
  const color = IDENTITY_COLORS[i] ?? "slate";
  if (seeded.size > 2000) seeded.clear();
  seeded.set(seed, color);
  return color;
}
const seeded = new Map<string, IdentityColor>();

/**
 * A project's colour: the one its owner picked, else one made from its repo (`owner/repo`) or its
 * name. Not from its id: the same repo checked out on a laptop and in a sandbox is two projects,
 * and should look like one.
 */
export function projectColor(p: Pick<Project, "name" | "repo" | "settings">): IdentityColor {
  return p.settings.color ?? seedColor(p.repo ? `${p.repo.owner}/${p.repo.repo}` : p.name);
}

/** A machine's colour: the one its owner picked, else one made from its id (it keeps it through a rename). */
export function machineColor(m: Pick<Machine, "id" | "appearance">): IdentityColor {
  return m.appearance.color ?? seedColor(m.id);
}

export const KIND_LABEL: Record<MachineKind, string> = {
  laptop: "Laptop",
  desktop: "Desktop",
  server: "Server",
  cloud: "Cloud",
  container: "Container",
};
export const MACHINE_KINDS = Object.keys(KIND_LABEL) as MachineKind[];

/** What a machine is: as its owner set it, else a sandbox is cloud, a Mac a laptop, Windows a desktop, the rest servers. */
export function machineKind(m: Pick<Machine, "appearance" | "sandbox" | "info">): MachineKind {
  if (m.appearance.kind) return m.appearance.kind;
  if (m.sandbox) return "cloud";
  const os = m.info?.os ?? "";
  return os === "macos" ? "laptop" : os === "windows" ? "desktop" : "server";
}

/** T3's monogram: the first letter and the first digit (or the last word's first letter), two at most. */
export function monogram(name: string): string {
  const words = name.normalize("NFKC").trim().match(/[\p{L}\p{N}]+/gu) ?? [];
  const first = words[0];
  if (!first) return "PR";
  const glyphs = Array.from(first);
  const a = glyphs[0] ?? "P";
  const b = glyphs.slice(1).find((g) => /\p{N}/u.test(g)) ?? (words.length > 1 ? Array.from(words.at(-1) ?? "")[0] : glyphs.at(-1)) ?? a;
  return Array.from(`${a}${b}`.toUpperCase()).slice(0, 2).join("");
}

/** What a project's tile shows. */
export type ProjectFace =
  | { kind: "logo"; url: string; color: IdentityColor }
  | { kind: "monogram"; text: string; color: IdentityColor }
  | { kind: "emoji"; emoji: string; color: IdentityColor };

export function projectFace(p: Pick<Project, "name" | "repo" | "settings" | "logo">): ProjectFace {
  const color = projectColor(p);
  const icon = p.settings.icon ?? { kind: "auto" };
  if (icon.kind === "emoji") return { kind: "emoji", emoji: icon.emoji, color };
  if (icon.kind === "monogram") return { kind: "monogram", text: icon.text ? icon.text.toUpperCase() : monogram(p.name), color };
  if (p.logo) return { kind: "logo", url: p.logo.url, color };
  return { kind: "monogram", text: monogram(p.name), color };
}

export const MAX_MONOGRAM = 3;
export const MAX_EMOJI = 32;
export const LOGO_EXTENSIONS = ["svg", "png", "ico", "gif", "webp", "jpg", "jpeg", "avif"];

/** The icon as the daemon will keep it, or why it won't (agent-proto's `ProjectIcon::check`). */
export function checkProjectIcon(icon: ProjectIcon): { icon: ProjectIcon } | { error: string } {
  const plain = (s: string) => !/[\s\p{Cc}]/u.test(s);
  switch (icon.kind) {
    case "auto":
      return { icon };
    case "monogram": {
      const text = icon.text?.trim() || undefined;
      if (text && (Array.from(text).length > MAX_MONOGRAM || !plain(text) || !/^[\p{L}\p{N}]/u.test(text))) {
        return { error: `a monogram is 1 to ${MAX_MONOGRAM} letters or digits` };
      }
      return { icon: text ? { kind: "monogram", text } : { kind: "monogram" } };
    }
    case "emoji": {
      const emoji = icon.emoji.trim();
      if (!emoji || new TextEncoder().encode(emoji).length > MAX_EMOJI || !plain(emoji) || /[\x00-\x7f]/.test(emoji)) return { error: "an emoji is one emoji" };
      return { icon: { kind: "emoji", emoji } };
    }
    case "file": {
      const path = icon.path.trim().replace(/^(\.\/)+/, "");
      const inside = !path.startsWith("/") && !path.includes("\\") && path.split("/").every((p) => p !== "" && p !== "..");
      if (!path || path.length > 1024 || !inside) return { error: "a logo's path is a file inside the project, like public/logo.svg" };
      const ext = path.includes(".") ? path.slice(path.lastIndexOf(".") + 1).toLowerCase() : "";
      if (!LOGO_EXTENSIONS.includes(ext)) return { error: `a logo is a picture: ${LOGO_EXTENSIONS.join(", ")}` };
      return { icon: { kind: "file", path } };
    }
  }
}

type Placed<T> = T & { machine: string };
interface PlaceRefs {
  project?: { type: "id"; id: string } | { type: "repo"; repo: string; path: string | null } | null;
  target?: { type: string; machine?: string } | null;
}

/**
 * Where an automation works, for its row in the list: the projects its defaults and steps name (by
 * id, or by repo when a project here is a checkout of it) and the machines they pin, each once, in
 * the order they first appear.
 */
export function automationPlaces<P extends Pick<Project, "id" | "repo">, M extends Pick<Machine, "id">>(
  spec: { defaults?: PlaceRefs | null; steps: readonly { config: unknown }[] },
  projects: readonly Placed<P>[],
  machines: readonly M[],
): { projects: Placed<P>[]; machines: M[] } {
  const refs: PlaceRefs[] = [spec.defaults ?? {}, ...spec.steps.map((s) => (s.config && typeof s.config === "object" ? (s.config as PlaceRefs) : {}))];
  const ps = new Map<string, Placed<P>>();
  const ms = new Map<string, M>();
  for (const r of refs) {
    const ref = r.project;
    const p = !ref ? undefined : ref.type === "id" ? projects.find((x) => x.id === ref.id) : projects.find((x) => x.repo && `${x.repo.owner}/${x.repo.repo}` === ref.repo);
    if (p) ps.set(p.id, p);
    const m = r.target?.type === "machine" ? machines.find((x) => x.id === r.target?.machine) : undefined;
    if (m) ms.set(m.id, m);
  }
  return { projects: [...ps.values()], machines: [...ms.values()] };
}
