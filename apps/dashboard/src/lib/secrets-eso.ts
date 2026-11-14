/**
 * What External Secrets Operator needs to pull an environment's folder into Kubernetes
 * (docs/secrets.md, "Pulling instead: External Secrets Operator"): the token's Secret, a webhook
 * SecretStore and an ExternalSecret. The same document `roll secrets kubernetes` prints.
 */
export interface EsoInput {
  /** roll's URL as the cluster reaches it. */
  server: string;
  project: string;
  env: string;
  path: string;
  /** The Secret ESO writes, and the prefix of the others. */
  name: string;
  namespace?: string;
  /** ESO's refreshInterval: a number of s, m or h. */
  refresh: string;
  token: string;
}

/** The longest `name`: the token's Secret adds `-roll-token`. */
const NAME_MAX = 63 - "-roll-token".length;

/** A DNS label Kubernetes accepts, or why not. */
export function k8sNameProblem(v: string, max = NAME_MAX): string | null {
  if (!v) return "required";
  if (v.length > max) return `at most ${max} characters`;
  if (!/^[a-z0-9]([a-z0-9-]*[a-z0-9])?$/.test(v)) return "lowercase letters, digits and '-'";
  return null;
}

export const esoName = (env: string) => env.toLowerCase().replace(/_/g, "-");

export const refreshOk = (v: string) => /^[0-9]+[smh]$/.test(v);

const enc = encodeURIComponent;

export function esoManifest(i: EsoInput): string {
  const server = i.server.replace(/\/+$/, "");
  const q = JSON.stringify;
  const ns = i.namespace ? `\n  namespace: ${i.namespace}` : "";
  const query = i.path === "/" ? "dynamic=false&strict=1" : `dynamic=false&strict=1&path=${enc(i.path)}`;
  // `urlquery`: a key can't add its own query (`staging/values?dynamic=true&x=`).
  const url = `${server}/v1/secrets/projects/${enc(i.project)}/environments/{{ .remoteRef.key | urlquery }}/values?${query}`;
  return `# roll secrets ${i.env}:${i.path} (${i.project}), pulled by External Secrets Operator (docs/secrets.md).
apiVersion: v1
kind: Secret
metadata:
  name: ${i.name}-roll-token${ns}
  labels:
    external-secrets.io/type: webhook
stringData:
  token: ${q(i.token)}
---
apiVersion: external-secrets.io/v1
kind: SecretStore
metadata:
  name: ${i.name}-roll${ns}
spec:
  provider:
    webhook:
      url: ${q(url)}
      headers:
        Authorization: "Bearer {{ .auth.token }}"
      result:
        jsonPath: "$.values{{ with .remoteRef.property }}.{{ . }}{{ end }}"
      secrets:
        - name: auth
          secretRef:
            name: ${i.name}-roll-token
---
apiVersion: external-secrets.io/v1
kind: ExternalSecret
metadata:
  name: ${i.name}${ns}
spec:
  refreshInterval: ${i.refresh}
  secretStoreRef:
    name: ${i.name}-roll
    kind: SecretStore
  target:
    name: ${i.name}
  dataFrom:
    - extract:
        key: ${q(i.env)}
`;
}
