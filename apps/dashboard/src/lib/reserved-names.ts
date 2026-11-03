/**
 * Names roll.sh keeps for itself. Owners share the top-level namespace with the app's own routes
 * (`roll.sh/<owner>`), so none of these can be a user or organisation name. The Rust side mirrors
 * this list in `code_control::names::RESERVED`; change both together.
 *
 * Everything is lowercase. Entries that can't be valid owner names anyway (`_next`, `icon.svg`)
 * are listed so the two lists also cover every path the app answers at the top level.
 */
export const RESERVED_NAMES = [
  // Routes and files that exist in src/app today.
  "dashboard",
  "icon.svg",
  "favicon.ico",
  "_next",
  // Next.js and web conventions.
  "api",
  "robots.txt",
  "sitemap.xml",
  "manifest.json",
  "manifest.webmanifest",
  ".well-known",
  "opengraph-image",
  "twitter-image",
  "apple-icon",
  "static",
  "assets",
  "public",
  // Product and marketing pages roll.sh has or will have.
  "about",
  "blog",
  "brand",
  "careers",
  "changelog",
  "community",
  "contact",
  "customers",
  "docs",
  "enterprise",
  "explore",
  "features",
  "help",
  "jobs",
  "legal",
  "pricing",
  "privacy",
  "security",
  "status",
  "support",
  "terms",
  "trust",
  // Accounts and auth.
  "account",
  "admin",
  "auth",
  "billing",
  "callback",
  "join",
  "login",
  "logout",
  "new",
  "oauth",
  "settings",
  "signin",
  "signout",
  "signup",
  "sso",
  // Code (GitHub-style paths that must never be an owner).
  "code",
  "git",
  "raw",
  "orgs",
  "organizations",
  "users",
  "repos",
  "search",
  "notifications",
  "marketplace",
  "topics",
  "trending",
  "gist",
  "gists",
  "sponsors",
  // Products and dashboard sections.
  "databases",
  "postgres",
  "redis",
  "queues",
  "tasks",
  "streams",
  "kafka",
  "functions",
  "deployments",
  "edge",
  "logs",
  "observability",
  "previews",
  // roll itself.
  "roll",
  "rollsh",
  "roll-sh",
  "www",
  "mail",
  "app",
  "apps",
  "cdn",
  "v1",
  "health",
  "root",
  "system",
] as const;

export type ReservedName = (typeof RESERVED_NAMES)[number];

const reserved = new Set<string>(RESERVED_NAMES);

export function isReservedName(name: string) {
  return reserved.has(name.toLowerCase());
}
