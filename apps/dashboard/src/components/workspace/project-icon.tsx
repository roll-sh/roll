import { ImageUp, Trash2 } from "lucide-react";
import { useRef, useState } from "react";
import type { IdentityColor } from "@/lib/agents-api";
import { monogram, seedColor } from "@/lib/agents-identity";
import { ICON_IMAGE_MAX, type WorkspaceIcon, type WorkspaceProject } from "@/lib/workspace-api";
import { ColorPicker, Monogram, tint } from "../agents/identity";

/** The colour a project shows in: its icon's, else one made from its id (stable across renames). */
export const iconColor = (p: Pick<WorkspaceProject, "id" | "icon">): IdentityColor => p.icon?.color ?? seedColor(p.id);

/**
 * A project's icon, as the sidebar, Home, the top bar and the settings show it: its picture, its
 * emoji or its letters on its colour. Sized by `className`.
 */
export function ProjectIcon({ project, className = "size-6" }: { project: Pick<WorkspaceProject, "id" | "name" | "icon">; className?: string }) {
  const icon = project.icon;
  const color = iconColor(project);
  if (icon?.kind === "image")
    return (
      <span aria-hidden className={`inline-flex shrink-0 overflow-hidden rounded-[25%] shadow-[0_0_0_1px_var(--line-2)] ${className}`}>
        {/* A data: URL the API checked is PNG, JPEG or WebP; an <img> never runs script anyway. */}
        <img src={icon.url} alt="" draggable={false} className="size-full object-cover" />
      </span>
    );
  if (icon?.kind === "emoji")
    return (
      <span aria-hidden className={`inline-flex shrink-0 items-center justify-center rounded-[25%] bg-(--tint)/18 leading-none [container-type:size] ${className}`} style={tint(color)}>
        <span className="text-[length:64cqh] leading-none">{icon.emoji}</span>
      </span>
    );
  return <Monogram text={(icon?.kind === "monogram" && icon.text) || monogram(project.name)} color={color} className={className} />;
}

const EMOJI = ["🚀", "🛒", "📦", "🧾", "📊", "🧠", "🎮", "🎧", "📮", "🔐", "🌍", "⚡️", "🧪", "🛰️", "🌱", "🐙"];

