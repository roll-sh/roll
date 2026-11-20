/**
 * The project-wide panels that used to share the Secrets surface's Syncs tab, by the tab each is on
 * now, in order. A panel's key is its anchor (`#transit-keys`), so a link to one keeps working
 * after a move: `?view=syncs#transit-keys` lands on Keys & certificates (`movedPanel`).
 */
export const PANELS = {
  syncs: ["syncs"],
  credentials: ["rotators", "saas-rotators", "outside-databases", "directories", "snowflake", "atlas", "gateways", "cloud-accounts", "aws-roles", "github-apps", "kubernetes"],
  keys: ["transit-keys", "cas", "ssh-cas"],
} as const;

export type PanelTab = keyof typeof PANELS;
export type Panel = (typeof PANELS)[PanelTab][number];

const TAB_OF = new Map<string, PanelTab>(Object.entries(PANELS).flatMap(([tab, panels]) => panels.map((p) => [p, tab as PanelTab] as const)));

/** The tab a panel is on, if `panel` names one. */
export const tabOf = (panel: string): PanelTab | undefined => TAB_OF.get(panel);

/** Where a link to a panel on the wrong tab belongs (the view to replace it with), else null. */
export function movedPanel(view: string | undefined, hash: string): PanelTab | null {
  const tab = tabOf(hash.replace(/^#/, ""));
  return tab && view && view !== tab && view in PANELS ? tab : null;
}
