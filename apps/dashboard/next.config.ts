import type { NextConfig } from "next";

/**
 * Hostnames allowed to load the dev server besides localhost (`ROLL_DEV_ORIGINS`, comma separated,
 * e.g. `devbox.example.ts.net,*.tunnel.example.com`). Next matches only the hostname, so a scheme
 * or port given by habit is dropped.
 */
const devOrigins = (process.env.ROLL_DEV_ORIGINS ?? "")
  .split(",")
  .map((o) => o.trim().replace(/^[a-z]+:\/\//i, "").replace(/[:/].*$/, ""))
  .filter(Boolean);

const nextConfig: NextConfig = {
  cacheComponents: true,
  partialPrefetching: true,
  ...(devOrigins.length > 0 ? { allowedDevOrigins: devOrigins } : {}),
  turbopack: {
    rules: {
      "*.css": {
        loaders: ["@tailwindcss/turbopack"],
        as: "*.css",
      },
    },
  },
};

export default nextConfig;
