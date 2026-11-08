/**
 * The application menu and the tray menu, as Electron menu templates. Pure: the actions are
 * passed in, so tests can build and click them without Electron.
 */
import type { MenuItemConstructorOptions } from "electron";
import { activityLine, type TrayModel } from "./status.ts";

export interface MenuActions {
  /** Show the main window (the dashboard and roll code). */
  openRoll(): void;
  /** Open or focus the Agents window. */
  openAgents(): void;
  back(): void;
  forward(): void;
  pause(): void;
  resume(): void;
  signIn(): void;
  showLogs(): void;
  openSettings(): void;
  openDocs(): void;
  checkForUpdates: (() => void) | null;
  quit(): void;
}

export function appMenu(platform: NodeJS.Platform, m: TrayModel, a: MenuActions): MenuItemConstructorOptions[] {
  const mac = platform === "darwin";
  const agents: MenuItemConstructorOptions = {
    label: "Agents",
    submenu: [
      // ⇧: ⌘1–9 are the composer's model picks in the pages.
      { label: "Open roll", accelerator: "CmdOrCtrl+Shift+1", click: a.openRoll },
      { label: "Open agents", accelerator: "CmdOrCtrl+Shift+2", click: a.openAgents },
      { type: "separator" },
      { label: m.machine, enabled: false },
      m.paused
        ? { label: "Resume agents", enabled: m.canPause, click: a.resume }
        : { label: "Pause agents", enabled: m.canPause, click: a.pause },
      { label: "Connect this machine…", enabled: m.canSignIn, click: a.signIn },
      { type: "separator" },
      { label: "Show agent log", click: a.showLogs },
    ],
  };
  // No ⌘,: the pages take it for the agents' settings (T3's shortcut); this is the app's own file.
  const settingsItem: MenuItemConstructorOptions = { label: "Desktop settings…", click: a.openSettings };
  const updates: MenuItemConstructorOptions[] = a.checkForUpdates
    ? [{ label: "Check for updates…", click: a.checkForUpdates }]
    : [];
  return [
    ...(mac
      ? [
          {
            label: "roll",
            submenu: [
              { role: "about" },
              ...updates,
              { type: "separator" },
              settingsItem,
              { type: "separator" },
              { role: "services" },
              { type: "separator" },
              { role: "hide" },
              { role: "hideOthers" },
              { role: "unhide" },
              { type: "separator" },
              { label: "Quit roll", accelerator: "Cmd+Q", click: a.quit },
            ],
          } satisfies MenuItemConstructorOptions,
        ]
      : []),
    {
      label: "File",
      submenu: mac
        ? [{ role: "close" }]
        : [settingsItem, { type: "separator" }, { role: "close" }, { label: "Quit", accelerator: "Ctrl+Q", click: a.quit }],
    },
    { role: "editMenu" },
    {
      label: "View",
      submenu: [
        { role: "reload" },
        { role: "forceReload" },
        { role: "toggleDevTools" },
        { type: "separator" },
        { role: "resetZoom" },
        { role: "zoomIn" },
        { role: "zoomOut" },
        { type: "separator" },
        { role: "togglefullscreen" },
      ],
    },
    {
      label: "Go",
      submenu: [
        { label: "Back", accelerator: mac ? "Cmd+[" : "Alt+Left", click: a.back },
        { label: "Forward", accelerator: mac ? "Cmd+]" : "Alt+Right", click: a.forward },
      ],
    },
    agents,
    { role: "windowMenu" },
    {
      role: "help",
      submenu: [{ label: "roll docs", click: a.openDocs }, ...(mac ? [] : updates)],
    },
  ];
}

export function trayMenu(m: TrayModel, a: MenuActions): MenuItemConstructorOptions[] {
  const activity = activityLine(m);
  return [
    { label: m.machine, enabled: false },
    ...(activity ? [{ label: activity, enabled: false }] : []),
    { type: "separator" },
    { label: "Open agents", click: a.openAgents },
    { label: "Open roll", click: a.openRoll },
    m.paused
      ? { label: "Resume agents", enabled: m.canPause, click: a.resume }
      : { label: "Pause agents", enabled: m.canPause, click: a.pause },
    ...(m.canSignIn ? [{ label: "Connect this machine…", click: a.signIn }] : []),
    { type: "separator" },
    { label: "Quit roll", click: a.quit },
  ];
}
