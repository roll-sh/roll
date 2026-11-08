import { describe, expect, it } from "vitest";
import { isNewAgentKey, newAgentHint } from "./agents-surface";

describe("the new-agent shortcut", () => {
  it("is ⌘N on a Mac and Ctrl+Shift+O elsewhere, where Ctrl+N is the browser's new window", () => {
    expect(newAgentHint("MacIntel")).toBe("⌘N");
    expect(newAgentHint("iPhone")).toBe("⌘N");
    expect(newAgentHint("Win32")).toBe("Ctrl+Shift+O");
    expect(newAgentHint("Linux x86_64")).toBe("Ctrl+Shift+O");
    // The hint shown off a Mac is a binding that works there.
    expect(isNewAgentKey({ key: "O", ctrlKey: true, shiftKey: true, metaKey: false, altKey: false } as KeyboardEvent)).toBe(true);
  });
});
