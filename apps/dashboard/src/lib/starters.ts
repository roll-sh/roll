/**
 * Code starters (Vercel's Clone, Render's and Railway's starter repos): a small app's files that
 * become a new roll code repo's first commit, which a service then follows, so the first deploy
 * is a landing like any other. One JSON file each in `./starters`; apps-build's
 * `every_code_starter_has_a_plan` checks each is detected with no settings.
 */
import astro from "./starters/astro.json";
import fastapi from "./starters/fastapi.json";
import go from "./starters/go.json";
import hono from "./starters/hono.json";
import next from "./starters/next.json";

export interface Starter {
  id: string;
  name: string;
  /** What it is, in a few words. */
  blurb: string;
  /** Path to content; `{{name}}` is the repo's name. */
  files: Record<string, string>;
}

export const STARTERS: Starter[] = [next, astro, hono, fastapi, go];

/** The starter's files for a repo named `name`. */
export const starterFiles = (s: Starter, name: string): Record<string, { content: string }> =>
  Object.fromEntries(Object.entries(s.files).map(([path, text]) => [path, { content: text.replaceAll("{{name}}", name) }]));
