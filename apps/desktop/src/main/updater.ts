/**
 * Auto-update through electron-updater against a generic feed (`latest*.yml` next to the
 * artifacts). Off unless `settings.autoUpdate.enabled` and a feed URL are set: there is no
 * update server yet.
 */
import type { Settings } from "./settings.ts";
import type { Log } from "./supervisor.ts";

export interface Updater {
  check(): void;
}

export async function startUpdater(s: Settings["autoUpdate"], packaged: boolean, log: Log): Promise<Updater | null> {
  if (!s.enabled || !s.url || !packaged) return null;
  // A plain require (main.cjs is CommonJS): ESM interop can't see electron-updater's getter exports.
  const { autoUpdater } = require("electron-updater") as typeof import("electron-updater");
  autoUpdater.logger = { info: log.event.bind(log), warn: log.event.bind(log), error: log.event.bind(log), debug() {} };
  autoUpdater.setFeedURL({ provider: "generic", url: s.url });
  autoUpdater.autoDownload = true;
  autoUpdater.autoInstallOnAppQuit = true;
  const check = () => {
    autoUpdater.checkForUpdatesAndNotify().catch((e: Error) => log.event(`update check failed: ${e.message}`));
  };
  check();
  setInterval(check, 6 * 60 * 60 * 1000).unref();
  return { check };
}
