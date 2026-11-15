import { describe, expect, it } from "vitest";
import type { Link, Service, Variables } from "@/lib/apps-api";
import { copyName, duplicateOf } from "./duplicate";

describe("duplicating a service", () => {
  it("copies its settings, links and own variables; sealed ones are named, not copied", () => {
    const s = { id: "svc_a", project: "prj_a", name: "web", source: { kind: "image", image: "nginx:1.27" }, build: { builder: "auto" }, run: { type: "web", port: 80 } } as unknown as Service;
    const vars: Variables = {
      vars: [
        { name: "MODE", masked: "••", source: "service" },
        { name: "API_KEY", masked: "••••••••", source: "service", sealed: true },
        { name: "API_URL", masked: "", source: "reference", reference: "${{ api.URL }}" },
        { name: "DATABASE_URL", masked: "••", source: "link" },
        { name: "PORT", masked: "80", source: "roll" },
      ],
    };
    const links = [{ kind: "postgres", ref: "pg_1", prefix: "", vars: ["DATABASE_URL"] }] as Link[];
    const { body, sealed } = duplicateOf(s, "web-copy", vars, { MODE: "prod", DATABASE_URL: "postgres://x" }, links);
    expect(body).toMatchObject({ project: "prj_a", name: "web-copy", source: s.source, run: s.run, links: [{ kind: "postgres", ref: "pg_1", prefix: "" }] });
    expect(body.variables).toEqual({ MODE: "prod", API_URL: "${{ api.URL }}" });
    expect(sealed).toEqual(["API_KEY"]);
    expect(copyName("web", new Set(["web-copy"]))).toBe("web-copy-2");
    expect(copyName("a".repeat(32), new Set()).length).toBeLessThanOrEqual(32);
  });
});
