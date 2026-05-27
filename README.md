# Tomlizer

`tomlizer` is a small Rust CLI utility for scanning `Cargo.toml` manifests and normalizing repeated dependency declarations into workspace dependencies.

## Features

- Scans every `Cargo.toml` in a repository tree
- Detects repeated crate dependency declarations used across multiple child manifests
- Suggests parent `workspace.dependencies` entries and child manifest updates
- Applies fixes automatically with `--yes`
- Uses `clap` for CLI parsing, `colors` for terminal output, and `spinners` for progress feedback
- Preserves existing TOML structure using `toml_edit`

## Usage

From the repository root:

```bash
cargo run --manifest-path Cargo.toml -- scan
```

To apply suggested fixes:

```bash
cargo run --manifest-path Cargo.toml -- apply
```

To apply without confirmation:

```bash
cargo run --manifest-path Cargo.toml -- apply --yes
```

## Options

- `--root <PATH>`: scan a different root directory (defaults to current directory)
- `scan`: print suggestions only
- `apply`: modify `Cargo.toml` files based on the suggested normalization

## Notes

- The tool currently scans `dependencies`, `dev-dependencies`, `build-dependencies`, and `target.*` sections.
- It only updates child manifests when a dependency is already available in the root workspace or can be added there.

## Development

Build and run locally:

```bash
cargo build
cargo run -- scan
```