/** Pick how a project looks: letters, an emoji or a picture, and its colour. */
export function IconEditor({
  project,
  value,
  onChange,
}: {
  project: Pick<WorkspaceProject, "id" | "name">;
  value: WorkspaceIcon | null;
  onChange: (icon: WorkspaceIcon | null) => void;
}) {
  const file = useRef<HTMLInputElement>(null);
  const [error, setError] = useState<string | null>(null);
  const kind = value?.kind ?? "monogram";
  const color = value?.color;
  const set = (next: WorkspaceIcon) => {
    setError(null);
    onChange(next);
  };
  const tab = (k: WorkspaceIcon["kind"], label: string) => (
    <button
      type="button"
      onClick={() =>
        k === "image" ? file.current?.click() : set(k === "emoji" ? { kind: "emoji", emoji: value?.kind === "emoji" ? value.emoji : "🚀", color } : { kind: "monogram", color })
      }
      className={`h-7 rounded-md px-2.5 text-[12px] transition-colors ${kind === k ? "bg-raised text-fg shadow-[0_0_0_1px_var(--line-2)] inset-shadow-highlight" : "text-muted hover:text-fg"}`}
    >
      {label}
    </button>
  );

  return (
    <div className="flex gap-4">
      <ProjectIcon project={{ ...project, icon: value }} className="size-16 text-[28px]" />
      <div className="min-w-0 flex-1 space-y-2.5">
        <div className="flex items-center gap-1 rounded-lg border border-line-2 bg-canvas p-0.5 w-fit">
          {tab("monogram", "Letters")}
          {tab("emoji", "Emoji")}
          {tab("image", "Picture")}
        </div>
        {kind === "monogram" && (
          <input
            aria-label="Letters"
            placeholder={monogram(project.name)}
            maxLength={3}
            value={value?.kind === "monogram" ? (value.text ?? "") : ""}
            onChange={(e) => set({ kind: "monogram", text: e.target.value.trim() || undefined, color })}
            className="h-8 w-20 rounded-lg border border-line-2 bg-canvas px-2.5 text-center font-mono text-[12.5px] uppercase outline-none focus:border-accent/60"
          />
        )}
        {kind === "emoji" && (
          <div className="flex flex-wrap gap-1">
            {EMOJI.map((e) => (
              <button
                key={e}
                type="button"
                onClick={() => set({ kind: "emoji", emoji: e, color })}
                className={`grid size-7 place-items-center rounded-md text-[15px] transition-colors hover:bg-white/[0.06] ${value?.kind === "emoji" && value.emoji === e ? "bg-white/[0.08] shadow-[0_0_0_1px_var(--line-3)]" : ""}`}
              >
                {e}
              </button>
            ))}
            <input
              aria-label="Any emoji"
              placeholder="any"
              value={value?.kind === "emoji" && !EMOJI.includes(value.emoji) ? value.emoji : ""}
              onChange={(e) => e.target.value.trim() && set({ kind: "emoji", emoji: Array.from(e.target.value.trim()).slice(0, 2).join(""), color })}
              className="h-7 w-14 rounded-md border border-line-2 bg-canvas text-center text-[13px] outline-none focus:border-accent/60"
            />
          </div>
        )}
        {kind === "image" && value?.kind === "image" && (
          <div className="flex gap-2">
            <button type="button" onClick={() => file.current?.click()} className="flex h-7 items-center gap-1.5 rounded-md border border-line-2 bg-raised px-2 text-[12px] text-muted inset-shadow-highlight hover:text-fg">
              <ImageUp className="size-3.5" /> Replace
            </button>
            <button type="button" onClick={() => set({ kind: "monogram", color })} className="flex h-7 items-center gap-1.5 rounded-md px-2 text-[12px] text-muted hover:text-fg">
              <Trash2 className="size-3.5" /> Remove
            </button>
          </div>
        )}
        {kind !== "image" && <ColorPicker value={color} auto={seedColor(project.id || project.name)} onChange={(c) => set({ ...(value ?? { kind: "monogram" }), color: c } as WorkspaceIcon)} />}
        {error && <p className="text-[11.5px] text-danger">{error}</p>}
        <input
          ref={file}
          type="file"
          accept="image/png,image/jpeg,image/webp"
          className="hidden"
          onChange={async (e) => {
            const f = e.target.files?.[0];
            e.target.value = "";
            if (!f) return;
            try {
              set({ kind: "image", url: await shrink(f) });
            } catch (err) {
              setError(err instanceof Error ? err.message : String(err));
            }
          }}
        />
      </div>
    </div>
  );
}

/** A picture as a square WebP `data:` URL no bigger than the API takes (256px is plenty for an icon). */
async function shrink(f: File): Promise<string> {
  if (!/^image\/(png|jpeg|webp)$/.test(f.type)) throw new Error("A PNG, JPEG or WebP picture");
  const bitmap = await createImageBitmap(f);
  const side = Math.min(bitmap.width, bitmap.height);
  const canvas = document.createElement("canvas");
  canvas.width = canvas.height = Math.min(256, side);
  const ctx = canvas.getContext("2d");
  if (!ctx) throw new Error("This browser can't read the picture");
  // The middle square, scaled down.
  ctx.drawImage(bitmap, (bitmap.width - side) / 2, (bitmap.height - side) / 2, side, side, 0, 0, canvas.width, canvas.height);
  for (const q of [0.9, 0.75, 0.6]) {
    const url = canvas.toDataURL("image/webp", q);
    if (url.startsWith("data:image/webp") && url.length <= ICON_IMAGE_MAX) return url;
  }
  const png = canvas.toDataURL("image/png");
  if (png.length <= ICON_IMAGE_MAX) return png;
  throw new Error("That picture is too big even made small");
}
