"use client";

import { Check, LoaderCircle, Plus, X } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { codeApi, isNotFound, validOwnerName, validRepoName, type Owner, type OwnerKind } from "@/lib/code-api";
import { btn, input } from "../ui";
import { Field, Segmented } from "./shared";

export type NameState = "idle" | "checking" | "available" | "taken" | "reserved" | "invalid" | "unknown";

export interface NameStatus {
  state: NameState;
  message: string | null;
}

const DEBOUNCE_MS = 300;

/** Debounced availability: a local syntax check first, then `check` once typing pauses. */
function useAvailability(key: string, valid: boolean, invalidMessage: string, check: () => Promise<NameStatus>) {
  const [status, setStatus] = useState<NameStatus>({ state: "idle", message: null });
  useEffect(() => {
    if (!key) {
      setStatus({ state: "idle", message: null });
      return;
    }
    if (!valid) {
      setStatus({ state: "invalid", message: invalidMessage });
      return;
    }
    let alive = true;
    setStatus({ state: "checking", message: null });
    const t = setTimeout(() => {
      check().then(
        (s) => alive && setStatus(s),
        () => alive && setStatus({ state: "unknown", message: "Couldn't check right now" }),
      );
    }, DEBOUNCE_MS);
    return () => {
      alive = false;
      clearTimeout(t);
    };
  }, [key, valid, invalidMessage]);
  return status;
}

/** Owner names share one global namespace with roll.sh's own routes. */
export function useOwnerName(name: string) {
  return useAvailability(name, validOwnerName(name), "a–z, 0–9 and single hyphens, up to 39", async () => {
    const r = await codeApi.checkName(name);
    if (r.available) return { state: "available", message: null };
    const state = r.reason ?? "taken";
    return {
      state,
      message: r.message ?? (state === "reserved" ? "reserved by roll.sh" : state === "taken" ? "already taken" : null),
    };
  });
}

/** Repo names are unique per owner, case-insensitively. */
export function useRepoName(owner: string, name: string) {
  return useAvailability(
    owner ? `${owner}/${name}` : "",
    !!name && validRepoName(name),
    "letters, digits, ., - and _ (not ending in .git)",
    async () => {
      try {
        const r = await codeApi.repo(`${owner}/${name}`);
        return { state: "taken", message: `${r.full_name} already exists` };
      } catch (e) {
        if (isNotFound(e)) return { state: "available", message: null };
        throw e;
      }
    },
  );
}

/** One crisp line under a name field. */
export function NameHint({ status, available = "available" }: { status: NameStatus; available?: string }) {
  const { state, message } = status;
  if (state === "idle") return <span className="block h-4" />;
  if (state === "checking")
    return (
      <span className="flex h-4 items-center gap-1 text-[11px] text-subtle">
        <LoaderCircle className="size-3 animate-spin motion-reduce:animate-none" /> checking
      </span>
    );
  if (state === "available")
    return (
      <span className="flex h-4 items-center gap-1 text-[11px] text-ok">
        <Check className="size-3" /> {available}
      </span>
    );
  if (state === "unknown") return <span className="block h-4 text-[11px] text-subtle">{message}</span>;
  return (
    <span className="flex h-4 items-center gap-1 text-[11px] text-danger">
      <X className="size-3" />
      <span className="font-medium">{state}</span>
      {message && <span className="truncate text-muted">— {message}</span>}
    </span>
  );
}

