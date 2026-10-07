// Typed access to the `pliron.*` settings.

import * as os from "os";
import * as vscode from "vscode";

export class Config {
  private get cfg() {
    return vscode.workspace.getConfiguration("pliron");
  }

  /** Settings whose change requires restarting the server. */
  static readonly restartKeys = [
    "pliron.server.path",
    "pliron.server.extraEnv",
    "pliron.engine.enabled",
    "pliron.engine.path",
    "pliron.bundles.enabled",
  ];

  get serverPath(): string | undefined {
    return expand(this.cfg.get<string | null>("server.path") ?? undefined);
  }

  get serverExtraEnv(): Record<string, string> {
    return this.cfg.get<Record<string, string>>("server.extraEnv") ?? {};
  }

  get engineEnabled(): boolean {
    return this.cfg.get<boolean>("engine.enabled", true);
  }

  get enginePath(): string | undefined {
    return expand(this.cfg.get<string | null>("engine.path") ?? undefined);
  }

  get bundlesEnabled(): boolean {
    return this.cfg.get<boolean>("bundles.enabled", true);
  }

  /** Options sent to the server in `initialize`. */
  initializationOptions(): object {
    return {
      enginePath: this.enginePath,
      disableEngine: !this.engineEnabled,
      // Building a dialect engine compiles the workspace's dialect crates
      // and runs their build scripts / proc macros: trusted workspaces only.
      disableBundles: !this.bundlesEnabled || !vscode.workspace.isTrusted,
    };
  }
}

/** Expand `${workspaceFolder}`, `${userHome}` and a leading `~`. */
export function expand(p: string | undefined): string | undefined {
  if (!p) {
    return undefined;
  }
  const folder = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath ?? "";
  return p
    .replace(/\$\{workspaceFolder\}/g, folder)
    .replace(/\$\{userHome\}/g, os.homedir())
    .replace(/^~(?=$|[\\/])/, os.homedir());
}
