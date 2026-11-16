/**
 * Starter templates (Railway's, Render's and Vercel's galleries): project files built on public
 * images, applied by the same import as a "deploy to roll" link (`/dashboard/deploy?template=…`),
 * so a `generate = "secret"` is made once in the browser and an `ask` is asked on the page.
 * apps-build's `every_starter_template_parses` reads these files as `roll up --all` would.
 */
import directus from "./templates/directus.toml?raw";
import metabase from "./templates/metabase.toml?raw";
import n8n from "./templates/n8n.toml?raw";
import umami from "./templates/umami.toml?raw";
import uptimeKuma from "./templates/uptime-kuma.toml?raw";

export interface Template {
  id: string;
  name: string;
  /** What it is, in a few words. */
  blurb: string;
  /** What it makes beside the service. */
  with: string;
  rollToml: string;
}

export const TEMPLATES: Template[] = [
  { id: "n8n", name: "n8n", blurb: "Workflow automation", with: "a volume", rollToml: n8n },
  { id: "umami", name: "Umami", blurb: "Web analytics", with: "Postgres", rollToml: umami },
  { id: "uptime-kuma", name: "Uptime Kuma", blurb: "Uptime monitoring", with: "a volume", rollToml: uptimeKuma },
  { id: "directus", name: "Directus", blurb: "Headless CMS and API", with: "Postgres", rollToml: directus },
  { id: "metabase", name: "Metabase", blurb: "Dashboards over your data", with: "Postgres", rollToml: metabase },
];

export const template = (id: string | undefined): Template | undefined => TEMPLATES.find((t) => t.id === id);
