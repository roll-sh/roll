import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi, type Mock } from "vitest";
import { SecretsError, type Environment, type SecretsApi } from "@/lib/secrets-api";
import type { Secret } from "@roll/secrets/types";
import { EnvView } from "./env-view";

/** A stand-in for `secretsApi`: answers a test gives, and loads nobody answers stay pending. */
const answers = new Map<string, Mock>();
const api = new Proxy({} as SecretsApi, {
  get(_, key) {
    if (typeof key !== "string") return undefined;
    let fn = answers.get(key);
    if (!fn) answers.set(key, (fn = vi.fn(() => new Promise(() => {}))));
    return fn;
  },
});
const fn = (key: keyof SecretsApi) => api[key] as unknown as Mock;

const ctx = { project: { id: "prj_1", name: "shop" }, api, envs: [] as Environment[], standard: [] as Environment[], previews: [], reloadEnvs: vi.fn(), historyDays: null, search: {}, go: vi.fn(), resourceName: () => "", required: [], serious: 0, reloadRequired: vi.fn() };
vi.mock("./secrets-page", () => ({ useSecrets: () => ctx, secretsHref: () => "/" }));

const environment = (slug: string, isProtected: boolean): Environment => ({ id: `env_${slug}`, slug, name: slug[0].toUpperCase() + slug.slice(1), position: 0, kind: "standard", base: null, labels: {}, expires_at: null, protected: isProtected, approvals: 1, approvers: [], created_at: 0, secrets: 1, max_age_days: null, notify: null, trusted: false });
const secret: Secret = { id: "sec_1", key: "DATABASE_URL", path: "/", type: "shared", version: 3, comment: "", tags: [], link: null, fp: "f1", references: [], updated_at: 0, updated_by: "ann", source: { kind: "own" }, overridden: false, expires_at: null };

beforeEach(() => {
  answers.clear();
  fn("secrets").mockResolvedValue([secret]);
  fn("folders").mockResolvedValue({ path: "/", folders: [] });
  fn("requests").mockResolvedValue([]);
  fn("imports").mockResolvedValue([]);
  fn("insights").mockResolvedValue({ path: "/", max_age_days: null, reads_since: 0, keys: [] });
});

const click = (name: string | RegExp) => act(async () => void fireEvent.click(screen.getByRole("button", { name })));

/** Opens DATABASE_URL's editor, types a new value and saves it. */
async function edit(value: string) {
  await click("Edit DATABASE_URL");
  fireEvent.change(screen.getByLabelText("Value"), { target: { value } });
  await click("Save");
}

describe("writing secrets", () => {
  it("previews a pasted .env against the folder, then imports it as one change", async () => {
    fn("importDotenv").mockResolvedValue({ change: { seq: 7 }, items: [{ key: "STRIPE_KEY" }], warnings: [], skipped: ["DATABASE_URL"] });
    render(<EnvView env={environment("staging", false)} />);
    await screen.findByText("DATABASE_URL");

    await click(/Paste \.env/);
    fireEvent.change(screen.getByLabelText(".env contents"), { target: { value: "DATABASE_URL=postgres://x\nSTRIPE_KEY=sk_test_1\n" } });
    expect(screen.getByText("1 new · 0 replaced · 1 skipped")).toBeInTheDocument();

    await click("Import 1");
    expect(fn("importDotenv")).toHaveBeenCalledTimes(1);
    expect(fn("importDotenv")).toHaveBeenCalledWith("staging", expect.objectContaining({ dotenv: "DATABASE_URL=postgres://x\nSTRIPE_KEY=sk_test_1\n", path: "/", overwrite: false }));
    expect(await screen.findByText(/Imported 1 key as change #7/)).toBeInTheDocument();
  });

  it("turns an edit in a protected environment into a change request, and says so", async () => {
    fn("set").mockRejectedValue(new SecretsError("production needs an approval", 409, "approval_required"));
    fn("openRequest").mockResolvedValue({ id: "req_1" });
    render(<EnvView env={environment("production", true)} />);
    await screen.findByText("DATABASE_URL");

    await edit("postgres://new");
    expect(await screen.findByText("Production is protected.")).toBeInTheDocument();
    await click("Open a change request");
    expect(fn("openRequest")).toHaveBeenCalledWith("production", expect.objectContaining({ ops: [expect.objectContaining({ op: "set", key: "DATABASE_URL", value: "postgres://new", if_version: 3 })] }));
    expect(await screen.findByText("Opened change request req_1")).toBeInTheDocument();
  });

  it("asks to read the key again when another edit landed first, instead of overwriting it", async () => {
    fn("set").mockRejectedValue(new SecretsError("DATABASE_URL changed since version 3: now version 4; read it again and reapply", 409, "stale"));
    render(<EnvView env={environment("staging", false)} />);
    await screen.findByText("DATABASE_URL");

    await edit("postgres://mine");
    expect(await screen.findByText(/read it again and reapply/)).toBeInTheDocument();
    expect(fn("set")).toHaveBeenCalledTimes(1);
    expect(fn("set")).toHaveBeenCalledWith("staging", "DATABASE_URL", expect.objectContaining({ value: "postgres://mine", if_version: 3 }), "/");
    expect(fn("openRequest")).not.toHaveBeenCalled();
    // What's there now is loaded again for the next try.
    await waitFor(() => expect(fn("secrets")).toHaveBeenCalledTimes(2));
  });
});
