// An environment, a key written write-only, a token for CI and the environment's values
// (sdk/pulumi/README.md). e2e.sh runs it against the Terraform provider's fake rolld.
import * as pulumi from "@pulumi/pulumi";
import * as roll from "@pulumi/roll";

const config = new pulumi.Config();

// Set name: left out, Pulumi auto-names it (staging-1a2b3c4).
const staging = new roll.SecretsEnvironment("staging", { slug: "staging", name: "staging" });

// valueWo is never read back, and is kept in the stack's state only encrypted (a Pulumi input).
const stripe = new roll.Secret("stripe", {
  environment: staging.slug,
  key: "STRIPE_KEY",
  valueWo: config.requireSecret("stripeKey"),
  valueWoVersion: 1, // bump to write a new value
});

// Its token is a Pulumi secret: encrypted in the state.
const ci = new roll.SecretsToken("ci", {
  name: "ci",
  environments: [staging.slug],
  actions: ["read", "reveal"],
});

// Read when the program runs. Pulumi doesn't mark a function's results secret: wrap them before
// they reach a stack output or another resource, or they're kept in the state in the clear.
const values = roll.getSecretsValuesOutput({ environment: staging.slug }, { dependsOn: [stripe] });

export const token = ci.token;
export const stripeKey = pulumi.secret(values.values.apply((v) => v["STRIPE_KEY"]));
