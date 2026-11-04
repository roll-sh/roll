import tailwindcss from "@tailwindcss/vite";
import { tanstackStart } from "@tanstack/react-start/plugin/vite";
import viteReact from "@vitejs/plugin-react";
import { defineConfig } from "vite";

/**
 * Hostnames allowed to load the dev server besides localhost (`ROLL_DEV_ORIGINS`, comma separated,
 * e.g. `devbox.example.ts.net,.tunnel.example.com`; a leading dot allows every subdomain). Vite
 * matches only the hostname, so a scheme or port given by habit is dropped, and `*.x` means `.x`.
 */
const devOrigins = (process.env.ROLL_DEV_ORIGINS ?? "")
  .split(",")
  .map((o) => o.trim().replace(/^[a-z]+:\/\//i, "").replace(/[:/].*$/, "").replace(/^\*\./, "."))
  .filter(Boolean);

export default defineConfig({
  server: {
    port: 3000,
    ...(devOrigins.length > 0 ? { allowedHosts: devOrigins } : {}),
  },
  resolve: { tsconfigPaths: true },
  // Native bindings load from node_modules at runtime instead of being bundled.
  ssr: { external: ["@resvg/resvg-js"] },
  plugins: [
    tanstackStart({
      // Git smart HTTP (`roll.sh/owner/repo.git/…`) is proxied to rolld by server routes under
      // src/routes/$owner/$repo/ — see lib/git-proxy.ts for why it isn't a rewrite.
      router: { routeFileIgnorePattern: "\\.test\\.tsx?$" },
    }),
    // React's plugin goes after Start's.
    viteReact(),
    tailwindcss(),
  ],
});
