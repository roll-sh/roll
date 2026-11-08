import { readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { IDENTITY_COLORS, NO_SETTINGS } from "./agents-api";
import { automationPlaces, checkProjectIcon, machineColor, machineKind, projectColor, projectFace, seedColor } from "./agents-identity";
import { deriveGraph } from "./agents-graph";
import { machine, mirrored, project, thread } from "@/test/agents";
import { agentsHarness } from "@/test/agents-render";

describe("identity colours", () => {
  it("are made from a seed, the same on every device, and always from the palette", () => {
    for (const seed of ["roll", "acme/web", "", "Ünïcødé 🦀", "mch_8f3a"]) {
      expect(IDENTITY_COLORS).toContain(seedColor(seed));
      expect(seedColor(seed)).toBe(seedColor(seed));
    }
    expect(seedColor("  Roll ")).toBe(seedColor("roll"));
    // Spread: twenty names don't all land on one or two colours.
    expect(new Set(Array.from({ length: 20 }, (_, i) => seedColor(`project-${i}`))).size).toBeGreaterThan(4);
  });

  it("a project's: picked, else its repo's (one repo on two machines looks the same), else its name's", () => {
    const repo = { owner: "acme", repo: "roll" };
    const onLaptop = project({ id: "prj_a", name: "roll", repo });
    const inSandbox = project({ id: "prj_b", name: "roll-checkout", repo });
    expect(projectColor(onLaptop)).toBe(projectColor(inSandbox));
    expect(projectColor(project({ name: "web" }))).toBe(seedColor("web"));
    expect(projectColor(project({ settings: { ...NO_SETTINGS, color: "rose" } }))).toBe("rose");
  });

  it("a machine's: picked, else its id's (a rename keeps it)", () => {
    const m = machine();
    const renamed: typeof m = { ...m, name: "renamed" };
    expect(machineColor(renamed)).toBe(machineColor(m));
    expect(machineColor(machine({ appearance: { color: "teal" } }))).toBe("teal");
  });
});

describe("machineKind", () => {
  it("is what the owner picked, else a sandbox is cloud, a Mac a laptop, Windows a desktop, the rest servers", () => {
    const info = (os: string) => ({ ...machine().info!, os });
    expect(machineKind(machine())).toBe("laptop");
    expect(machineKind(machine({ info: info("windows") }))).toBe("desktop");
    expect(machineKind(machine({ info: info("linux") }))).toBe("server");
    expect(machineKind(machine({ info: null }))).toBe("server");
    expect(machineKind(machine({ sandbox: { backend: "agent37", size: "standard", status: "running", hourly_cents: 3 } }))).toBe("cloud");
    expect(machineKind(machine({ appearance: { kind: "container" } }))).toBe("container");
  });
});

describe("projectFace", () => {
  const logo = { path: "public/favicon.svg", url: "data:image/svg+xml;base64,PHN2Zy8+" };
  it("shows the logo found in the project, else the monogram", () => {
    expect(projectFace(project({ logo }))).toMatchObject({ kind: "logo", url: logo.url });
    expect(projectFace(project({ name: "roll web" }))).toMatchObject({ kind: "monogram", text: "RW" });
  });
  it("a monogram or emoji the owner picked wins over the logo", () => {
    expect(projectFace(project({ logo, settings: { ...NO_SETTINGS, icon: { kind: "monogram" } } }))).toMatchObject({ kind: "monogram", text: "RL" });
    expect(projectFace(project({ logo, settings: { ...NO_SETTINGS, icon: { kind: "monogram", text: "rx" } } }))).toMatchObject({ kind: "monogram", text: "RX" });
    expect(projectFace(project({ logo, settings: { ...NO_SETTINGS, icon: { kind: "emoji", emoji: "🦀" }, color: "pink" } }))).toEqual({ kind: "emoji", emoji: "🦀", color: "pink" });
  });

});

describe("checkProjectIcon", () => {
  // The same cases as agent-proto's `identity::tests::icons_are_checked`.
  it("keeps what the daemon keeps and says why not otherwise", () => {
    expect(checkProjectIcon({ kind: "monogram", text: " rl " })).toEqual({ icon: { kind: "monogram", text: "rl" } });
    expect(checkProjectIcon({ kind: "monogram", text: "  " })).toEqual({ icon: { kind: "monogram" } });
    for (const text of ["ROLL", "a b", "-x"]) expect(checkProjectIcon({ kind: "monogram", text })).toHaveProperty("error");
    expect(checkProjectIcon({ kind: "emoji", emoji: "🦀" })).toEqual({ icon: { kind: "emoji", emoji: "🦀" } });
    expect(checkProjectIcon({ kind: "emoji", emoji: "👨‍👩‍👧" })).toHaveProperty("icon");
    for (const emoji of ["x", ""]) expect(checkProjectIcon({ kind: "emoji", emoji })).toHaveProperty("error");
    expect(checkProjectIcon({ kind: "file", path: "./public/logo.svg" })).toEqual({ icon: { kind: "file", path: "public/logo.svg" } });
    for (const path of ["../x.png", "/etc/x.png", "a//b.png", "logo.txt", "AppIcon.icns"]) expect(checkProjectIcon({ kind: "file", path })).toHaveProperty("error");
  });
});

describe("the identity palette (globals.css)", () => {
  const css = readFileSync(join(process.cwd(), "src/app/globals.css"), "utf8");
  const tokens = [...css.matchAll(/--id-([a-z]+):\s*oklch\(([\d.]+)\s+([\d.]+)\s+([\d.]+)\)/g)].map((m) => ({ name: m[1], l: +m[2], c: +m[3], h: +m[4] }));

  it("has no orange, amber or yellow, and nothing that reads as the accent's blue (busy)", () => {
    const accentHue = 259; // #4c8dff in oklch
    for (const t of tokens) {
      if (t.c < 0.05) continue; // a neutral (slate) has no hue to speak of
      expect(t.h >= 35 && t.h <= 115, `${t.name} hue ${t.h} is orange, amber or yellow`).toBe(false);
      expect(Math.abs(t.h - accentHue) < 30, `${t.name} hue ${t.h} is the accent's`).toBe(false);
    }
  });
});

describe("automationPlaces", () => {
  const roll = { ...project({ id: "prj_roll", repo: { owner: "acme", repo: "roll" } }), machine: "mch_1" };
  const web = { ...project({ id: "prj_web", name: "web" }), machine: "mch_1" };
  const box = machine({ id: "mch_box", name: "build-box" });
  it("names each project and pinned machine once, by id or by repo, in order", () => {
    const spec = {
      defaults: { project: { type: "repo" as const, repo: "acme/roll", path: null }, target: null },
      steps: [
        { config: { project: { type: "id", id: "prj_web" }, target: { type: "machine", machine: "mch_box" } } },
        { config: { project: { type: "id", id: "prj_roll" } } },
        { config: { project: { type: "id", id: "prj_gone" }, target: { type: "sandbox", size: "standard" } } },
        { config: "not an object" },
      ],
    };
    const places = automationPlaces(spec, [roll, web], [machine(), box]);
    expect(places.projects.map((p) => p.id)).toEqual(["prj_roll", "prj_web"]);
    expect(places.machines.map((m) => m.id)).toEqual(["mch_box"]);
  });
});

describe("canvas edges", () => {
  it("carry their project's colour, and a machine's for what hangs off it", () => {
    const h = agentsHarness({
      machines: [machine({ appearance: { color: "violet" } })],
      entities: [mirrored("project", project({ settings: { ...NO_SETTINGS, color: "teal" } }), 2), mirrored("thread", thread(), 3)],
    });
    const g = deriveGraph(h.client.data);
    const tint = (to: string) => g.edges.find((e) => e.to === to)?.tint;
    expect(tint("p:prj_1")).toBe("--id-teal");
    expect(tint("t:thr_1")).toBe("--id-teal");
    expect(g.nodes.get("m:mch_1")?.tint).toBe("--id-violet");
  });
});

