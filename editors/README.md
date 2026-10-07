# Editor setup

`pliron-lsp` is a standard stdio language server. Build or install it:

```sh
cargo install --path crates/pliron-lsp            # the server
cargo install --path crates/pliron-lsp-engine-ref # reference engine (builtin + llvm)
```

The reference engine must sit next to the `pliron-lsp` binary (both end up
in `~/.cargo/bin`), or be given via `PLIRON_LSP_ENGINE` / the `enginePath`
initialization option. Project dialect engines are built automatically.

## VS Code

```sh
cargo xtask dist   # bundles the server into dist/pliron-<platform>.vsix
code --install-extension dist/pliron-<platform>.vsix
```

See [vscode/README.md](vscode/README.md) for commands and settings.

## Neovim (0.10+)

```lua
vim.filetype.add({ extension = { pliron = "pliron", plir = "pliron" } })
vim.api.nvim_create_autocmd("FileType", {
  pattern = "pliron",
  callback = function(ev)
    vim.lsp.start({
      name = "pliron-lsp",
      cmd = { "pliron-lsp" },
      root_dir = vim.fs.root(ev.buf, { "Cargo.toml", ".git" }),
    })
  end,
})
```

## Helix (`languages.toml`)

```toml
[language-server.pliron-lsp]
command = "pliron-lsp"

[[language]]
name = "pliron"
scope = "source.pliron"
file-types = ["pliron", "plir"]
roots = ["Cargo.toml"]
language-servers = ["pliron-lsp"]
```

## Initialization options

| Option | Meaning |
|---|---|
| `enginePath` | Use this engine binary for every document (no automatic bundles). |
| `disableEngine` | Syntax layer only. |
| `disableBundles` | Never build project engines (reference engine only). |
