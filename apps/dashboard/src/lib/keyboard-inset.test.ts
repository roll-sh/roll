import { describe, expect, it } from "vitest";
import { keyboardOf } from "./keyboard-inset";

describe("the on-screen keyboard", () => {
  it("is what the visible part leaves of the window's bottom", () => {
    // iPhone: 844 tall, the keyboard leaves 508 visible.
    expect(keyboardOf(844, { height: 508, offsetTop: 0, scale: 1 })).toEqual({ inset: 336, visible: 508 });
    // Safari scrolled the visible part down to the focused field.
    expect(keyboardOf(844, { height: 508, offsetTop: 120, scale: 1 })).toEqual({ inset: 216, visible: 508 });
  });

  it("isn't there with no keyboard, a toolbar settling, a pinch zoom or no visual viewport", () => {
    expect(keyboardOf(844, { height: 844, offsetTop: 0, scale: 1 }).inset).toBe(0);
    expect(keyboardOf(844, { height: 820, offsetTop: 0, scale: 1 }).inset).toBe(0);
    expect(keyboardOf(844, { height: 422, offsetTop: 100, scale: 2 }).inset).toBe(0);
    expect(keyboardOf(844, null)).toEqual({ inset: 0, visible: 844 });
  });
});
