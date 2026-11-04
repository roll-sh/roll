import { Resvg } from "@resvg/resvg-js";
import { horizonSvg } from "@roll/brand/geometry";
import { colors } from "@roll/brand/tokens";
import { createFileRoute } from "@tanstack/react-router";
import satori from "satori";
import geistTtf from "@/assets/Geist-Regular.ttf?inline";
import { fetchShare, OG_IMAGE, shareCard, type ShareCard } from "@/lib/agents-server";

// The dashboard's dark palette (globals.css), spelled out: the image renderer has no CSS variables.
const BG = "#000000";
const FG = "#ededed";
const MUTED = "#a1a1a1";
const SUBTLE = "#6f6f6f";
const LINE = "#1f1f1f";
const DOT: Record<ShareCard["marks"][number], string> = { text: "#3b3b3b", tool: "#6f6f6f", ask: "#a1a1a1", agent: "#d4d4d4" };
const OS: Record<string, string> = { macos: "macOS", linux: "Linux", windows: "Windows" };

const logo = `data:image/svg+xml;base64,${Buffer.from(horizonSvg({ color: FG, size: 96 })).toString("base64")}`;
/** Geist Regular, the face Next's image renderer drew these with. */
const font = Buffer.from(geistTtf.slice(geistTtf.indexOf(",") + 1), "base64");

/** The link preview: Horizon, the thread's title, who ran it where, and a dot per step. */
async function render(c: ShareCard | null) {
  const title = c?.title ?? "A shared thread";
  const line = c
    ? [c.owner, `${c.machine}${c.os ? ` (${OS[c.os] ?? c.os})` : ""}`, [c.providers.join(" + "), c.model].filter(Boolean).join(" · "), `${c.runs} run${c.runs === 1 ? "" : "s"}`, `${c.steps} step${c.steps === 1 ? "" : "s"}`]
        .filter(Boolean)
        .join("  ·  ")
    : "roll agents";
  const marks = c?.marks ?? [];
  const svg = await satori(
    <div style={{ width: "100%", height: "100%", display: "flex", flexDirection: "column", background: BG, color: FG, padding: "64px 72px", fontFamily: "sans-serif" }}>
      <div style={{ display: "flex", alignItems: "center", gap: 18 }}>
        <img src={logo} width={52} height={52} alt="" />
        <span style={{ fontSize: 40, fontWeight: 600, letterSpacing: "-0.05em" }}>roll</span>
        <span style={{ fontSize: 28, color: SUBTLE, marginLeft: 6 }}>agents · shared thread</span>
      </div>
      <div style={{ display: "flex", flexDirection: "column", marginTop: 72, flex: 1 }}>
        <div style={{ display: "flex", fontSize: title.length > 60 ? 52 : 64, fontWeight: 600, letterSpacing: "-0.045em", lineHeight: 1.08, maxHeight: 150, overflow: "hidden" }}>{title}</div>
        <div style={{ display: "flex", marginTop: 28, fontSize: 26, color: MUTED }}>{line}</div>
      </div>
      <div style={{ display: "flex", alignItems: "center", borderTop: `1px solid ${LINE}`, paddingTop: 28 }}>
        <div style={{ display: "flex", gap: 10, flex: 1 }}>
          {marks.map((m, i) => (
            <div key={i} style={{ width: 14, height: 14, borderRadius: 7, background: i === marks.length - 1 ? colors.accent : DOT[m] }} />
          ))}
        </div>
        <span style={{ fontSize: 24, color: SUBTLE }}>keep the code rolling</span>
      </div>
    </div>,
    { width: OG_IMAGE.width, height: OG_IMAGE.height, fonts: [{ name: "sans serif", data: font, weight: 400, style: "normal" }] },
  );
  return new Resvg(svg, { fitTo: { mode: "width", value: OG_IMAGE.width } }).render().asPng();
}

/** `og:image` of a share page. An unknown share still gets a generic card, as a crawler may ask late. */
export const Route = createFileRoute("/agents/s/$id/opengraph-image")({
  server: {
    handlers: {
      GET: async ({ params: { id } }) => {
        const share = await fetchShare(id).catch(() => null);
        const png = await render(share && shareCard(share));
        return new Response(new Uint8Array(png), {
          headers: { "content-type": OG_IMAGE.type, "cache-control": "public, immutable, no-transform, max-age=31536000" },
        });
      },
    },
  },
});
