// Command implementations.

import * as vscode from "vscode";
import type * as lc from "vscode-languageclient/node";

import type { Ctx } from "./ctx";
import * as ext from "./lsp_ext";
import { showView, ViewKind } from "./views";

type Cmd = (ctx: Ctx) => (...args: unknown[]) => unknown;

function view(kind: ViewKind, needsDocument: boolean): Cmd {
  return (ctx) => async () => {
    const editor = ctx.activePlironEditor;
    if (needsDocument && !editor) {
      vscode.window.showInformationMessage("pliron: open a .pliron file first.");
      return;
    }
    await showView(kind, editor?.document.uri);
  };
}

export const commands: Record<string, Cmd> = {
  "pliron.restartServer": (ctx) => () => ctx.restart(),
  "pliron.stopServer": (ctx) => () => ctx.stop(),
  "pliron.startServer": (ctx) => () => ctx.start(),

  "pliron.rebuildEngine": (ctx) => () => {
    if (!ctx.client) {
      vscode.window.showWarningMessage("pliron: the server is not running.");
      return;
    }
    ctx.client.sendNotification(ext.rebuild);
  },

  "pliron.showStatus": view("status", false),
  "pliron.viewEngineModel": view("model", true),
  "pliron.viewSyntaxTree": view("syntax", true),
  "pliron.showRegistry": view("registry", true),

  "pliron.openBundleManifest": (ctx) => async () => {
    const editor = ctx.activePlironEditor;
    if (!ctx.client || !editor) {
      return;
    }
    const manifest = await ctx.client.sendRequest(ext.bundleManifest, {
      textDocument: { uri: editor.document.uri.toString() },
    });
    if (!manifest) {
      vscode.window.showInformationMessage(
        "pliron: this file is not served by a generated dialect engine."
      );
      return;
    }
    await vscode.window.showTextDocument(vscode.Uri.file(manifest));
  },

  "pliron.showLogs": (ctx) => () => ctx.output.show(true),

  // Run by the "N references" code lenses. The server sends LSP JSON;
  // VS Code's reference peek wants its own types.
  "pliron.showReferences": (ctx) => async (...args: unknown[]) => {
    const [uri, position, locations] = args as [string, lc.Position, lc.Location[]];
    const c = ctx.client?.protocol2CodeConverter;
    if (!c) {
      return;
    }
    await vscode.commands.executeCommand(
      "editor.action.showReferences",
      vscode.Uri.parse(uri),
      c.asPosition(position),
      locations.map((l) => c.asLocation(l))
    );
  },

  "pliron.serverVersion": (ctx) => async () => {
    if (!ctx.client) {
      vscode.window.showWarningMessage("pliron: the server is not running.");
      return;
    }
    const version = await ctx.client.sendRequest(ext.serverVersion);
    vscode.window.showInformationMessage(
      `pliron-lsp ${version} (${ctx.server?.source}: ${ctx.server?.path})`
    );
  },

  "pliron.openMenu": (ctx) => async () => {
    const items: (vscode.QuickPickItem & { command: string })[] = [
      { label: "$(info) Show Status", command: "pliron.showStatus" },
      { label: "$(list-tree) View Engine Model", command: "pliron.viewEngineModel" },
      { label: "$(symbol-structure) View Syntax Tree", command: "pliron.viewSyntaxTree" },
      { label: "$(book) Show Dialect Registry", command: "pliron.showRegistry" },
      { label: "$(tools) Rebuild Dialect Engine", command: "pliron.rebuildEngine" },
      { label: "$(file-code) Open Generated Bundle Manifest", command: "pliron.openBundleManifest" },
      { label: "$(debug-restart) Restart Server", command: "pliron.restartServer" },
      ctx.client
        ? { label: "$(debug-stop) Stop Server", command: "pliron.stopServer" }
        : { label: "$(debug-start) Start Server", command: "pliron.startServer" },
      { label: "$(output) Show Logs", command: "pliron.showLogs" },
    ];
    const pick = await vscode.window.showQuickPick(items, { title: "pliron" });
    if (pick) {
      await vscode.commands.executeCommand(pick.command);
    }
  },
};
