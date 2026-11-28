/**
 * What the marketing header's menus, its phone sheet and the footer link to, beside the products
 * (those come from lib/marketing.ts). Every link here is a route or an anchor that exists: add a
 * page before adding its link.
 */
import { Activity, GitBranch, LayoutGrid, LogIn, Palette, Plug, Route, Sparkles, Waypoints, KeyRound, type LucideIcon } from "lucide-react";
import { productBySlug, productHref, type Scene } from "@/lib/marketing";

/** Where the CLI's releases live (the installer in `public/install.sh` downloads from it). */
export const GITHUB = "https://github.com/roll-sh/roll";

/** The home page's sections a link can land on. */
export const ANCHORS = { products: "/#products", how: "/#how", measured: "/#measured", shipped: "/#shipped" };

export type NavLink = { title: string; body: string; href: string; icon: LucideIcon; external?: boolean };

export const DEVELOPERS: { name: string; links: NavLink[] }[] = [
  {
    name: "Get started",
    links: [
      { title: "Dashboard", body: "A project's products on one canvas.", href: "/dashboard", icon: LayoutGrid },
      { title: "Sign in", body: "GitHub, Google or an email code.", href: "/signin", icon: LogIn },
    ],
  },
  {
    name: "Learn",
    links: [
      { title: "How roll works", body: "One request through the daemon.", href: ANCHORS.how, icon: Waypoints },
      { title: "Measured", body: "Numbers, beside what they're measured against.", href: ANCHORS.measured, icon: Activity },
      { title: "Recently shipped", body: "What's new across the products.", href: ANCHORS.shipped, icon: Sparkles },
    ],
  },
  {
    name: "Integrate",
    links: [
      { title: "MCP server", body: "About 500 tools, one per route.", href: productHref("access"), icon: Plug },
      { title: "Secrets SDKs", body: "TypeScript, Go, Python, Java, .NET, Ruby.", href: productHref("secrets"), icon: KeyRound },
      { title: "Model gateway", body: "Your model keys behind one endpoint.", href: productHref("gateway"), icon: Route },
    ],
  },
  {
    name: "Resources",
    links: [
      { title: "Brand", body: "Horizon, the dot globe and their states.", href: "/brand", icon: Palette },
      { title: "GitHub", body: "Releases of the roll CLI.", href: GITHUB, icon: GitBranch, external: true },
    ],
  },
];

/** The footer's last two columns: the same links, dealt by what they're for. */
export const FOOTER: { name: string; links: { title: string; href: string; external?: boolean }[] }[] = [
  { name: "Developers", links: [...DEVELOPERS[0].links, ...DEVELOPERS[2].links] },
  { name: "Resources", links: [{ title: "All products", href: "/products" }, ...DEVELOPERS[1].links, ...DEVELOPERS[3].links] },
];

/** The product the Products menu shows until another is pointed at, and whether it's marked new. */
export const FEATURED = { slug: "previews", fresh: true };

/** The header's plain links: the two products people ask for by name. */
export const PINNED = ["code", "agents"].map((slug) => productBySlug(slug)!);

/**
 * The Developers menu's terminal: the short `roll` commands of a product's first scene, each with
 * the line it answers with. They're the catalogue's own lines, so they can't drift from the CLI's.
 */
export function transcript(slug: string, width = 38): { cmd: string; out: string }[] {
  const scene: Scene | undefined = productBySlug(slug)?.scenes[0];
  const pairs: { cmd: string; out: string }[] = [];
  scene?.lines.forEach((line, i) => {
    const next = scene.lines[i + 1];
    if (line.cmd?.startsWith("roll ") && line.cmd.length <= width && next?.out) pairs.push({ cmd: line.cmd, out: next.out });
  });
  return pairs;
}
