// Integration tests: the extension starts pliron-lsp, which analyzes the
// document with the real pliron parsers (reference engine).

import * as assert from "assert";
import * as path from "path";
import * as vscode from "vscode";

const ws = process.env.PLIRON_TEST_WORKSPACE!;
const docUri = vscode.Uri.file(path.join(ws, "demo.pliron"));

function sleep(ms: number) {
  return new Promise((r) => setTimeout(r, ms));
}

async function until<T>(what: string, f: () => Promise<T | undefined>, ms = 60_000): Promise<T> {
  const deadline = Date.now() + ms;
  for (;;) {
    const v = await f().catch(() => undefined);
    if (v !== undefined) {
      return v;
    }
    if (Date.now() > deadline) {
      throw new Error(`timed out waiting for ${what}`);
    }
    await sleep(250);
  }
}

function positionOf(doc: vscode.TextDocument, needle: string, delta = 0): vscode.Position {
  const off = doc.getText().indexOf(needle);
  assert.ok(off >= 0, `${needle} not found`);
  return doc.positionAt(off + delta);
}

function hoverText(hovers: vscode.Hover[]): string {
  return hovers
    .flatMap((h) => h.contents)
    .map((c) => (typeof c === "string" ? c : "value" in c ? c.value : ""))
    .join("\n");
}

