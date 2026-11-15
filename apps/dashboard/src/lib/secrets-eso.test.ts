import { describe, expect, it } from "vitest";
import { esoManifest, k8sNameProblem } from "./secrets-eso";

describe("esoManifest", () => {
  it("points ESO's store at the folder's values, without dynamic credentials, as roll secrets kubernetes does", () => {
    const y = esoManifest({ server: "https://roll.example/", project: "prj_1", env: "staging", path: "/payments", name: "staging", namespace: "shop", refresh: "1h", token: "rlst_x" });
    expect(y).toContain('url: "https://roll.example/v1/secrets/projects/prj_1/environments/{{ .remoteRef.key | urlquery }}/values?dynamic=false&strict=1&path=%2Fpayments"');
    expect(y).toContain("  name: staging-roll-token\n  namespace: shop\n  labels:\n    external-secrets.io/type: webhook");
    expect(y).toContain('token: "rlst_x"');
    expect(y).toContain('        key: "staging"');
    expect(k8sNameProblem("Staging_1")).toBe("lowercase letters, digits and '-'");
  });
});
