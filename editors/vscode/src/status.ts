// The status bar item (rust-analyzer style): shows what the server is doing
// and opens a menu of actions when clicked.

import * as path from "path";
import * as vscode from "vscode";

import type { Status } from "./lsp_ext";

export type ServerState = "starting" | "running" | "stopped" | "failed";

export class StatusBar implements vscode.Disposable {
  private readonly item: vscode.StatusBarItem;
  private server: ServerState = "stopped";
  private engines = new Map<string, Status>();

  constructor() {
    this.item = vscode.window.createStatusBarItem(
      "pliron.status",
      vscode.StatusBarAlignment.Left,
      10
    );
    this.item.name = "pliron";
    this.item.command = "pliron.openMenu";
    this.render();
    this.item.show();
  }

  setServer(state: ServerState) {
    this.server = state;
    if (state !== "running") {
      this.engines.clear();
    }
    this.render();
  }

  onStatus(s: Status) {
    this.engines.set(s.engine ?? "", s);
    this.render();
  }

  private render() {
    const statuses = [...this.engines.values()];
    const building = statuses.filter((s) => s.state === "building");
    const errors = statuses.filter((s) => s.state === "error");
    let icon = "$(check)";
    let text = "pliron";
    this.item.backgroundColor = undefined;
    if (this.server === "starting") {
      icon = "$(loading~spin)";
    } else if (this.server === "stopped") {
      icon = "$(stop-circle)";
    } else if (this.server === "failed") {
      icon = "$(error)";
      this.item.backgroundColor = new vscode.ThemeColor(
        "statusBarItem.errorBackground"
      );
    } else if (building.length > 0) {
      icon = "$(loading~spin)";
      text = "pliron: building engine";
    } else if (errors.length > 0) {
      icon = "$(warning)";
      this.item.backgroundColor = new vscode.ThemeColor(
        "statusBarItem.warningBackground"
      );
    } else if (statuses.some((s) => s.state === "syntax-only")) {
      icon = "$(symbol-structure)";
    }
    this.item.text = `${icon} ${text}`;

    const md = new vscode.MarkdownString("", true);
    md.isTrusted = true;
    md.appendMarkdown(`**pliron-lsp** — server ${this.server}\n\n`);
    for (const s of statuses) {
      const name = s.engine ? path.basename(s.engine) : "server";
      md.appendMarkdown(`- **${name}**: ${s.state}`);
      if (s.message) {
        md.appendText(` — ${s.message.split("\n")[0]}`);
      }
      md.appendMarkdown("\n");
    }
    md.appendMarkdown(
      "\n[Status](command:pliron.showStatus) · [Rebuild](command:pliron.rebuildEngine) · " +
        "[Restart](command:pliron.restartServer) · [Logs](command:pliron.showLogs)"
    );
    this.item.tooltip = md;
  }

  dispose() {
    this.item.dispose();
  }
}
