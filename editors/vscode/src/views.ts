// Read-only virtual documents backed by server requests (like
// rust-analyzer's "View Syntax Tree" / "Status"): they refresh while you
// edit the source document.

import * as vscode from "vscode";
import * as lc from "vscode-languageclient/node";

import type { Ctx } from "./ctx";
import * as ext from "./lsp_ext";

export const SCHEME = "pliron-view";

export type ViewKind = "model" | "syntax" | "registry" | "status";

const REQUESTS: Record<ViewKind, lc.RequestType<ext.DocParams, string, void>> = {
  model: ext.viewEngineModel,
  syntax: ext.viewSyntaxTree,
  registry: ext.dialectRegistry,
  status: ext.analyzerStatus,
};

const TITLES: Record<ViewKind, string> = {
  model: "Engine Model",
  syntax: "Syntax Tree",
  registry: "Dialect Registry",
  status: "Status",
};

export class ViewProvider implements vscode.TextDocumentContentProvider, vscode.Disposable {
  private readonly emitter = new vscode.EventEmitter<vscode.Uri>();
  readonly onDidChange = this.emitter.event;
  private readonly open = new Set<string>();
  private readonly disposables: vscode.Disposable[] = [];
  private timer: NodeJS.Timeout | undefined;

  constructor(private readonly ctx: Ctx) {
    this.disposables.push(
      vscode.workspace.onDidChangeTextDocument((e) => {
        if (e.document.languageId === "pliron") {
          this.scheduleRefresh(e.document.uri.toString());
        }
      }),
      vscode.workspace.onDidCloseTextDocument((d) => {
        if (d.uri.scheme === SCHEME) {
          this.open.delete(d.uri.toString());
        }
      })
    );
    // Engine results arrive asynchronously.
    ctx.onAnalysis((uri) => this.refresh(uri, 0));
    ctx.onStatus(() => this.scheduleRefresh(undefined));
  }

  /** The virtual document URI for a view of `source`. */
  static uri(kind: ViewKind, source: vscode.Uri | undefined): vscode.Uri {
    const ext = kind === "registry" || kind === "status" ? "md" : "txt";
    const query = source ? `source=${encodeURIComponent(source.toString())}` : "";
    return vscode.Uri.from({
      scheme: SCHEME,
      path: `/${TITLES[kind]}.${ext}`,
      query: `kind=${kind}&${query}`,
    });
  }

  private scheduleRefresh(source: string | undefined) {
    this.refresh(source, 300);
  }

  /** Refresh the open views of `source` (all views if undefined). */
  private refresh(source: string | undefined, delay: number) {
    if (this.timer) {
      clearTimeout(this.timer);
    }
    this.timer = setTimeout(() => {
      for (const u of this.open) {
        const uri = vscode.Uri.parse(u);
        const s = new URLSearchParams(uri.query).get("source");
        if (!source || !s || s === source) {
          this.emitter.fire(uri);
        }
      }
    }, delay);
  }

  async provideTextDocumentContent(uri: vscode.Uri): Promise<string> {
    this.open.add(uri.toString());
    const params = new URLSearchParams(uri.query);
    const kind = (params.get("kind") ?? "status") as ViewKind;
    const source = params.get("source");
    const client = this.ctx.client;
    if (!client) {
      return "The pliron language server is not running.";
    }
    const req: ext.DocParams = source ? { textDocument: { uri: source } } : {};
    try {
      return await client.sendRequest(REQUESTS[kind], req);
    } catch (e) {
      return `Request failed: ${e}`;
    }
  }

  dispose() {
    this.emitter.dispose();
    for (const d of this.disposables) {
      d.dispose();
    }
  }
}

/** Open a view beside the current editor. */
export async function showView(kind: ViewKind, source: vscode.Uri | undefined) {
  const uri = ViewProvider.uri(kind, source);
  if (kind === "registry" || kind === "status") {
    try {
      await vscode.commands.executeCommand("markdown.showPreviewToSide", uri);
      return;
    } catch {
      // Fall back to showing the markdown source.
    }
  }
  const doc = await vscode.workspace.openTextDocument(uri);
  await vscode.window.showTextDocument(doc, {
    viewColumn: vscode.ViewColumn.Beside,
    preserveFocus: true,
    preview: false,
  });
}
