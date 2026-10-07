// Runs the extension's integration tests inside a real VS Code instance.

import * as fs from "fs";
import * as os from "os";
import * as path from "path";

import { runTests } from "@vscode/test-electron";

async function main() {
  // When run from VS Code's integrated terminal, these variables would make
  // the test instance run as plain Node or attach to the parent window.
  for (const k of Object.keys(process.env)) {
    if (k === "ELECTRON_RUN_AS_NODE" || k.startsWith("VSCODE_")) {
      delete process.env[k];
    }
  }
  const extensionDevelopmentPath = path.resolve(__dirname, "../../");
  const extensionTestsPath = path.resolve(__dirname, "./suite/index");

  // A throwaway workspace with a pliron file, using the server built in
  // this repository (or $PLIRON_LSP_SERVER).
  const ws = fs.mkdtempSync(path.join(os.tmpdir(), "pliron-vscode-"));
  fs.copyFileSync(
    path.join(extensionDevelopmentPath, "test-fixtures", "demo.pliron"),
    path.join(ws, "demo.pliron")
  );
  const server =
    process.env.PLIRON_LSP_SERVER ??
    path.resolve(extensionDevelopmentPath, "../../target/debug/pliron-lsp");
  // With PLIRON_TEST_BUNDLED=1 no server path is configured, so the
  // extension must use the binaries bundled in `server/` (cargo xtask dist).
  if (!process.env.PLIRON_TEST_BUNDLED) {
    fs.mkdirSync(path.join(ws, ".vscode"));
    fs.writeFileSync(
      path.join(ws, ".vscode", "settings.json"),
      JSON.stringify({ "pliron.server.path": server }, null, 2)
    );
  }

  await runTests({
    extensionDevelopmentPath,
    extensionTestsPath,
    launchArgs: [ws, "--disable-extensions", "--disable-workspace-trust"],
    extensionTestsEnv: { PLIRON_TEST_WORKSPACE: ws },
  });
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
