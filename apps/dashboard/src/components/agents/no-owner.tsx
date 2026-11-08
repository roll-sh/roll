"use client";

import { useState, type FormEvent } from "react";
import { codeApi } from "@/lib/code-api";
import { btn, input, primary } from "../ui";
import { chooseOwner } from "./use-connection";

/**
 * A roll with no owner yet (a fresh server, signed in as admin or open): agents, machines and repos
 * belong to an owner, so make the first one here (`roll code owner create` does the same) and show it.
 */
export function NoOwner() {
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const submit = async (e: FormEvent) => {
    e.preventDefault();
    const n = name.trim().toLowerCase();
    if (!n) return;
    setBusy(true);
    setError(null);
    try {
      await codeApi.createOwner(n, "user");
      chooseOwner(n);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
      setBusy(false);
    }
  };
  return (
    <form onSubmit={submit} className="flex w-full max-w-[340px] flex-col gap-2 text-left">
      <label htmlFor="new-owner" className="text-[12px] text-muted">
        Create an owner
      </label>
      <div className="flex gap-2">
        <input
          id="new-owner"
          className={`${input} min-w-0 flex-1 font-mono`}
          placeholder="your-name"
          autoComplete="off"
          spellCheck={false}
          autoFocus
          value={name}
          onChange={(e) => setName(e.target.value)}
        />
        <button type="submit" className={`${btn} ${primary}`} disabled={busy || !name.trim()}>
          {busy ? "Creating…" : "Create"}
        </button>
      </div>
      {error ? (
        <p role="alert" className="text-[12px] text-danger">
          {error}
        </p>
      ) : (
        <p className="text-[11.5px] text-subtle">Lowercase letters, digits and dashes. Its sign-in token: <code className="font-mono">roll code token create &lt;name&gt; web</code>.</p>
      )}
    </form>
  );
}
