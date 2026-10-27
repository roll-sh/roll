"use client";

import { Plus, UserRound } from "lucide-react";
import { useState } from "react";
import { iamApi, type IamUser } from "@/lib/storage-iam";
import { btn, ErrorNote, input, Panel, primary } from "../../ui";
import { useAction } from "../shared";
import {
  AttachedPolicies,
  DeleteButton,
  DetailHeader,
  EntityList,
  InlinePolicies,
  Loading,
  MasterDetail,
  Muted,
  NameForm,
  NothingSelected,
  Row,
  Section,
  useLoad,
  When,
} from "./common";

/** IAM groups: give many users the same policies. */
export function GroupsTab() {
  const groups = useLoad("groups", iamApi.listGroups);
  const [selected, setSelected] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const { busy, error, run } = useAction(groups.reload);
  const current = groups.data?.some((g) => g.GroupName === selected) ? selected : null;
  return (
    <>
      <ErrorNote error={error ?? groups.error} />
      <div className="mt-4">
        <MasterDetail
          list={
            <Panel
              title="Groups"
              aside={
                <span className="flex items-center gap-2">
                  <span className="font-mono text-[12px] text-subtle">{groups.data?.length ?? "—"}</span>
                  {!creating && (
                    <button type="button" onClick={() => setCreating(true)} className={btn}>
                      <Plus className="size-3.5" /> New
                    </button>
                  )}
                </span>
              }
            >
              {creating && (
                <NameForm
                  kind="group"
                  placeholder="developers"
                  busy={busy === "create"}
                  onCancel={() => setCreating(false)}
                  onCreate={async (name) => {
                    const r = await run("create", () => iamApi.createGroup(name));
                    if (!r) return false;
                    setCreating(false);
                    setSelected(name);
                    return true;
                  }}
                />
              )}
              <EntityList
                items={groups.data}
                keyOf={(g) => g.GroupName}
                selected={current}
                onSelect={setSelected}
                title={(g) => g.GroupName}
                meta={(g) => (
                  <>
                    created <When iso={g.CreateDate} />
                  </>
                )}
                empty="No groups yet. Attach policies to a group and every user in it gets them."
              />
            </Panel>
          }
          detail={
            current ? (
              <GroupDetail
                key={current}
                name={current}
                onDeleted={async () => {
                  setSelected(null);
                  await groups.reload();
                }}
              />
            ) : (
              <NothingSelected>Choose a group to see its members and policies.</NothingSelected>
            )
          }
        />
      </div>
    </>
  );
}

function GroupDetail({ name, onDeleted }: { name: string; onDeleted: () => Promise<void> }) {
  const group = useLoad(`group:${name}`, () => iamApi.getGroup(name));
  const [adding, setAdding] = useState(false);
  const [all, setAll] = useState<IamUser[] | null>(null);
  const [pick, setPick] = useState("");
  const { busy, error, run } = useAction(group.reload);
  const members = group.data?.users ?? null;
  const have = new Set((members ?? []).map((u) => u.UserName));
  const choices = (all ?? []).filter((u) => !have.has(u.UserName));
  return (
    <section className="rounded-xl border border-line bg-panel">
      <DetailHeader
        name={name}
        arn={group.data?.group.Arn ?? "…"}
        meta={
          group.data && (
            <>
              <span className="font-mono text-[11px] text-subtle">{group.data.group.GroupId}</span> · created{" "}
              <When iso={group.data.group.CreateDate} />
            </>
          )
        }
        busy={busy === "delete"}
        deleteHint="IAM only deletes a group without members or policies."
        onDelete={async () => {
          const r = await run("delete", () => iamApi.deleteGroup(name));
          if (r) await onDeleted();
        }}
      />
      {(error || group.error) && (
        <div className="px-5 pb-1">
          <ErrorNote error={error ?? group.error} />
        </div>
      )}
      <Section
        title="Members"
        aside={
          !adding && (
            <button
              type="button"
              onClick={() => {
                setAdding(true);
                iamApi.listUsers().then(setAll, () => setAll([]));
              }}
              className={btn}
            >
              <Plus className="size-3.5" /> Add user
            </button>
          )
        }
      >
        {adding && (
          <form
            className="rise mb-3 flex flex-wrap gap-2"
            onSubmit={async (e) => {
              e.preventDefault();
              if (!pick) return;
              const r = await run("add", () => iamApi.addUserToGroup(name, pick));
              if (r) {
                setPick("");
                setAdding(false);
              }
            }}
          >
            <select value={pick} onChange={(e) => setPick(e.target.value)} aria-label="User" className={`${input} min-w-0 flex-1 font-mono text-[12px]`}>
              <option value="">{all === null ? "Loading users…" : choices.length ? "Choose a user" : "No other users"}</option>
              {choices.map((u) => (
                <option key={u.UserName} value={u.UserName}>
                  {u.UserName}
                </option>
              ))}
            </select>
            <button type="submit" disabled={!pick || busy !== null} className={primary}>
              {busy === "add" ? <span className="shimmer">Adding…</span> : "Add"}
            </button>
            <button type="button" onClick={() => setAdding(false)} className={btn}>
              Cancel
            </button>
          </form>
        )}
        {members === null ? (
          <Loading />
        ) : members.length === 0 ? (
          <Muted>No members.</Muted>
        ) : (
          <ul>
            {members.map((u) => (
              <Row
                key={u.UserName}
                actions={
                  <DeleteButton
                    label={`Remove ${u.UserName}`}
                    confirm="Remove"
                    busy={busy === `rm-${u.UserName}`}
                    onConfirm={() => run(`rm-${u.UserName}`, () => iamApi.removeUserFromGroup(name, u.UserName))}
                  />
                }
              >
                <p className="flex items-center gap-2 truncate font-mono text-[13px]">
                  <UserRound className="size-3.5 shrink-0 text-subtle" />
                  {u.UserName}
                </p>
              </Row>
            ))}
          </ul>
        )}
      </Section>
      <AttachedPolicies target={{ kind: "group", name }} />
      <InlinePolicies target={{ kind: "group", name }} />
    </section>
  );
}
