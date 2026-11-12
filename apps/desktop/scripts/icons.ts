// Renders the app, tray and badge icons from @roll/brand's geometry into resources/icons/
// (PNG, plus ICNS for macOS and ICO for Windows). Never redraw the mark here: it comes from
// `horizonSvg`, exactly as the favicon does in packages/brand/scripts/export-svg.ts.
// Run: pnpm --filter @roll/desktop icons
import { Resvg } from "@resvg/resvg-js";
import { horizonSvg } from "@roll/brand/geometry";
import { colors } from "@roll/brand/tokens";
import { mkdirSync, writeFileSync } from "node:fs";

const out = new URL("../resources/icons/", import.meta.url);
mkdirSync(out, { recursive: true });

const inner = horizonSvg().replace(/^<svg[^>]*>|<\/svg>$/g, "");

/** The app icon: Horizon on a rounded ink tile, inset like a macOS icon (824 of 1024). */
const appSvg = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1024 1024"><rect x="100" y="100" width="824" height="824" rx="185" fill="${colors.ink}"/><g fill="${colors.paper}" transform="translate(512 512) scale(5.27) translate(-50 -49.5)">${inner}</g></svg>`;
/** The tray icon for Windows and Linux (panels may be light or dark): the favicon tile. */
const traySvg = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100"><rect width="100" height="100" rx="24" fill="${colors.ink}"/><g fill="${colors.paper}" transform="translate(50 50) scale(.72) translate(-50 -49.5)">${inner}</g></svg>`;
/** The macOS menu bar template: black on transparent, the system tints it. */
const templateSvg = horizonSvg({ color: "#000000" });
/** The Windows taskbar overlay for "something is waiting": one accent dot. */
const badgeSvg = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><circle cx="8" cy="8" r="6" fill="${colors.accent}"/></svg>`;

const png = (svg: string, size: number): Buffer =>
  Buffer.from(new Resvg(svg, { fitTo: { mode: "width", value: size }, background: "rgba(0,0,0,0)" }).render().asPng());

const write = (name: string, data: Buffer) => writeFileSync(new URL(name, out), data);

write("icon.png", png(appSvg, 1024));
write("tray.png", png(traySvg, 32));
write("tray@2x.png", png(traySvg, 64));
write("trayTemplate.png", png(templateSvg, 16));
write("trayTemplate@2x.png", png(templateSvg, 32));
write("badge.png", png(badgeSvg, 16));
write("badge@2x.png", png(badgeSvg, 32));
write("icon.ico", ico([16, 24, 32, 48, 64, 128, 256].map((s) => png(appSvg, s))));
write(
  "icon.icns",
  icns([
    ["icp4", 16],
    ["icp5", 32],
    ["icp6", 64],
    ["ic07", 128],
    ["ic08", 256],
    ["ic09", 512],
    ["ic10", 1024],
  ].map(([type, size]) => [type as string, png(appSvg, size as number)])),
);
console.log(`wrote icons to ${out.pathname}`);

/** An ICO holding PNG images (Windows Vista and later read PNG entries). */
function ico(images: Buffer[]): Buffer {
  const header = Buffer.alloc(6 + 16 * images.length);
  header.writeUInt16LE(0, 0);
  header.writeUInt16LE(1, 2);
  header.writeUInt16LE(images.length, 4);
  let offset = header.length;
  images.forEach((img, i) => {
    const size = img.readUInt32BE(16); // PNG IHDR width
    const e = 6 + 16 * i;
    header.writeUInt8(size >= 256 ? 0 : size, e);
    header.writeUInt8(size >= 256 ? 0 : size, e + 1);
    header.writeUInt8(0, e + 2);
    header.writeUInt8(0, e + 3);
    header.writeUInt16LE(1, e + 4);
    header.writeUInt16LE(32, e + 6);
    header.writeUInt32LE(img.length, e + 8);
    header.writeUInt32LE(offset, e + 12);
    offset += img.length;
  });
  return Buffer.concat([header, ...images]);
}

/** An ICNS holding PNG images, keyed by OSType. */
function icns(entries: [string, Buffer][]): Buffer {
  const chunks = entries.map(([type, data]) => {
    const head = Buffer.alloc(8);
    head.write(type, 0, "ascii");
    head.writeUInt32BE(data.length + 8, 4);
    return Buffer.concat([head, data]);
  });
  const body = Buffer.concat(chunks);
  const head = Buffer.alloc(8);
  head.write("icns", 0, "ascii");
  head.writeUInt32BE(body.length + 8, 4);
  return Buffer.concat([head, body]);
}
