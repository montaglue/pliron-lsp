// "pliron: Run Pass…": run a pass of the document's dialect engine (pliron's
// own, or one a dialect registers with `pliron_lsp_api::pass!`) and show the
// IR before and after it as a diff.

import * as path from "path";
import * as vscode from "vscode";

import type { Ctx } from "./ctx";
import * as ext from "./lsp_ext";

export const PASS_SCHEME = "pliron-pass";

/** Read-only documents holding the printed IR before / after passes. */
class PassResults implements vscode.TextDocumentContentProvider {
  private readonly texts = new Map<string, string>();
  private readonly emitter = new vscode.EventEmitter<vscode.Uri>();
  readonly onDidChange = this.emitter.event;

  set(uri: vscode.Uri, text: string) {
    this.texts.set(uri.toString(), text);
    this.emitter.fire(uri);
  }

  provideTextDocumentContent(uri: vscode.Uri): string {
    return this.texts.get(uri.toString()) ?? "";
  }
}

export const passResults = new PassResults();

/** Run `pass` (asked for when not given) on the active pliron document. */
export async function runPass(ctx: Ctx, pass?: string): Promise<ext.PassResult | undefined> {
  const editor = ctx.activePlironEditor;
  const client = ctx.client;
  if (!editor || !client) {
    vscode.window.showInformationMessage("pliron: open a .pliron file first.");
    return undefined;
  }
  const textDocument = { uri: editor.document.uri.toString() };
  if (!pass) {
    let list: ext.PassList;
    try {
      list = await client.sendRequest(ext.listPasses, { textDocument });
    } catch (e) {
      vscode.window.showWarningMessage(`pliron: ${e instanceof Error ? e.message : e}`);
      return undefined;
    }
    if (list.passes.length === 0) {
      vscode.window.showInformationMessage(`pliron: the ${list.engine} engine has no passes.`);
      return undefined;
    }
    const pick = await vscode.window.showQuickPick(
      list.passes.map((p) => ({ label: p.name, detail: p.description })),
      { placeHolder: `A pass of the ${list.engine} engine`, matchOnDetail: true }
    );
    if (!pick) {
      return undefined;
    }
    pass = pick.label;
  }
  const name = pass;
  const result = await vscode.window.withProgress(
    { location: vscode.ProgressLocation.Notification, title: `pliron: running ${name}…` },
    () => client.sendRequest(ext.runPass, { textDocument, pass: name })
  );
  if (result.before == null || result.after == null) {
    vscode.window.showErrorMessage(`pliron: ${name}: ${result.errors.join("; ")}`);
    return result;
  }
  const file = path.basename(editor.document.uri.path);
  const run = String(Date.now());
  const uri = (side: string) =>
    vscode.Uri.from({ scheme: PASS_SCHEME, path: `/${file} (${side} ${name}).pliron`, query: run });
  const before = uri("before");
  const after = uri("after");
  passResults.set(before, result.before);
  passResults.set(after, result.after);
  await vscode.commands.executeCommand("vscode.diff", before, after, `${file}: ${name}`);
  if (result.errors.length > 0) {
    vscode.window.showWarningMessage(`pliron: ${name}: ${result.errors.join("; ")}`);
  }
  return result;
}
