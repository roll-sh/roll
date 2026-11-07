/**
 * roll desktop: two windows on the roll web app, a tray with this machine's status, and
 * `roll agent up` supervised in the background. The main window shows roll (the dashboard and
 * roll code, `settings.url`); the Agents window shows the canvas (`/dashboard/agents`) and opens
 * from the dashboard's header control, `roll://` links, notifications, the tray and the menu.
 * The shell's logic lives in the modules next to this one (`windows.ts` decides which window a
 * link goes to); this file wires them to Electron.
 *
 * Structure and security posture follow T3 Code's desktop app (`apps/desktop/src/main.ts`,
 * `window/DesktopWindow.ts`): a sandboxed, context-isolated renderer with no Node, navigation
 * pinned to the app's origin, new windows denied (web links go to the system browser), every
 * permission denied but a short allowlist, and webviews refused. Both windows get the same.
 */
import { app, BrowserWindow, ipcMain, Menu, nativeImage, Notification, screen, session, shell, Tray, type IpcMainEvent } from "electron";
import { homedir } from "node:os";
import { join, resolve } from "node:path";
import {
  ipc,
  VERSION_ARG,
  WINDOW_ARG,
  type AgentsTarget,
  type DeepLink,
  type DesktopNotice,
  type DesktopStatus,
  type DesktopWindow,
} from "../shared/bridge.ts";
import { deepLinkFromArgv, parseDeepLink, SCHEME } from "./deeplink.ts";
import { agentConfigPath, findRollBinary, loginShellPath, mergePath, readAgentLogin } from "./local.ts";
import { lineReader, runLogin, type Login } from "./login.ts";
import { appMenu, trayMenu, type MenuActions } from "./menu.ts";
import { decideNavigation, httpOrigin, isInApp, permissionAllowed, type Origins } from "./policy.ts";
import { loadSettings } from "./settings.ts";
import { DEFAULT_SIZE, fitBounds, MIN_SIZE, readState, writeState } from "./state.ts";
import { Announcer, noticeFor, parseDaemonLine, parseNotice, parseStatus, trayModel, type LinkState, type LoginState, type TrayModel } from "./status.ts";
import { FileLog, Supervisor, type DaemonState, type Log } from "./supervisor.ts";
import { startUpdater } from "./updater.ts";
import { agentsUrl, delivery, parseAgentsTarget, routeLink, targetLink } from "./windows.ts";

app.setName("roll");
// Not `<appData>/roll`: on Linux that is ~/.config/roll, the CLI's own directory (agent.json),
// and Chromium fills userData with caches.
app.setPath("userData", join(app.getPath("appData"), "roll-desktop"));
app.enableSandbox();

// One instance: a second launch (often carrying a roll:// link) hands its argv to the first.
const primary = app.requestSingleInstanceLock();
if (!primary) app.exit(0);

/** Set once the windows are wired; until then links wait in `pendingLink`. */
let route: ((link: DeepLink) => void) | null = null;
let focusWindow: (() => void) | null = null;
let pendingLink: DeepLink | null = deepLinkFromArgv(process.argv);

function deliver(link: DeepLink | null): void {
  if (!link) return;
  if (route) route(link);
  else pendingLink = link;
}

app.on("open-url", (event, url) => {
  event.preventDefault();
  deliver(parseDeepLink(url));
});
app.on("second-instance", (_event, argv) => {
  const link = deepLinkFromArgv(argv);
  if (link) deliver(link);
  else focusWindow?.();
});

if (process.defaultApp) {
  // Development (`electron .`): register this checkout's Electron with the app path.
  app.setAsDefaultProtocolClient(SCHEME, process.execPath, [resolve(process.argv[1] ?? ".")]);
} else {
  app.setAsDefaultProtocolClient(SCHEME);
}

if (primary) {
  app.whenReady().then(main, (e: unknown) => {
    console.error(e);
    app.exit(1);
  });
}

