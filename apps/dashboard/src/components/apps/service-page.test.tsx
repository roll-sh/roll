import { render, screen } from "@testing-library/react";
import type { ReactNode } from "react";
import { describe, expect, it, vi } from "vitest";
import type { Deployment, Environment, Service } from "@/lib/apps-api";
import { MarkStateProvider } from "../mark-state";
import { ServicePage } from "./service-page";

const T0 = 1_760_000_000_000;

const service: Service = {
  id: "svc_1",
  project: "prj_1",
  name: "web",
  source: { kind: "image", image: "nginx:1.27" },
  build: { builder: "auto" },
  run: { type: "web", port: 80, min_instances: 0, max_instances: 1, concurrency: 50, cpu: 0.25, memory_mb: 512, health_timeout_s: 60, suspend_after_s: 30, sleep_after_s: 900 },
  registry: null,
  created_at: T0,
  environments: [{ name: "production", state: "running", url: "https://web.roll.test", active: { id: "dep_1", revision: null, created_at: T0 }, latest: { id: "dep_1", status: "active" } }],
};

const env: Environment = {
  ...service.environments[0],
  service: "svc_1",
  branch: null,
  ephemeral: false,
  instances: [],
  domains: [],
  port: null,
  needs_deploy: false,
  private: false,
  next_run_at: null,
  crash_loop: null,
  created_at: T0,
};

const deploy: Deployment = {
  id: "dep_1",
  service: "svc_1",
  environment: "production",
  status: "active",
  trigger: "api",
  revision: null,
  message: "First deploy",
  image: "nginx:1.27",
  plan: null,
  error: null,
} as Deployment;

const later = <T,>() => new Promise<T>(() => {});

vi.mock("@/lib/apps-api", async (orig) => {
  const real = await orig<typeof import("@/lib/apps-api")>();
  const fake = new Proxy({} as typeof real.appsApi, {
    get: (_, key) =>
      ({
        service: async () => service,
        environment: async () => env,
        deploys: async () => [deploy],
      })[key as string] ?? (() => later()),
  });
  return { ...real, appsApi: fake };
});
vi.mock("@tanstack/react-router", () => ({ useNavigate: () => vi.fn() }));
vi.mock("@/components/link", () => ({ Link: ({ href, children, className }: { href: string; children: ReactNode; className?: string }) => <a href={href} className={className}>{children}</a> }));
vi.mock("../app-shell", () => ({
  AppShell: ({ crumb, actions, children }: { crumb: ReactNode; actions?: ReactNode; children: ReactNode }) => (
    <div>
      <nav>{crumb}</nav>
      {actions}
      <main>{children}</main>
    </div>
  ),
}));
vi.mock("@/lib/workspace-store", () => ({
  useWorkspace: () => ({ status: "ready", projects: [], inventory: new Map(), kinds: new Set(), spend: new Map(), staged: {} }),
  workspace: { refresh: vi.fn() },
}));

describe("ServicePage", () => {
  // The page renders "Connecting" first and the service once it loads: every hook has to run on
  // both renders (a hook after an early return throws React's #310 on the second).
  it("loads a service and shows its environment and deployments", async () => {
    render(
      <MarkStateProvider>
        <ServicePage id="svc_1" />
      </MarkStateProvider>,
    );
    expect(await screen.findByText("First deploy")).toBeInTheDocument();
    expect(screen.getAllByText("web").length).toBeGreaterThan(0);
  });
});
