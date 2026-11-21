/**
 * Names that read as secrets (`STRIPE_SECRET_KEY`, `GITHUB_TOKEN`, `DB_PASSWORD`): a new variable
 * with one comes sealed by default (Vercel's sensitive-by-default), unless whoever sets it says
 * otherwise. The CLI's `roll apps vars` uses the same rule (`looks_secret`).
 */
/**
 * Whether a project's preview rule is offered a key in roll secrets instead of its value: its name
 * reads as a secret and its value is there and isn't a reference already.
 */
export function keepable(name: string, value: string): boolean {
  return looksSecret(name) && value.trim() !== "" && !value.trim().toLowerCase().startsWith("roll://");
}

export function looksSecret(name: string): boolean {
  const n = name.toUpperCase();
  return /(^|_)(SECRET|TOKEN|PASSWORD|PASSWD|PASS|PRIVATE_KEY|API_KEY|ACCESS_KEY|KEY|CREDENTIALS?|DSN)(_|$)/.test(n) || /SECRET|PASSWORD|TOKEN/.test(n);
}