async function main(): Promise<void> {
  const data = app.getPath("userData");
  const logs = app.getPath("logs");
  const desktopLog = new FileLog(join(logs, "desktop.log"), "desktop");
  const agentLog = new FileLog(join(logs, "agent.log"), "agent");
  const settingsFile = join(data, "settings.json");
  const stateFile = join(data, "state.json");
  const settings = loadSettings(settingsFile, process.env);
  for (const w of settings.warnings) desktopLog.event(w);
  desktopLog.event(`roll desktop ${app.getVersion()}: ${settings.url} (api ${settings.apiUrl})`);
  const origins: Origins = { app: settings.origin, extra: settings.extraOrigins };
  const state = readState(stateFile);
  const saveState = debounce(() => writeState(stateFile, state), 500);
  const icons = join(app.getAppPath(), "resources", "icons");

  app.setAboutPanelOptions({ applicationName: "roll", applicationVersion: app.getVersion(), copyright: "keep the code rolling" });

  // ── security, for every web contents ──
  const ses = session.defaultSession;
  ses.setPermissionRequestHandler((_wc, permission, callback, details) => {
    callback(permissionAllowed(origins, permission, details.requestingUrl, details.isMainFrame));
  });
  ses.setPermissionCheckHandler((_wc, permission, requestingOrigin, details) =>
    permissionAllowed(origins, permission, details.requestingUrl ?? requestingOrigin, details.isMainFrame),
  );
  ses.setDevicePermissionHandler(() => false);
  ses.setDisplayMediaRequestHandler((_request, callback) => callback(null));
  app.on("web-contents-created", (_event, contents) => {
    contents.on("will-attach-webview", (e) => e.preventDefault());
    contents.setWindowOpenHandler(({ url }) => {
      const d = decideNavigation(origins, url, true);
      // A change's link in the Agents window: roll's own page, in roll's window, signed in.
      if (d.action === "main") show(d.url);
      else if (d.action === "external") void shell.openExternal(d.url);
      return { action: "deny" };
    });
    // Top-level navigations and redirects: in the app, to the system browser, or nowhere.
    const top = (e: Electron.Event<Electron.WebContentsWillNavigateEventParams>) => {
      if (!e.isMainFrame) return;
      const d = decideNavigation(origins, e.url, false);
      if (d.action === "allow") return;
      e.preventDefault();
      if (d.action === "external") void shell.openExternal(d.url);
    };
    // Subframes may only show the allowed origins and never open anything.
    const sub = (e: Electron.Event<Electron.WebContentsWillNavigateEventParams>) => {
      if (!e.isMainFrame && !isInApp(origins, e.url)) e.preventDefault();
    };
    contents.on("will-navigate", top);
    contents.on("will-redirect", top);
    contents.on("will-redirect", sub);
    contents.on("will-frame-navigate", sub);
  });

  // ── this machine: the CLI, its login, the daemon ──
  // The login shell can take seconds; the window doesn't wait for it, only the daemon does.
  const env = { ...process.env };
  const envReady = loginShellPath(process.env, process.platform).then((shellPath) => {
    if (shellPath) env.PATH = mergePath(shellPath, env.PATH);
  });
  const bin = findRollBinary({
    env: process.env,
    packaged: app.isPackaged,
    resourcesPath: process.resourcesPath,
    appPath: app.getAppPath(),
    platform: process.platform,
  });
  desktopLog.event(bin ? `roll CLI: ${bin}` : "roll CLI not found (set ROLL_BIN, or build target/release/roll)");
  const agentJson = agentConfigPath(process.env, homedir());
  const saved = readAgentLogin(agentJson);
  let machineId = saved?.machine ?? null;
  let loginState: LoginState = saved ? { kind: "signed_in" } : { kind: "signed_out" };
  let login: Login | null = null;
  let status: DesktopStatus | null = null;
  let link: LinkState = "connecting";
  const announcer = new Announcer();

  // The daemon's output goes to agent.log; its "connected" / "retrying" lines drive the tray.
  const watchLink = lineReader((line) => {
    const next = parseDaemonLine(line);
    if (next && next !== link) {
      link = next;
      refresh();
    }
  });
  const daemonLog: Log = {
    event: (line) => agentLog.event(line),
    output: (chunk) => {
      agentLog.output(chunk);
      watchLink(chunk.toString());
    },
  };

  const supervisor = bin
    ? new Supervisor({
        command: bin,
        // `up` takes the API and token from agent.json.
        args: ["agent", "up"],
        env,
        log: daemonLog,
        onState: (s) => onDaemonState(s),
      })
    : null;

  function onDaemonState(s: DaemonState): void {
    if (s.kind !== "running") link = "connecting";
    // `roll agent logout` (or a revoked machine) leaves no agent.json: stop retrying, ask again.
    if (s.kind === "restarting" && !readAgentLogin(agentJson)) {
      loginState = { kind: "signed_out" };
      machineId = null;
      void supervisor?.stop();
    }
    refresh();
  }

  function startDaemon(): void {
    void envReady.then(() => {
      if (!supervisor || !settings.daemon || state.paused || loginState.kind !== "signed_in") return;
      supervisor.start();
    });
  }

  function signIn(): void {
    if (!bin || !settings.daemon || login || loginState.kind === "signed_in") return;
    loginState = { kind: "signing_in", code: null };
    refresh();
    const started = runLogin({
      command: bin,
      args: ["agent", "login", "--json", "--api", settings.apiUrl],
      env,
      log: agentLog,
      onPrompt: (p) => {
        loginState = { kind: "signing_in", code: p.code };
        refresh();
        // The approve page is on the app's origin, where the user is signed in: one click.
        if (isInApp(origins, p.url)) show(p.url);
        else void shell.openExternal(p.url);
      },
    });
    login = started;
    void started.done.then((r) => {
      login = null;
      const now = readAgentLogin(agentJson);
      if (r.ok && now) {
        machineId = now.machine;
        loginState = { kind: "signed_in" };
        desktopLog.event("machine signed in");
        startDaemon();
        show(settings.url);
      } else {
        loginState = { kind: "signed_out" };
        desktopLog.event(`sign-in did not finish: ${r.ok ? "no agent.json" : r.reason}`);
      }
      refresh();
    });
  }

  // ── the windows ──
  let win: BrowserWindow | null = null;
  let agentsWin: BrowserWindow | null = null;
  let tray: Tray | null = null;
  /** The Agents window's page has a deep-link listener. */
  let agentsListens = false;
  /** A target waiting for the loading Agents window's page to listen. */
  let agentsPending: AgentsTarget | null = null;
  let agentsPendingTimer: ReturnType<typeof setTimeout> | null = null;
  let quitting = false;

  function createWindow(kind: DesktopWindow, url: string): BrowserWindow {
    // The main window's state is at the top of state.json, the Agents window's under `agents`.
    const saved = kind === "main" ? state : state.agents;
    const bounds = fitBounds(
      saved.bounds,
      screen.getAllDisplays().map((d) => d.workArea),
    );
    const mac = process.platform === "darwin";
    const w = new BrowserWindow({
      title: kind === "main" ? "roll" : "roll agents",
      ...(bounds ?? DEFAULT_SIZE),
      minWidth: MIN_SIZE.width,
      minHeight: MIN_SIZE.height,
      show: false,
      backgroundColor: "#000000",
      ...(mac ? {} : { icon: join(icons, "icon.png") }),
      // The canvas draws its own 48px top bar around the traffic lights on macOS.
      ...(mac && kind === "agents" ? { titleBarStyle: "hiddenInset" as const, trafficLightPosition: { x: 16, y: 16 } } : {}),
      autoHideMenuBar: !mac,
      webPreferences: {
        preload: join(__dirname, "preload.cjs"),
        contextIsolation: true,
        sandbox: true,
        nodeIntegration: false,
        nodeIntegrationInSubFrames: false,
        nodeIntegrationInWorker: false,
        webviewTag: false,
        spellcheck: true,
        additionalArguments: [`${VERSION_ARG}${app.getVersion()}`, `${WINDOW_ARG}${kind}`],
      },
    });
    if (saved.maximized) w.maximize();
    w.once("ready-to-show", () => w.show());
    w.on("page-title-updated", (e) => e.preventDefault());
    const remember = () => {
      if (w.isDestroyed() || w.isMinimized() || w.isFullScreen()) return;
      saved.maximized = w.isMaximized();
      if (!saved.maximized) saved.bounds = w.getNormalBounds();
      saveState();
    };
    for (const ev of ["resize", "move", "maximize", "unmaximize"] as const) w.on(ev as "resize", remember);
    w.on("close", (e) => {
      remember();
      // Closing the Agents window just closes it; the agents keep running either way.
      if (kind === "main" && !quitting && settings.closeToTray && tray) {
        e.preventDefault();
        w.hide();
      }
    });
    // A trackpad pinch reaches the page as ctrl+wheel (the canvas zooms itself), not page zoom.
    const noPinchZoom = () => void w.webContents.setVisualZoomLevelLimits(1, 1).catch(() => {});
    noPinchZoom();
    w.webContents.on("did-finish-load", noPinchZoom);
    w.webContents.on("render-process-gone", (_e, d) => desktopLog.event(`${kind} renderer gone: ${d.reason}`));
    void w.loadURL(url);
    return w;
  }

  const alive = (w: BrowserWindow | null): w is BrowserWindow => !!w && !w.isDestroyed();

  function reveal(w: BrowserWindow): void {
    if (w.isMinimized()) w.restore();
    w.show();
    w.focus();
  }

  function ensureMain(): BrowserWindow {
    if (alive(win)) return win;
    const w = createWindow("main", settings.url);
    w.on("closed", () => {
      if (win === w) win = null;
    });
    win = w;
    return w;
  }

  function focusMain(): void {
    reveal(ensureMain());
  }

  /** Loads `url` in the main window and shows it. */
  function show(url: string): void {
    void ensureMain().loadURL(url);
    focusMain();
  }

  function clearAgentsPending(): void {
    agentsPending = null;
    if (agentsPendingTimer) clearTimeout(agentsPendingTimer);
    agentsPendingTimer = null;
  }

  function createAgents(target: AgentsTarget): BrowserWindow {
    const w = createWindow("agents", agentsUrl(settings.origin, target));
    w.on("closed", () => {
      if (agentsWin !== w) return;
      agentsWin = null;
      agentsListens = false;
      clearAgentsPending();
    });
    w.webContents.on("did-start-navigation", (e) => {
      if (e.isMainFrame && !e.isSameDocument && agentsWin === w) agentsListens = false;
    });
    // A queued target whose page never listens (it isn't the canvas) loads as a URL instead.
    w.webContents.on("did-stop-loading", () => {
      if (agentsWin !== w || !agentsPending || agentsListens) return;
      if (agentsPendingTimer) clearTimeout(agentsPendingTimer);
      agentsPendingTimer = setTimeout(() => {
        agentsPendingTimer = null;
        const target = agentsPending;
        agentsPending = null;
        if (target && agentsWin === w && !w.isDestroyed() && !agentsListens) void w.loadURL(agentsUrl(settings.origin, target));
      }, 3000);
    });
    return w;
  }

  /** Opens (or focuses) the one Agents window on `target`. */
  function openAgents(target: AgentsTarget): void {
    const w = alive(agentsWin) ? agentsWin : null;
    const how = delivery({
      open: !!w,
      listening: !!w && agentsListens && isInApp(origins, w.webContents.getURL()),
      loading: !!w && w.webContents.isLoading(),
    });
    if (how === "open" || !w) {
      // A new window shows itself once it has painted.
      agentsWin = createAgents(target);
      return;
    }
    if (how === "queue") agentsPending = target;
    else {
      clearAgentsPending();
      if (how === "send") w.webContents.send(ipc.deepLink, targetLink(target));
      else void w.loadURL(agentsUrl(settings.origin, target));
    }
    reveal(w);
  }

  function openLink(link: DeepLink): void {
    const to = routeLink(link);
    if (to.window === "agents") openAgents(to.target);
    else show(settings.origin + to.path);
  }

  // ── the bridge: only the app's own top frame in one of our windows may talk to the shell ──
  const fromApp = (e: IpcMainEvent): DesktopWindow | null => {
    const frame = e.senderFrame;
    if (!frame || frame.parent !== null || httpOrigin(frame.url) !== origins.app) return null;
    if (alive(win) && e.sender === win.webContents) return "main";
    if (alive(agentsWin) && e.sender === agentsWin.webContents) return "agents";
    return null;
  };
  ipcMain.on(ipc.setStatus, (e, raw: unknown) => {
    // Either window may report; the latest wins.
    if (!fromApp(e)) return;
    const next = parseStatus(raw);
    if (!next) return;
    status = next;
    for (const p of announcer.fresh(next)) notify(noticeFor(p));
    refresh();
  });
  ipcMain.on(ipc.notify, (e, raw: unknown) => {
    if (!fromApp(e)) return;
    const n = parseNotice(raw);
    if (n) notify(n);
  });
  ipcMain.on(ipc.openAgents, (e, raw: unknown) => {
    if (!fromApp(e)) return;
    const target = parseAgentsTarget(raw);
    if (target) openAgents(target);
  });
  ipcMain.on(ipc.deepLinkListening, (e, on: unknown) => {
    // Links only ever go to the Agents window, so only its page's listener counts.
    if (fromApp(e) !== "agents" || !alive(agentsWin)) return;
    agentsListens = on === true;
    if (agentsListens && agentsPending) {
      const target = agentsPending;
      clearAgentsPending();
      agentsWin.webContents.send(ipc.deepLink, targetLink(target));
    }
  });

  // Notifications are garbage-collected (and lose their click handler) unless referenced.
  const shown: Notification[] = [];
  const noteIcon = nativeImage.createFromPath(join(icons, "icon.png"));
  function notify(n: DesktopNotice): void {
    if (!Notification.isSupported()) return;
    // Don't interrupt someone looking at the canvas; the badge still counts it.
    if (alive(agentsWin) && agentsWin.isVisible() && agentsWin.isFocused()) return;
    const note = new Notification({ title: n.title, body: n.body ?? "", icon: noteIcon });
    note.on("click", () => deliver(n.thread ? { kind: "thread", thread: n.thread } : { kind: "agents" }));
    shown.push(note);
    if (shown.length > 20) shown.shift();
    note.show();
  }
  // ── menus, tray, badge ──
  const updater = await startUpdater(settings.autoUpdate, app.isPackaged, desktopLog);
  const actions: MenuActions = {
    openRoll: focusMain,
    openAgents: () => openAgents({ kind: "canvas" }),
    back: () => BrowserWindow.getFocusedWindow()?.webContents.navigationHistory.goBack(),
    forward: () => BrowserWindow.getFocusedWindow()?.webContents.navigationHistory.goForward(),
    pause: () => {
      state.paused = true;
      saveState();
      void supervisor?.stop();
      refresh();
    },
    resume: () => {
      state.paused = false;
      saveState();
      startDaemon();
      refresh();
    },
    signIn: () => {
      focusMain();
      signIn();
    },
    showLogs: () => void shell.openPath(agentLog.file),
    openSettings: () => void shell.openPath(settingsFile),
    openDocs: () => void shell.openExternal(`${settings.origin}/docs`),
    checkForUpdates: updater ? () => updater.check() : null,
    quit: () => app.quit(),
  };

  const model = (): TrayModel =>
    trayModel({
      daemon: supervisor?.state ?? { kind: "stopped" },
      link,
      missingBinary: !bin,
      external: !settings.daemon,
      login: loginState,
      paused: state.paused,
      status,
      machineId,
      now: Date.now(),
    });

  const badge = nativeImage.createFromPath(join(icons, "badge.png"));
  let last = "";
  function refresh(): void {
    const m = model();
    const key = JSON.stringify(m);
    if (key === last) return;
    last = key;
    Menu.setApplicationMenu(Menu.buildFromTemplate(appMenu(process.platform, m, actions)));
    if (tray) {
      tray.setContextMenu(Menu.buildFromTemplate(trayMenu(m, actions)));
      tray.setToolTip(m.tooltip);
    }
    if (process.platform === "win32") {
      for (const w of [win, agentsWin]) if (alive(w)) w.setOverlayIcon(m.pending ? badge : null, m.pending ? `${m.pending} waiting` : "");
    } else app.setBadgeCount(m.pending);
  }

  const trayIcon = nativeImage.createFromPath(join(icons, process.platform === "darwin" ? "trayTemplate.png" : "tray.png"));
  if (process.platform === "darwin") trayIcon.setTemplateImage(true);
  try {
    tray = new Tray(trayIcon);
    if (process.platform !== "darwin") tray.on("click", focusMain);
  } catch (e) {
    desktopLog.event(`no tray: ${(e as Error).message}`);
  }
  // The "retrying in N s" countdown needs a tick while the daemon is down.
  setInterval(() => {
    if (supervisor?.state.kind === "restarting") refresh();
  }, 1000).unref();

  // Everything is wired: from here on links and focus requests go straight to the windows.
  focusWindow = focusMain;
  route = openLink;
  const first = ensureMain();
  refresh();
  if (pendingLink) {
    // A link that launched the app opens its window on the link's URL.
    const link = pendingLink;
    pendingLink = null;
    openLink(link);
  }

  if (settings.daemon && bin) {
    if (loginState.kind === "signed_in") startDaemon();
    else first.webContents.once("did-finish-load", signIn);
  }

  // The dock icon shows roll; the Agents window has its own ways in.
  app.on("activate", focusMain);
  app.on("window-all-closed", () => {
    if (!tray) app.quit();
  });
  let stopped = false;
  app.on("before-quit", (e) => {
    quitting = true;
    if (stopped) return;
    e.preventDefault();
    login?.cancel();
    void (supervisor?.stop() ?? Promise.resolve()).finally(() => {
      stopped = true;
      writeState(stateFile, state);
      agentLog.close();
      desktopLog.close();
      app.quit();
    });
  });
}

function debounce(f: () => void, ms: number): () => void {
  let t: ReturnType<typeof setTimeout> | null = null;
  return () => {
    if (t) clearTimeout(t);
    t = setTimeout(() => {
      t = null;
      f();
    }, ms);
  };
}