/** Owners you can create repos under, with a new organisation (or your username, the first time) inline. */
export function OwnerPicker({ value, onChange }: { value: string; onChange: (owner: string) => void }) {
  const [owners, setOwners] = useState<Owner[] | null>(null);
  const [creating, setCreating] = useState(false);
  // The owner to go back to if a new organisation is cancelled.
  const [previous, setPrevious] = useState(value);

  const load = useCallback(async () => {
    try {
      const list = await codeApi.owners();
      setOwners(list);
      return list;
    } catch {
      setOwners([]);
      return [];
    }
  }, []);

  useEffect(() => {
    load().then((list) => {
      if (!list.length) setCreating(true);
      else if (!value) onChange((list.find((o) => o.kind === "user") ?? list[0]).name);
    });
  }, [load]);

  if (owners === null) {
    return (
      <Field label="Owner">
        <div className={`${input} text-subtle`}>
          <span className="shimmer">Loading owners…</span>
        </div>
      </Field>
    );
  }

  const first = owners.length === 0;
  return (
    <div className="space-y-3">
      {!first && (
        <Field label="Owner">
          <select
            value={creating ? "__new" : value}
            onChange={(e) => {
              if (e.target.value === "__new") {
                // No owner until the new one exists, so the repo name isn't checked against the old one.
                setPrevious(value);
                onChange("");
                setCreating(true);
              } else {
                setCreating(false);
                onChange(e.target.value);
              }
            }}
            className={`${input} w-full font-mono text-[12px]`}
          >
            {owners.map((o) => (
              <option key={o.name} value={o.name}>
                {o.name}
                {o.kind === "org" ? " · organisation" : ""}
              </option>
            ))}
            <option value="__new">+ New organisation…</option>
          </select>
        </Field>
      )}
      {creating && (
        <NewOwner
          first={first}
          onCancel={
            first
              ? undefined
              : () => {
                  setCreating(false);
                  onChange(previous);
                }
          }
          onCreated={async (o) => {
            await load();
            setCreating(false);
            onChange(o.name);
          }}
        />
      )}
    </div>
  );
}

function NewOwner({
  first,
  onCreated,
  onCancel,
}: {
  first: boolean;
  onCreated: (o: Owner) => void;
  onCancel?: () => void;
}) {
  const [kind, setKind] = useState<OwnerKind>(first ? "user" : "org");
  const [name, setName] = useState("");
  const [display, setDisplay] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const status = useOwnerName(name);

  const create = async () => {
    if (status.state !== "available") return;
    setBusy(true);
    setError(null);
    try {
      onCreated(await codeApi.createOwner(name, kind, display.trim() || undefined));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="rise space-y-3 rounded-lg border border-line bg-bg p-4">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <p className="text-[13px] font-medium">{first ? "Claim a name" : "New organisation"}</p>
        {first && (
          <div className="w-48">
            <Segmented
              value={kind}
              options={[
                { label: "user", value: "user" },
                { label: "org", value: "org" },
              ]}
              onChange={setKind}
            />
          </div>
        )}
      </div>
      <p className="text-[12px] text-subtle">
        Owners share one namespace across roll.sh — <span className="font-mono">roll.sh/{name || "name"}</span>.
      </p>
      <div className="grid gap-3 sm:grid-cols-2">
        <label className="block min-w-0 space-y-1">
          <span className="block text-[11px] text-subtle">{kind === "user" ? "Username" : "Organisation name"}</span>
          <input
            autoFocus
            value={name}
            onChange={(e) => setName(e.target.value.toLowerCase())}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                e.preventDefault();
                create();
              }
            }}
            placeholder={kind === "user" ? "ada" : "acme"}
            spellCheck={false}
            autoComplete="off"
            className={`${input} w-full font-mono text-[12px]`}
          />
          <NameHint status={status} />
        </label>
        <Field label="Display name (optional)">
          <input
            value={display}
            onChange={(e) => setDisplay(e.target.value)}
            placeholder={kind === "user" ? "Ada Lovelace" : "Acme Inc."}
            className={`${input} w-full`}
          />
        </Field>
      </div>
      {error && <p className="text-[12px] text-danger">{error}</p>}
      <div className="flex gap-2">
        <button type="button" onClick={create} disabled={busy || status.state !== "available"} className={btn}>
          {busy ? <span className="shimmer">Creating…</span> : (
            <>
              <Plus className="size-3.5" /> Create {kind === "user" ? "username" : "organisation"}
            </>
          )}
        </button>
        {onCancel && (
          <button type="button" onClick={onCancel} className={btn}>
            Cancel
          </button>
        )}
      </div>
    </div>
  );
}
