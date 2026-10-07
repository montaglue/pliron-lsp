// VS Code extension for pliron IR, backed by pliron-lsp.
//
// Like rust-analyzer's extension, it ships the server binaries (in
// `server/`), so nothing has to be installed separately. The server builds
// a dialect engine for the cargo project a `.pliron` file lives in, by
// itself.

import * as vscode from "vscode";

import { commands } from "./commands";
import { Config } from "./config";
import { Ctx } from "./ctx";
import { SCHEME, ViewProvider } from "./views";

let ctx: Ctx | undefined;

/** The API returned from `activate` (used by tests). */
export interface PlironExtensionApi {
  ctx: Ctx;
}

export async function activate(
  context: vscode.ExtensionContext
): Promise<PlironExtensionApi> {
  ctx = new Ctx(context);
  const c = ctx;
  context.subscriptions.push(c);

  const views = new ViewProvider(c);
  context.subscriptions.push(
    views,
    vscode.workspace.registerTextDocumentContentProvider(SCHEME, views)
  );

  for (const [name, factory] of Object.entries(commands)) {
    context.subscriptions.push(vscode.commands.registerCommand(name, factory(c)));
  }

  context.subscriptions.push(
    vscode.workspace.onDidChangeConfiguration(async (e) => {
      if (Config.restartKeys.some((k) => e.affectsConfiguration(k))) {
        const choice = await vscode.window.showInformationMessage(
          "pliron: the server must be restarted for this setting to take effect.",
          "Restart Now"
        );
        if (choice) {
          await c.restart();
        }
      }
    }),
    // Dialect engines are only built in trusted workspaces.
    vscode.workspace.onDidGrantWorkspaceTrust(() => c.restart())
  );

  await c.start();
  return { ctx: c };
}

export async function deactivate() {
  await ctx?.stop();
  ctx = undefined;
}
