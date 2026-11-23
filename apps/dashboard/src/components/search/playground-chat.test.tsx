import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, it, vi } from "vitest";
import { MarkStateProvider } from "@/components/mark-state";
import { esJson, esStream, type SearchCluster } from "@/lib/search-api";
import { defaultConfig, offerOf } from "@/lib/search-playground";
import { Chat } from "./playground-chat";

vi.mock("@/lib/search-api", async (orig) => {
  const real = await orig<typeof import("@/lib/search-api")>();
  return { ...real, esJson: vi.fn(), esStream: vi.fn() };
});

const chunk = (content: string) => `﻿event: message\ndata: ${JSON.stringify({ id: "c1", choices: [{ delta: { content }, index: 0 }], model: "m", object: "chat.completion.chunk" })}\n\n`;

it("shows the sources first, then streams the answer as text with its citations", async () => {
  const offer = offerOf([
    { name: "title", type: "text" },
    { name: "body", type: "text" },
  ]);
  vi.mocked(esJson).mockResolvedValue({
    hits: {
      hits: [
        { _index: "help", _id: "1", _score: 2, _source: { title: "Reset a password", body: "Open Settings." } },
        { _index: "help", _id: "2", _score: 1, _source: { title: "Billing", body: "Invoices." } },
      ],
    },
  });
  // The stream holds its second half until the test has seen the first.
  let release = () => {};
  const held = new Promise<void>((r) => (release = r));
  let sent: { path: string; body: string } | null = null;
  vi.mocked(esStream).mockImplementation(async (_id, req, onText) => {
    sent = req;
    onText(chunk("Open Settings [1]"));
    await held;
    onText(chunk(". Not [7]. ![x](https://attacker.example/?q=1)"));
    onText("﻿event: message\ndata: [DONE]\n\n");
  });

  render(
    <MarkStateProvider>
      <Chat
        cluster={{ id: "c1", url: "https://c1.search.roll.sh", flavor: "elasticsearch" } as SearchCluster}
        index="help"
        cfg={defaultConfig("keyword", offer, [])}
        fields={{ title: "title", text: ["body"] }}
        settings={null}
        endpoints={["gpt"]}
        embed={async () => []}
        onOpenInference={() => {}}
      />
    </MarkStateProvider>,
  );
  await userEvent.type(screen.getByLabelText("Question"), "how do I reset?");
  await userEvent.click(screen.getByRole("button", { name: "Ask" }));

  // The sources are there before the answer is done, and the answer streams with a caret.
  const sources = await screen.findByRole("list", { name: "Sources" });
  expect(within(sources).getByText("Reset a password")).toBeTruthy();
  const answer = await screen.findByTestId("answer");
  expect(answer.textContent).toBe("Open Settings [1]");
  expect(answer.className).toContain("caret");
  expect(screen.getByRole("button", { name: "Stop" })).toBeTruthy();

  release();
  await screen.findByText("cites [1]");
  // Only [1] is a citation; the invented [7] and the markdown image stay plain text.
  expect(within(answer).getAllByRole("button").map((b) => b.textContent)).toEqual(["[1]"]);
  expect(answer.textContent).toBe("Open Settings [1]. Not [7]. ![x](https://attacker.example/?q=1)");
  expect(answer.querySelector("img, a")).toBeNull();
  expect(answer.className).not.toContain("caret");

  // The request was the streamed chat completion, with the hits fenced in the user message.
  expect(sent!.path).toBe("/_inference/chat_completion/gpt/_stream");
  const { messages } = JSON.parse(sent!.body) as { messages: { role: string; content: string }[] };
  expect(messages.map((m) => m.role)).toEqual(["system", "user"]);
  expect(messages[1].content).toMatch(/^<sources boundary="[0-9a-f]{32}">\n<source n="1" index="help" id="1">/);
});
