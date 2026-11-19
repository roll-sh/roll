import type { Identity } from "@roll/secrets/types";

/** The issuers the dashboard makes identities for (docs/secrets.md, "Workload identities"). */
export const GITHUB = "https://token.actions.githubusercontent.com";
export const GITLAB = "https://gitlab.com";
/** Azure Pipelines' issuer, one per organization (its id: `$(System.CollectionId)`). */
export const AZURE = "https://vstoken.dev.azure.com/";
/** The audience of every Azure Pipelines token: Azure's own, not this server's. */
export const AZURE_AUDIENCE = "api://AzureADTokenExchange";

/** The action's reference, as sdk/github-action's README gives it. */
const ACTION = "roll-sh/roll/sdk/github-action@main";

export type CiSnippet = { title: string; text: string };

/**
 * The CI lines that log a fresh runner in as `i` and read `env` (none for an issuer roll has no
 * snippet for). `api` is the API as the runner reaches it; `install` the CLI's install line.
 */
export function ciSnippets(i: Pick<Identity, "id" | "issuer" | "audience">, { api, env, install }: { api: string; env: string; install: string }): CiSnippet[] {
  if (i.issuer === GITHUB) {
    // Both ask GitHub for a token with the API's URL as its audience unless told otherwise.
    const ownAudience = i.audience !== api;
    const action = [
      "permissions: { id-token: write, contents: read }",
      "steps:",
      `  - uses: ${ACTION}`,
      "    with:",
      `      api-url: ${api}`,
      `      identity: ${i.id}`,
      `      environment: ${env}`,
      ...(ownAudience ? [`      audience: ${i.audience}`] : []),
      `  - run: ./deploy.sh   # ${env}'s values are its environment variables, masked`,
    ];
    const cli = [
      "permissions: { id-token: write, contents: read }",
      "env:",
      `  ROLL_API_URL: ${api}`,
      `  ROLL_IDENTITY: ${i.id}`,
      ...(ownAudience ? [`  ROLL_OIDC_AUDIENCE: ${i.audience}`] : []),
      "steps:",
      `  - run: ${install} && echo "$HOME/.local/bin" >> "$GITHUB_PATH"`,
      `  - run: roll run --env ${env} -- ./deploy.sh`,
    ];
    return [
      { title: "With the action (no CLI to install)", text: action.join("\n") },
      { title: "Or with roll run", text: cli.join("\n") },
    ];
  }
  if (i.issuer === GITLAB) {
    // The CLI reads the job's token from ROLL_OIDC_TOKEN; its aud must be the identity's.
    const job = [
      "deploy:",
      "  id_tokens:",
      "    ROLL_OIDC_TOKEN:",
      `      aud: ${i.audience}`,
      "  variables:",
      `    ROLL_API_URL: ${api}`,
      `    ROLL_IDENTITY: ${i.id}`,
      "  script:",
      `    - ${install}`,
      '    - export PATH="$HOME/.local/bin:$PATH"',
      `    - roll run --env ${env} -- ./deploy.sh`,
    ];
    return [{ title: ".gitlab-ci.yml", text: job.join("\n") }];
  }
  if (i.issuer.startsWith(AZURE)) {
    // System.AccessToken reaches a script only when mapped: roll asks Azure for the pipeline's token with it.
    const pipeline = [
      "variables:",
      `  ROLL_API_URL: ${api}`,
      `  ROLL_IDENTITY: ${i.id}`,
      "steps:",
      `  - script: ${install} && echo "##vso[task.prependpath]$HOME/.local/bin"`,
      `  - script: roll run --env ${env} -- ./deploy.sh`,
      "    env: { SYSTEM_ACCESSTOKEN: $(System.AccessToken) }",
    ];
    return [{ title: "azure-pipelines.yml", text: pipeline.join("\n") }];
  }
  return [];
}
