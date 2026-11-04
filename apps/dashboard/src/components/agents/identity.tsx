/**
 * Projects and machines as roll shows them everywhere (docs/code/agents-design.md, "Identity"):
 * a project's logo, else its monogram (or emoji) on its colour; a machine's kind on its colour.
 * The canvas cards, the threads sidebar, ⌘K, the launcher, the conversation's breadcrumb and the
 * automations list all use these, so a project looks the same wherever it shows.
 *
 * Ported from T3 Code (MIT): `apps/web/src/components/ProjectFavicon.tsx` (a logo that fails to
 * load falls back to the monogram) and `ProjectMonogram.tsx` (the SVG tile).
 */

import { Check, Cloud, Container, Laptop, Monitor, Server, type LucideIcon } from "lucide-react";
import { useState, type CSSProperties } from "react";
import { IDENTITY_COLORS, type IdentityColor, type Machine, type MachineKind, type Project } from "@/lib/agents-api";
import { COLOR_LABEL, colorVar, KIND_LABEL, machineColor, machineKind, MACHINE_KINDS, monogram, projectFace } from "@/lib/agents-identity";

/** Sets `--tint` for an element and its children (`text-(--tint)`, `bg-(--tint)/15`…). */
export function tint(c: IdentityColor): CSSProperties {
  return { "--tint": colorVar(c) } as CSSProperties;
}

type ProjectLike = Pick<Project, "name" | "repo" | "settings" | "logo">;

/** A project's tile: its logo, else its monogram or emoji on its colour. Sized by `className`. */
export function ProjectIcon({ project, className = "size-4" }: { project: ProjectLike; className?: string }) {
  const face = projectFace(project);
  // A logo that won't load (a broken file) shows the monogram, as T3's favicon does.
  const [broken, setBroken] = useState<string | null>(null);
  if (face.kind === "logo" && broken !== face.url) {
    return (
      <span aria-hidden data-identity="logo" className={`inline-flex shrink-0 items-center justify-center ${className}`} style={tint(face.color)}>
        {/* A data: URL from the record; an <img> never runs an SVG's scripts */}
        <img src={face.url} alt="" draggable={false} onError={() => setBroken(face.url)} className="size-full rounded-[25%] object-contain shadow-[0_0_0_1px_var(--line-2)]" />
      </span>
    );
  }
  if (face.kind === "emoji") {
    return (
      <span aria-hidden data-identity="emoji" className={`inline-flex shrink-0 items-center justify-center rounded-[25%] bg-(--tint)/18 leading-none [container-type:size] ${className}`} style={tint(face.color)}>
        <span className="text-[length:68cqh] leading-none">{face.emoji}</span>
      </span>
    );
  }
  const text = face.kind === "monogram" ? face.text : monogram(project.name);
  return <Monogram text={text} color={face.color} className={className} />;
}

/** T3's monogram tile: the letters in the colour, on the colour at 18%. */
export function Monogram({ text, color, className = "size-4" }: { text: string; color: IdentityColor; className?: string }) {
  const n = Array.from(text).length;
  return (
    <span aria-hidden data-identity="monogram" data-color={color} className={`inline-flex shrink-0 items-center justify-center ${className}`} style={tint(color)}>
      <svg viewBox="0 0 16 16" className="size-full overflow-hidden rounded-[25%] text-(--tint) select-none" style={{ backgroundColor: "color-mix(in srgb, var(--tint) 18%, transparent)" }}>
        <text
          x="8"
          y="10.8"
          textAnchor="middle"
          fill="currentColor"
          className="font-mono"
          fontSize={n > 2 ? "6.5" : "8.25"}
          fontWeight="700"
          textLength={n === 1 ? 6 : 12}
          lengthAdjust="spacingAndGlyphs"
          textRendering="geometricPrecision"
        >
          {text}
        </text>
      </svg>
    </span>
  );
}

const KIND_ICON: Record<MachineKind, LucideIcon> = { laptop: Laptop, desktop: Monitor, server: Server, cloud: Cloud, container: Container };

export function KindIcon({ kind, className = "size-4" }: { kind: MachineKind; className?: string }) {
  const Icon = KIND_ICON[kind];
  return <Icon className={className} aria-hidden />;
}

type MachineLike = Pick<Machine, "id" | "appearance" | "sandbox" | "info">;

