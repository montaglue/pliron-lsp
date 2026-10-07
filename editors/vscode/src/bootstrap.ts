// Locating the pliron-lsp server binary (like rust-analyzer's bootstrap):
// an explicit setting wins, then the binary bundled with the extension,
// then a development build in the workspace, then `~/.cargo/bin`, `PATH`.

import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import * as vscode from "vscode";

import { Config } from "./config";

export interface ServerLocation {
  path: string;
  /** Where it was found, for the status / logs. */
  source: "setting" | "bundled" | "workspace" | "cargo" | "PATH";
}

export function exeName(name: string): string {
  return process.platform === "win32" ? `${name}.exe` : name;
}

function isFile(p: string): boolean {
  try {
    return fs.statSync(p).isFile();
  } catch {
    return false;
  }
}

function onPath(name: string): string | undefined {
  for (const dir of (process.env.PATH ?? "").split(path.delimiter)) {
    const p = path.join(dir, name);
    if (dir && isFile(p)) {
      return p;
    }
  }
  return undefined;
}

export function findServer(
  context: vscode.ExtensionContext,
  config: Config
): ServerLocation | undefined {
  const configured = config.serverPath;
  if (configured) {
    return { path: configured, source: "setting" };
  }
  const name = exeName("pliron-lsp");
  const bundled = path.join(context.extensionPath, "server", name);
  if (isFile(bundled)) {
    return { path: bundled, source: "bundled" };
  }
  if (vscode.workspace.isTrusted) {
    for (const folder of vscode.workspace.workspaceFolders ?? []) {
      for (const profile of ["release", "debug"]) {
        const p = path.join(folder.uri.fsPath, "target", profile, name);
        if (isFile(p)) {
          return { path: p, source: "workspace" };
        }
      }
    }
  }
  const cargo = path.join(
    process.env.CARGO_HOME ?? path.join(os.homedir(), ".cargo"),
    "bin",
    name
  );
  if (isFile(cargo)) {
    return { path: cargo, source: "cargo" };
  }
  const p = onPath(name);
  return p ? { path: p, source: "PATH" } : undefined;
}
