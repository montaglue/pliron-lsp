// pliron-lsp extensions to the Language Server Protocol.

import * as lc from "vscode-languageclient/node";

export interface DocParams {
  textDocument?: lc.TextDocumentIdentifier;
}

/** Markdown report of projects, engines and indexes. */
export const analyzerStatus = new lc.RequestType<DocParams, string, void>(
  "pliron/analyzerStatus"
);
/** The engine's IR model of a document, as text. */
export const viewEngineModel = new lc.RequestType<DocParams, string, void>(
  "pliron/viewEngineModel"
);
/** The syntax layer's tree of a document, as text. */
export const viewSyntaxTree = new lc.RequestType<DocParams, string, void>(
  "pliron/viewSyntaxTree"
);
/** Ops / types / attributes found in the dialect sources (markdown). */
export const dialectRegistry = new lc.RequestType<DocParams, string, void>(
  "pliron/dialectRegistry"
);
/** Path of the generated bundle manifest serving a document, if any. */
export const bundleManifest = new lc.RequestType<DocParams, string | null, void>(
  "pliron/bundleManifest"
);
export const serverVersion = new lc.RequestType0<string, void>(
  "pliron/serverVersion"
);

/** Rebuild the dialect engines of all open projects. */
export const rebuild = new lc.NotificationType0("pliron/rebuild");

export interface Status {
  /** Project root, "reference", or absent. */
  engine?: string;
  state: "syntax-only" | "building" | "ready" | "error";
  message?: string;
}
export const status = new lc.NotificationType<Status>("pliron/status");

/** An engine analysis of a document finished. */
export const analysisUpdated = new lc.NotificationType<{ uri: string }>(
  "pliron/analysisUpdated"
);
