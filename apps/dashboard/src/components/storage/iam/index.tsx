"use client";

import Link from "next/link";
import { storageApi } from "@/lib/storage-api";
import { StorageError } from "@/lib/storage-session";
import { AppShell } from "../../app-shell";
import { Connecting, Offline } from "../../ui";
import { StorageIdentity, useActingAccount, useConsoleSession, useScope } from "../console";
import { TabNav, useHashTab } from "../shared";
import { AccountTab } from "./account";
import { useLoad } from "./common";
import { GroupsTab } from "./groups";
import { PoliciesTab } from "./policies";
import { IamRolesTab } from "./roles";
import { SimulatorTab } from "./simulator";
import { UsersTab } from "./users";

const TABS = [
  { value: "users", label: "Users" },
  { value: "groups", label: "Groups" },
  { value: "policies", label: "Policies" },
  { value: "roles", label: "Roles" },
  { value: "account", label: "Account settings" },
  { value: "simulator", label: "Policy simulator" },
] as const;
type Tab = (typeof TABS)[number]["value"];
const TAB_VALUES = TABS.map((t) => t.value);

/** IAM for the account the dashboard acts in: as its root user (operator), or as the signed-in IAM user. */
export function StorageIam() {
  const session = useConsoleSession();
  const acting = useActingAccount();
  const scope = useScope();
  const [tab, setTab] = useHashTab<Tab>(TAB_VALUES, "users");
  // The default account's id, when the operator acts in it.
  const info = useLoad(`info:${scope}`, () =>
    storageApi.info().then(
      (i) => ({ accountId: i.account_id, offline: false }),
      (e) => {
        if (e instanceof StorageError) return { accountId: null, offline: false };
        return { accountId: null, offline: true };
      },
    ),
  );
  const accountId = session?.account_id ?? acting ?? info.data?.accountId ?? null;

  return (
    <AppShell
      crumb={
        <>
          <Link href="/dashboard/storage" className="text-muted hover:text-fg">
            storage
          </Link>{" "}
          <span className="text-subtle">/</span> IAM
        </>
      }
    >
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <h1 className="text-2xl font-semibold tracking-[-0.04em]">IAM</h1>
          <p className="mt-1 text-[13px] text-muted">
            Users, groups, roles and policies, as in AWS IAM — the same API answers at the S3 endpoint for the AWS CLI and SDKs.
          </p>
        </div>
        <div className="flex flex-col items-start gap-1 sm:items-end">
          <StorageIdentity />
          {accountId && <p className="font-mono text-[11px] text-subtle">account {accountId}</p>}
        </div>
      </div>

      {info.data?.offline ? (
        <Offline />
      ) : !info.data || !accountId ? (
        <Connecting />
      ) : (
        <>
          <TabNav tabs={TABS} tab={tab} onChange={setTab} label="IAM sections" />
          <div key={scope}>
            {tab === "users" && <UsersTab accountId={accountId} />}
            {tab === "groups" && <GroupsTab />}
            {tab === "policies" && <PoliciesTab />}
            {tab === "roles" && <IamRolesTab accountId={accountId} />}
            {tab === "account" && <AccountTab accountId={accountId} session={session} />}
            {tab === "simulator" && <SimulatorTab />}
          </div>
        </>
      )}
    </AppShell>
  );
}
