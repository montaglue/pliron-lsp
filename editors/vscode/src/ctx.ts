// The extension context: owns the language client and its lifecycle.

import * as vscode from "vscode";
import * as lc from "vscode-languageclient/node";

import { findServer, ServerLocation } from "./bootstrap";
import { Config } from "./config";
import * as ext from "./lsp_ext";
import { StatusBar } from "./status";

export class Ctx implements vscode.Disposable {
  client: lc.LanguageClient | undefined;
  server: ServerLocation | undefined;
  readonly config = new Config();
  readonly output: vscode.OutputChannel;
  readonly traceOutput: vscode.OutputChannel;
  readonly status: StatusBar;
  private readonly statusListeners: ((s: ext.Status) => void)[] = [];
  private readonly analysisListeners: ((uri: string) => void)[] = [];

  constructor(readonly extCtx: vscode.ExtensionContext) {
    this.output = vscode.window.createOutputChannel("pliron Language Server");
    this.traceOutput = vscode.window.createOutputChannel("pliron Language Server Trace");
    this.status = new StatusBar();
  }

  /** Called with every `pliron/status` notification. */
  onStatus(listener: (s: ext.Status) => void) {
    this.statusListeners.push(listener);
  }

  /** Called whenever the server finished analyzing a document. */
  onAnalysis(listener: (uri: string) => void) {
    this.analysisListeners.push(listener);
  }

  async start(): Promise<void> {
    if (this.client) {
      return;
    }
    this.server = findServer(this.extCtx, this.config);
    if (!this.server) {
      this.status.setServer("failed");
      const choice = await vscode.window.showErrorMessage(
        "pliron: the pliron-lsp server was not found. Install it with " +
          "`cargo install --path crates/pliron-lsp` or set `pliron.server.path`.",
        "Open Settings"
      );
      if (choice) {
        vscode.commands.executeCommand(
          "workbench.action.openSettings",
          "pliron.server.path"
        );
      }
      return;
    }
    this.output.appendLine(
      `Starting ${this.server.path} (found via ${this.server.source})`
    );
    const run: lc.Executable = {
      command: this.server.path,
      options: { env: { ...process.env, ...this.config.serverExtraEnv } },
    };
    const clientOptions: lc.LanguageClientOptions = {
      documentSelector: [
        { scheme: "file", language: "pliron" },
        { scheme: "untitled", language: "pliron" },
      ],
      initializationOptions: this.config.initializationOptions(),
      outputChannel: this.output,
      traceOutputChannel: this.traceOutput,
    };
    const client = new lc.LanguageClient(
      "pliron",
      "pliron Language Server",
      { run, debug: run },
      clientOptions
    );
    client.onDidChangeState((e) => {
      if (e.newState === lc.State.Running) {
        this.status.setServer("running");
      } else if (e.newState === lc.State.Stopped) {
        this.status.setServer("stopped");
      }
    });
    this.client = client;
    this.status.setServer("starting");
    try {
      await client.start();
      client.onNotification(ext.status, (s) => {
        this.status.onStatus(s);
        for (const l of this.statusListeners) {
          l(s);
        }
      });
      client.onNotification(ext.analysisUpdated, ({ uri }) => {
        for (const l of this.analysisListeners) {
          l(uri);
        }
      });
      this.status.setServer("running");
    } catch (e) {
      this.status.setServer("failed");
      this.client = undefined;
      vscode.window.showErrorMessage(`pliron: failed to start the server: ${e}`);
    }
  }

  async stop(): Promise<void> {
    const client = this.client;
    this.client = undefined;
    if (client) {
      await client.stop().catch(() => {});
      client.dispose();
    }
    this.status.setServer("stopped");
  }

  async restart(): Promise<void> {
    await this.stop();
    await this.start();
  }

  /** The active pliron editor, if any. */
  get activePlironEditor(): vscode.TextEditor | undefined {
    const e = vscode.window.activeTextEditor;
    return e?.document.languageId === "pliron" ? e : undefined;
  }

  dispose() {
    this.stop();
    this.status.dispose();
    this.output.dispose();
    this.traceOutput.dispose();
  }
}
