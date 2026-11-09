/**
 * On a change's page in roll code: the agent conversation that published it, if one of the
 * signed-in owner's did (its thread record names the change). A link back to it, and for a
 * conflict the same "Ask to rebase" as the conversation, then "Publish again" once the agent
 * rebased (opening the conversation with its publish form): the page was a dead end.
 */

import { Bot } from "lucide-react";
import { machineFor, useAgentsSnapshot } from "@/components/agents/use-agents";
import { useAgentsConnection } from "@/components/agents/use-connection";
import { RebaseActions } from "@/components/agents/change";
import { requestPublish } from "@/components/agents/session";
import { showAgents } from "@/lib/agents-surface";

/** Opens the conversation over the page and its publish form, once the conversation is there. */
function publishAgain(thread: string) {
  showAgents({ kind: "thread", thread });
  requestPublish(thread);
}

export function ChangeAgent({ repo, number, conflict }: { repo: string; number: number; conflict: boolean }) {
  const { client } = useAgentsConnection({});
  // Follows the stream: the thread's runs say how far a rebase got.
  useAgentsSnapshot(client);
  if (!client) return null;
  let thread = null;
  for (const r of client.data.threads.values()) if (r.value.change?.repo === repo && r.value.change.number === number) thread = r.value;
  if (!thread) return null;
  const id = machineFor(client, thread);
  const machine = id ? client.data.machines.get(id)?.value : undefined;
  return (
    <p className="mt-2 flex flex-wrap items-center gap-x-2 gap-y-1 text-[12.5px] text-muted">
      <Bot className="size-3.5 text-subtle" aria-hidden />
      Published by an agent:
      <button type="button" onClick={() => showAgents({ kind: "thread", thread: thread.id })} className="min-w-0 truncate text-accent hover:underline">
        {thread.title}
      </button>
      {conflict && <RebaseActions client={client} thread={thread} machine={machine} change={{ number, detail: thread.change?.detail ?? null }} onPublish={() => publishAgain(thread.id)} />}
    </p>
  );
}
