import { describe, expect, it } from "vitest";
import { keyboardOf } from "./keyboard-inset";

describe("the on-screen keyboard", () => {
  it("is what the visible part leaves of the window's bottom", () => {
    // iPhone: 844 tall, the keyboard leaves 508 visible.
    expect(keyboardOf(844, { height: 508, offsetTop: 0, scale: 1 })).toEqual({ inset: 336, visible: 508 });
    // Safari scrolled the visible part down to the focused field.
    expect(keyboardOf(844, { height: 508, offsetTop: 120, scale: 1 })).toEqual({ inset: 216, visible: 508 });
  });

});
