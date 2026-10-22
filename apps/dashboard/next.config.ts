import type { NextConfig } from "next";

const nextConfig: NextConfig = {
  /* config options here */
  cacheComponents: true,
  partialPrefetching: true,
  turbopack: {
    rules: {
      "*.css": {
        loaders: ["@tailwindcss/turbopack"],
        as: "*.css",
      },
    },
  },
  // Git smart HTTP (`roll.sh/owner/repo.git/…`) is proxied to rolld by route handlers under
  // src/app/[owner]/[repo]/ — see lib/git-proxy.ts for why it isn't a rewrite.
};

export default nextConfig;
