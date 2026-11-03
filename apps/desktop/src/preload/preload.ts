/**
 * The preload: exposes `window.rollDesktop` (src/shared/bridge.ts) and nothing else. Runs
 * sandboxed with context isolation. The main process accepts these messages only from the app's
 * own top frame in its window, so a page on another origin gains nothing from the bridge.
 */
import { contextBridge, ipcRenderer, type IpcRendererEvent } from "electron";
import { ipc, VERSION_ARG, type DeepLink, type DesktopNotice, type DesktopStatus, type RollDesktop } from "../shared/bridge.ts";

const version = process.argv.find((a) => a.startsWith(VERSION_ARG))?.slice(VERSION_ARG.length) ?? "";
const listeners = new Set<(link: DeepLink) => void>();

ipcRenderer.on(ipc.deepLink, (_event: IpcRendererEvent, link: DeepLink) => {
  for (const l of listeners) {
    try {
      l(link);
    } catch (e) {
      console.error(e);
    }
  }
});

const bridge: RollDesktop = {
  version,
  platform: process.platform as RollDesktop["platform"],
  setStatus(status: DesktopStatus) {
    ipcRenderer.send(ipc.setStatus, status);
  },
  notify(notice: DesktopNotice) {
    ipcRenderer.send(ipc.notify, notice);
  },
  onDeepLink(listener) {
    listeners.add(listener);
    if (listeners.size === 1) ipcRenderer.send(ipc.deepLinkListening, true);
    return () => {
      if (listeners.delete(listener) && listeners.size === 0) ipcRenderer.send(ipc.deepLinkListening, false);
    };
  },
};

contextBridge.exposeInMainWorld("rollDesktop", Object.freeze(bridge));
