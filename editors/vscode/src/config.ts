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
    "pliron.bundles.cacheDir",
    "pliron.diagnostics.roundTrip",
    "pliron.engine.idleTimeout",
  ];

  get serverPath(): string | undefined {
    return expand(this.cfg.get<string | null>("server.path") ?? undefined);
  }

  get serverExtraEnv(): Record<string, string> {
    return this.cfg.get<Record<string, string>>("server.extraEnv") ?? {};
  }

  /**
   * `PLIRON_LSP_CACHE_DIR` for `pliron.bundles.cacheDir`: nothing for
   * `shared` (the server's default, or the variable from the environment),
   * empty for the project's `target/`, else the directory.
   */
  get cacheDirEnv(): Record<string, string> {
    const dir = this.cfg.get<string>("bundles.cacheDir", "shared").trim();
    if (dir === "shared") {
      return {};
    }
    return { PLIRON_LSP_CACHE_DIR: expand(dir) ?? "" };
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

  get engineIdleTimeout(): number {
    return this.cfg.get<number>("engine.idleTimeout", 600);
  }

  get roundTrip(): boolean {
    return this.cfg.get<boolean>("diagnostics.roundTrip", true);
  }

  /** Options sent to the server in `initialize`. */
  initializationOptions(): object {
    return {
      enginePath: this.enginePath,
      disableEngine: !this.engineEnabled,
      // Building a dialect engine compiles the workspace's dialect crates
      // and runs their build scripts / proc macros: trusted workspaces only.
      disableBundles: !this.bundlesEnabled || !vscode.workspace.isTrusted,
      roundTrip: this.roundTrip,
      engineIdleTimeout: this.engineIdleTimeout,
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