/**
 * A machine's kind in its colour. `tile` puts it on its colour at 16% in a rounded square (cards,
 * settings); without, it's the glyph alone (meta lines: "on ada-mbp").
 */
export function MachineIcon({ machine, className = "size-4", tile = false }: { machine: MachineLike; className?: string; tile?: boolean }) {
  const color = machineColor(machine);
  const kind = machineKind(machine);
  if (!tile) {
    return (
      <span aria-hidden data-identity="machine" data-color={color} className="inline-flex shrink-0 text-(--tint)" style={tint(color)}>
        <KindIcon kind={kind} className={className} />
      </span>
    );
  }
  return (
    <span aria-hidden data-identity="machine" data-color={color} className={`grid shrink-0 place-items-center rounded-[25%] bg-(--tint)/16 text-(--tint) ${className}`} style={tint(color)}>
      <KindIcon kind={kind} className="size-[58%] stroke-[1.75]" />
    </span>
  );
}

/**
 * The colour picker (Linear's swatch row): "Auto" (the colour made from the project or machine,
 * shown in the swatch) and the palette. `value` undefined is Auto.
 */
export function ColorPicker({ value, auto, onChange, label = "Colour" }: { value: IdentityColor | undefined; auto: IdentityColor; onChange: (c: IdentityColor | undefined) => void; label?: string }) {
  const swatch = (c: IdentityColor | undefined, title: string) => {
    const shown = c ?? auto;
    const on = value === c;
    return (
      <button
        key={c ?? "auto"}
        type="button"
        role="radio"
        aria-checked={on}
        aria-label={title}
        title={title}
        onClick={() => onChange(c)}
        style={tint(shown)}
        className={`relative grid size-6 place-items-center rounded-full transition-[box-shadow,transform] duration-150 ease-out active:scale-[0.94] focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent/70 ${
          on ? "shadow-[0_0_0_2px_var(--sheet),0_0_0_3.5px_var(--tint)]" : "hover:shadow-[0_0_0_2px_var(--sheet),0_0_0_3px_var(--line-3)]"
        }`}
      >
        <span className={`size-4.5 rounded-full ${c ? "bg-(--tint)" : "border-[1.5px] border-dashed border-(--tint) bg-(--tint)/25"}`} />
        {on && <Check className="absolute size-3 stroke-[2.5] text-sheet" aria-hidden />}
      </button>
    );
  };
  return (
    <div role="radiogroup" aria-label={label} className="flex flex-wrap items-center gap-1.5">
      {swatch(undefined, `Auto (${COLOR_LABEL[auto]})`)}
      <span aria-hidden className="mx-0.5 h-4 w-px bg-line-2" />
      {IDENTITY_COLORS.map((c) => swatch(c, COLOR_LABEL[c]))}
    </div>
  );
}

/** A machine's kind: Auto (from its OS, or cloud for a sandbox) and the kinds, as icon buttons. */
export function KindPicker({ value, auto, color, onChange }: { value: MachineKind | undefined; auto: MachineKind; color: IdentityColor; onChange: (k: MachineKind | undefined) => void }) {
  const option = (k: MachineKind | undefined) => {
    const on = value === k;
    const shown = k ?? auto;
    const title = k ? KIND_LABEL[k] : `Auto (${KIND_LABEL[auto]})`;
    return (
      <button
        key={k ?? "auto"}
        type="button"
        role="radio"
        aria-checked={on}
        aria-label={title}
        title={title}
        onClick={() => onChange(k)}
        style={tint(color)}
        className={`flex h-7 items-center gap-1.5 rounded-md border px-2 text-[12px] transition-colors duration-150 ${
          on ? "border-line-3 bg-fg/[0.07] text-fg inset-shadow-highlight" : "border-line text-muted hover:bg-fg/[0.04] hover:text-fg"
        }`}
      >
        <KindIcon kind={shown} className={`size-3.5 stroke-[1.75] ${on ? "text-(--tint)" : ""}`} />
        {k ? KIND_LABEL[k] : "Auto"}
      </button>
    );
  };
  return (
    <div role="radiogroup" aria-label="Kind" className="flex flex-wrap gap-1">
      {option(undefined)}
      {MACHINE_KINDS.map(option)}
    </div>
  );
}
