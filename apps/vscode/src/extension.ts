// roll secrets for VS Code (docs/secrets.md, "VS Code"): which environments set the variables the
// code reads, a warning where the workspace's environment lacks one, names completed where a name
// goes, and in a `.env` file a value replaced by its `roll://` reference (or moved into roll first).
// Values are never shown, logged or kept: the extension reads only names and presence.
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import * as vscode from "vscode";
import { grid, RollError, setValue, type Conn } from "./api.ts";
import { completesNames, dotenvLines, envReads, hoverText, isDotenvFile, LANGUAGES, nearestSetup, presence, problems, rollRef, type GridLike, type Setup } from "./logic.ts";

const TOKEN = "roll.token";
const REFRESH_MS = 60_000;

type Where = { conn: Conn; name: string; env: string; path: string };

/** The CLI's setup file: `$XDG_CONFIG_HOME/roll/secrets.json`, as `roll secrets setup` writes it. */
function setups(): Record<string, Setup> {
  const base = process.env.XDG_CONFIG_HOME || path.join(os.homedir(), ".config");
  try {
    return JSON.parse(fs.readFileSync(path.join(base, "roll", "secrets.json"), "utf8")) as Record<string, Setup>;
  } catch {
    return {};
  }
}

export function activate(context: vscode.ExtensionContext) {
  const diagnostics = vscode.languages.createDiagnosticCollection("roll");
  const status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 50);
  status.command = "roll.chooseEnvironment";
  context.subscriptions.push(diagnostics, status);

  let cached: { key: string; at: number; grid: GridLike } | undefined;
  let warned = false;

  /** Where the workspace's secrets are: settings, else the CLI's setup for the folder; null without a token. */
  async function where(uri?: vscode.Uri): Promise<Where | null> {
    const folder = (uri && vscode.workspace.getWorkspaceFolder(uri)) ?? vscode.workspace.workspaceFolders?.[0];
    const cfg = vscode.workspace.getConfiguration("roll", folder);
    const setup = folder ? nearestSetup(setups(), folder.uri.fsPath) : undefined;
    const project = cfg.get<string>("project") || setup?.project;
    const token = (await context.secrets.get(TOKEN)) || process.env.ROLL_TOKEN;
    if (!project || !token) return null;
    const env = context.workspaceState.get<string>("roll.environment") || cfg.get<string>("environment") || setup?.env || "development";
    const api = cfg.get<string>("apiUrl") || process.env.ROLL_API_URL || "http://127.0.0.1:7070";
    return { conn: { api, token, project }, name: setup?.name ?? project, env, path: cfg.get<string>("path") || setup?.path || "/" };
  }

  /** The folder's grid, kept a minute (names and presence only). */
  async function current(uri?: vscode.Uri, fresh = false): Promise<{ w: Where; grid: GridLike } | null> {
    const w = await where(uri);
    if (!w) {
      status.text = "$(key) roll: sign in";
      status.command = "roll.signIn";
      status.show();
      return null;
    }
    const key = `${w.conn.api} ${w.conn.project} ${w.path}`;
    if (fresh || !cached || cached.key !== key || Date.now() - cached.at > REFRESH_MS) {
      try {
        cached = { key, at: Date.now(), grid: await grid(w.conn, w.path) };
        warned = false;
      } catch (e) {
        if (!warned) void vscode.window.showWarningMessage(`roll secrets: ${e instanceof Error ? e.message : "can't reach roll"}`);
        warned = true;
        return null;
      }
    }
    status.text = `$(key) roll: ${w.name} · ${w.env}`;
    status.tooltip = `roll secrets: ${w.name}${w.path === "/" ? "" : ` ${w.path}`}, ${w.env}. Click to choose the environment.`;
    status.command = "roll.chooseEnvironment";
    status.show();
    return { w, grid: cached.grid };
  }

  async function check(doc: vscode.TextDocument) {
    if (!LANGUAGES.includes(doc.languageId) || doc.uri.scheme !== "file") return;
    const at = await current(doc.uri);
    if (!at) return diagnostics.delete(doc.uri);
    const found = problems(envReads(doc.getText(), doc.languageId), at.grid, at.w.env).map((p) => {
      const d = new vscode.Diagnostic(new vscode.Range(doc.positionAt(p.start), doc.positionAt(p.end)), p.message, vscode.DiagnosticSeverity.Warning);
      d.source = "roll secrets";
      return d;
    });
    diagnostics.set(doc.uri, found);
  }

  const pending = new Map<string, ReturnType<typeof setTimeout>>();
  const soon = (doc: vscode.TextDocument) => {
    clearTimeout(pending.get(doc.uri.toString()));
    pending.set(
      doc.uri.toString(),
      setTimeout(() => void check(doc), 400),
    );
  };

  const code = LANGUAGES.map((language) => ({ language, scheme: "file" }));
  const dotenv: vscode.DocumentSelector = [{ scheme: "file", pattern: "**/.env" }, { scheme: "file", pattern: "**/.env.*" }];

  context.subscriptions.push(
    vscode.workspace.onDidOpenTextDocument(soon),
    vscode.workspace.onDidChangeTextDocument((e) => soon(e.document)),
    vscode.workspace.onDidCloseTextDocument((d) => diagnostics.delete(d.uri)),

    vscode.languages.registerHoverProvider(code, {
      async provideHover(doc, pos) {
        const offset = doc.offsetAt(pos);
        const read = envReads(doc.getText(), doc.languageId).find((r) => r.start <= offset && offset <= r.end);
        if (!read) return;
        const at = await current(doc.uri);
        if (!at) return;
        const md = new vscode.MarkdownString(hoverText(read.name, presence(at.grid, read.name), { project: at.w.name, env: at.w.env, path: at.w.path }));
        return new vscode.Hover(md, new vscode.Range(doc.positionAt(read.start), doc.positionAt(read.end)));
      },
    }),

    vscode.languages.registerCompletionItemProvider(
      code,
      {
        async provideCompletionItems(doc, pos) {
          if (!completesNames(doc.lineAt(pos.line).text.slice(0, pos.character), doc.languageId)) return;
          const at = await current(doc.uri);
          if (!at) return;
          return at.grid.rows.map((r) => {
            const item = new vscode.CompletionItem(r.key, vscode.CompletionItemKind.Variable);
            const p = presence(at.grid, r.key);
            item.detail = `roll · set in ${p.set.join(", ") || "no environment"}`;
            return item;
          });
        },
      },
      ".",
      '"',
      "'",
    ),

    vscode.languages.registerCodeActionsProvider(dotenv, {
      async provideCodeActions(doc, range) {
        if (!isDotenvFile(doc.fileName)) return;
        const line = dotenvLines(doc.getText()).find((l) => l.line === range.start.line);
        if (!line || line.reference || !line.value) return;
        const at = await current(doc.uri);
        if (!at) return;
        const ref = rollRef(at.w.env, at.w.path, line.key);
        if (!ref) return;
        const span = new vscode.Range(line.line, line.valueStart, line.line, line.valueEnd);
        const actions: vscode.CodeAction[] = [];
        if (presence(at.grid, line.key).set.includes(at.w.env)) {
          const a = new vscode.CodeAction(`Replace with ${ref}`, vscode.CodeActionKind.RefactorRewrite);
          a.edit = new vscode.WorkspaceEdit();
          a.edit.replace(doc.uri, span, ref);
          actions.push(a);
        }
        const move = new vscode.CodeAction(`Move to roll (${at.w.env}) and use ${ref}`, vscode.CodeActionKind.RefactorRewrite);
        move.command = { command: "roll.moveToRoll", title: "Move to roll", arguments: [doc.uri, line.line] };
        actions.push(move);
        return actions;
      },
    }),

    vscode.commands.registerCommand("roll.signIn", async () => {
      const token = await vscode.window.showInputBox({ title: "roll secrets", prompt: "A project token (rlst_…): roll secrets tokens create vscode --for-env development", password: true, ignoreFocusOut: true });
      if (!token) return;
      await context.secrets.store(TOKEN, token.trim());
      cached = undefined;
      await refreshAll(true);
    }),
    vscode.commands.registerCommand("roll.signOut", async () => {
      await context.secrets.delete(TOKEN);
      cached = undefined;
      diagnostics.clear();
      await current();
    }),
    vscode.commands.registerCommand("roll.refresh", () => refreshAll(true)),
    vscode.commands.registerCommand("roll.chooseEnvironment", async () => {
      const at = await current(undefined, true);
      if (!at) return;
      const env = await vscode.window.showQuickPick(at.grid.environments, { title: "roll secrets: the environment this workspace runs", placeHolder: at.w.env });
      if (!env) return;
      await context.workspaceState.update("roll.environment", env);
      await refreshAll(false);
    }),
    vscode.commands.registerCommand("roll.moveToRoll", async (uri: vscode.Uri, lineNo: number) => {
      const doc = await vscode.workspace.openTextDocument(uri);
      const line = dotenvLines(doc.getText()).find((l) => l.line === lineNo);
      const at = await current(uri, true);
      if (!line || !at) return;
      const ref = rollRef(at.w.env, at.w.path, line.key);
      if (!ref) return;
      const exists = presence(at.grid, line.key).set.includes(at.w.env);
      const ok = await vscode.window.showWarningMessage(
        `Set ${line.key} in ${at.w.name} ${at.w.env}${at.w.path === "/" ? "" : ` ${at.w.path}`} to this line's value${exists ? ", replacing roll's" : ""}, and write ${ref} here instead?`,
        { modal: true },
        "Move to roll",
      );
      if (ok !== "Move to roll") return;
      try {
        await setValue(at.w.conn, at.w.env, at.w.path, line.key, line.value);
      } catch (e) {
        const why = e instanceof RollError && e.code === "approval_required" ? `${at.w.env} takes changes only through approved requests: open one in the dashboard` : e instanceof Error ? e.message : "it failed";
        void vscode.window.showErrorMessage(`roll secrets: ${line.key} wasn't moved (${why}).`);
        return;
      }
      const edit = new vscode.WorkspaceEdit();
      edit.replace(uri, new vscode.Range(line.line, line.valueStart, line.line, line.valueEnd), ref);
      await vscode.workspace.applyEdit(edit);
      void vscode.window.showInformationMessage(`roll secrets: ${line.key} is in ${at.w.env}; this file now holds ${ref}. Save it, and run with roll run --env-file ${path.basename(doc.fileName)}.`);
      // What the open files read has changed: their warnings too.
      await refreshAll(true);
    }),
    vscode.workspace.onDidChangeConfiguration((e) => e.affectsConfiguration("roll") && refreshAll(true)),
  );

  async function refreshAll(fresh: boolean) {
    if (fresh) cached = undefined;
    await current();
    for (const doc of vscode.workspace.textDocuments) await check(doc);
  }
  void refreshAll(false);
}

export function deactivate() {}
