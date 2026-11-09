import { useEffect, useRef, useState } from "react";

/**
 * Whether a canvas card's floating tools have been needed yet: the first time the pointer comes
 * onto the card, it takes focus or a tap, or it's open beside the canvas (`data-selected`). They
 * mount then and stay. A quarter of a card's elements were tools nobody had looked at, and 200
 * cards arriving at once (a filter cleared) paid for them all in style, layout and paint.
 * Put the ref on the toolbar (inside the card).
 */
export function useToolsArmed() {
  const [armed, setArmed] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (armed) return;
    const card = ref.current?.closest<HTMLElement>("[data-card]");
    const arm = () => setArmed(true);
    // Not on the canvas (a test rendering the card alone): always there.
    if (!card || card.hasAttribute("data-selected") || card.matches(":hover, :focus-within")) return arm();
    const opened = new MutationObserver(() => card.hasAttribute("data-selected") && arm());
    opened.observe(card, { attributes: true, attributeFilter: ["data-selected"] });
    card.addEventListener("pointerenter", arm);
    card.addEventListener("pointerdown", arm);
    card.addEventListener("focusin", arm);
    return () => {
      opened.disconnect();
      card.removeEventListener("pointerenter", arm);
      card.removeEventListener("pointerdown", arm);
      card.removeEventListener("focusin", arm);
    };
  }, [armed]);
  return { armed, ref };
}