suite("pliron extension", () => {
  let doc: vscode.TextDocument;

  suiteSetup(async () => {
    doc = await vscode.workspace.openTextDocument(docUri);
    await vscode.window.showTextDocument(doc);
    const ext = vscode.extensions.getExtension("pliron-lsp.pliron");
    assert.ok(ext, "extension not found");
    await ext.activate();
    assert.strictEqual(doc.languageId, "pliron");
  });

  test("hover shows the exact type computed by the dialect parser", async () => {
    const text = await until("exact hover", async () => {
      const hovers = await vscode.commands.executeCommand<vscode.Hover[]>(
        "vscode.executeHoverProvider",
        docUri,
        positionOf(doc, "r = llvm.call")
      );
      const t = hoverText(hovers ?? []);
      return t.includes("result #0 of `llvm.call`") ? t : undefined;
    });
    assert.ok(text.includes("r: builtin.integer i64"), text);
  });

  test("go to definition of a value", async () => {
    const defs = await vscode.commands.executeCommand<(vscode.Location | vscode.LocationLink)[]>(
      "vscode.executeDefinitionProvider",
      docUri,
      positionOf(doc, "z) :")
    );
    assert.strictEqual(defs.length, 1);
    const d = defs[0];
    const range = "range" in d ? d.range : d.targetRange;
    assert.deepStrictEqual(range.start, positionOf(doc, "z = llvm.add"));
  });

  test("semantic tokens and inlay hints", async () => {
    const tokens = await vscode.commands.executeCommand<vscode.SemanticTokens>(
      "vscode.provideDocumentSemanticTokens",
      docUri
    );
    assert.ok(tokens && tokens.data.length > 50, "expected semantic tokens");
    const hints = await vscode.commands.executeCommand<vscode.InlayHint[]>(
      "vscode.executeInlayHintProvider",
      docUri,
      new vscode.Range(0, 0, doc.lineCount, 0)
    );
    const labels = hints.map((h) => (typeof h.label === "string" ? h.label : h.label.map((p) => p.value).join("")));
    assert.ok(labels.includes(": builtin.integer i64"), JSON.stringify(labels));
  });

  test("diagnostics come from the real parser", async () => {
    const edit = new vscode.WorkspaceEdit();
    const p = positionOf(doc, "llvm.add");
    edit.replace(docUri, new vscode.Range(p, p.translate(0, "llvm.add".length)), "llvm.ad");
    await vscode.workspace.applyEdit(edit);
    const diags = await until("diagnostics", async () => {
      const d = vscode.languages.getDiagnostics(docUri);
      return d.some((x) => x.message.includes("Unregistered Op llvm.ad")) ? d : undefined;
    });
    assert.ok(diags.length >= 1);
    await vscode.commands.executeCommand("undo");
  });

  test("views: engine model and status", async () => {
    await vscode.window.showTextDocument(doc);
    await vscode.commands.executeCommand("pliron.viewEngineModel");
    const model = await until("model view", async () => {
      const d = vscode.workspace.textDocuments.find(
        (t) => t.uri.scheme === "pliron-view" && t.uri.query.includes("kind=model")
      );
      const t = d?.getText() ?? "";
      return t.includes("llvm.call") && !t.includes("stale") && !t.includes("error:") ? t : undefined;
    });
    assert.ok(model.includes("r: builtin.integer i64 = llvm.call (z)"), model);
    await vscode.commands.executeCommand("pliron.serverVersion");
  });

  test("reference counts above functions", async () => {
    const lenses = await until("code lenses", async () => {
      const l = await vscode.commands.executeCommand<vscode.CodeLens[]>(
        "vscode.executeCodeLensProvider",
        docUri
      );
      return l && l.length === 2 ? l : undefined;
    });
    assert.deepStrictEqual(
      lenses.map((l) => l.command?.title),
      ["1 reference", "0 references"]
    );
    const cmd = lenses[0].command!;
    assert.strictEqual(cmd.command, "pliron.showReferences");
    // Opens the reference peek without throwing.
    await vscode.commands.executeCommand(cmd.command, ...(cmd.arguments ?? []));
  });

  test("rename: symbols, and local names with scope checks", async () => {
    const edit = await vscode.commands.executeCommand<vscode.WorkspaceEdit>(
      "vscode.executeDocumentRenameProvider",
      docUri,
      positionOf(doc, "@callee:", 1),
      "target"
    );
    assert.strictEqual(edit.get(docUri).length, 2);
    const local = await vscode.commands.executeCommand<vscode.WorkspaceEdit>(
      "vscode.executeDocumentRenameProvider",
      docUri,
      positionOf(doc, "z = llvm"),
      "sum"
    );
    assert.strictEqual(local.get(docUri).length, 3);
    await assert.rejects(
      Promise.resolve(
        vscode.commands.executeCommand(
          "vscode.executeDocumentRenameProvider",
          docUri,
          positionOf(doc, "z = llvm"),
          "y2"
        )
      ),
      /already names another value/
    );
  });

  test("source locations are links", async () => {
    const uri = vscode.Uri.file(path.join(ws, "located.pliron"));
    await vscode.workspace.openTextDocument(uri);
    const links = await until("links", async () => {
      const l = await vscode.commands.executeCommand<vscode.DocumentLink[]>(
        "vscode.executeLinkProvider",
        uri
      );
      return l && l.length > 0 ? l : undefined;
    });
    const target = links[0].target!;
    assert.ok(target.fsPath.endsWith(path.join("src", "kernel.rs")), target.toString());
    assert.strictEqual(target.fragment, "L2,5");
    // Following the link opens the file at that line and column.
    await vscode.commands.executeCommand("vscode.open", target);
    const editor = await until("kernel.rs editor", async () => {
      const e = vscode.window.activeTextEditor;
      return e?.document.uri.fsPath.endsWith("kernel.rs") ? e : undefined;
    });
    assert.deepStrictEqual(
      [editor.selection.active.line, editor.selection.active.character],
      [1, 4]
    );
  });

  test("printed form view", async () => {
    await vscode.window.showTextDocument(doc);
    await vscode.commands.executeCommand("pliron.viewPrinted");
    const printed = await until("printed view", async () => {
      const d = vscode.workspace.textDocuments.find(
        (t) => t.uri.scheme === "pliron-view" && t.uri.query.includes("kind=printed")
      );
      const t = d?.getText() ?? "";
      return t.includes("outlined_attributes:") ? t : undefined;
    });
    assert.ok(printed.includes("llvm.call @callee"), printed);
  });

  test("server location", async () => {
    const api = vscode.extensions.getExtension("pliron-lsp.pliron")!.exports;
    const source = api.ctx.server?.source;
    if (process.env.PLIRON_TEST_BUNDLED) {
      assert.strictEqual(source, "bundled");
    } else {
      assert.strictEqual(source, "setting");
    }
  });
});
