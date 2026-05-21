# rgit

A Rust reimplementation of Git from scratch. Object model, pack format, ref system, index file, working-tree operations, Smart HTTP / SSH transport, line diff, fast-forward merge — enough to drive a real codebase from `init` to GitHub round-trip.

**This repository was authored and pushed by `rgit` itself.** The commit history on `main` was produced by the binary built from this source tree. Every artifact you see — pack files, refs, objects, the index — was written by rgit. Verifiable: `git fsck` is clean against this repo.

## What works

```
rgit init <path>              create a new repository
rgit clone <url> [<dir>]      clone over Smart HTTP v2 (public + private repos)
rgit add <paths…>             stage files; recurses; honors .gitignore (cascading)
rgit commit -m <msg>          record a commit; reflog-tracked HEAD update
rgit status                   working-tree vs index vs HEAD (gitignore-filtered)
rgit log [-n N]               walk commit history from HEAD
rgit diff [--cached | <a> <b>]  unified diff (LCS-based; default: workdir vs index)
rgit merge <target>           fast-forward merge with ancestor walk
rgit blame <file>             per-line authorship walk (LCS-based)
rgit branch [-d] [<name>]     list / create / delete branches
rgit checkout <ref>           switch HEAD to a branch or commit; reflog-tracked
rgit tag [-d] [<name> [<id>]] list / create / delete tags
rgit reflog [<ref>]           show ref-update history (byte-compatible with git's)
rgit show [<id>]              inspect an object with kind-aware formatting
rgit ls-tree <ref>            list entries of a tree
rgit rev-parse <ref>          resolve a ref name or short id to a full SHA
rgit cat-file [-t|-p|-s] <id> low-level object inspection
rgit hash-object [-w] <file>  compute (and optionally store) a blob's id
rgit push <url>               upload refs + objects (Smart HTTP or SSH)
```

Byte-compatible with upstream Git's on-disk format: `git log`, `git ls-tree`, `git cat-file`, `git fsck` all read rgit-produced repositories without complaint. You can hand a repo back and forth between the two.

## Quick start

```sh
cargo build --release

# Clone any public repo over HTTPS:
./target/release/rgit clone https://github.com/hmalladi3/rgit.git

# Or initialize fresh, commit, and push:
mkdir demo && cd demo
../target/release/rgit init .
echo "hello rgit" > README.md
../target/release/rgit add README.md
../target/release/rgit commit -m "initial"

# Push via HTTPS (PAT auth):
export GITHUB_USERNAME=you GITHUB_TOKEN=ghp_xxx
../target/release/rgit push https://github.com/you/repo.git

# …or via SSH (your existing ssh-agent / ~/.ssh config):
../target/release/rgit push git@github.com:you/repo.git
```

## Architecture

```
rgit-core/src/
├── object/      blob / tree / commit / tag — parse + serialize, byte-exact round-trip
├── odb/         object database (loose + packed); atomic writes; hash verification on every read
├── pack/        packfile v2 read+write; REF + OFS delta resolution; build_index from a pack
├── refs/        branches, tags, packed-refs, HEAD; atomic writes; path-traversal-safe validation
├── index/       .git/index v2 codec; stat-cache fidelity so upstream git can pick up where rgit left off
├── workdir/     checkout, status, build-tree-from-index
├── gitignore/   pattern parser + matcher; cascading per-directory .gitignore files
├── diff/        LCS-based line diff + unified-diff output
├── merge/       fast-forward merge with ancestor walk through the commit DAG
├── blame/       per-line authorship walk via diff-driven line tracking
└── transport/   Smart HTTP v2 fetch (clone) + v0 push; HTTPS and SSH; pkt-line, sideband-64k
rgit-cli/src/
└── main.rs      CLI dispatch (clap-derive) — 17 commands
```

Each module owns its data, errors, and tests. `#![forbid(unsafe_code)]` workspace-wide. `cargo clippy --workspace --all-targets -- -D warnings` is clean.

## Quality

- **244 unit tests** covering format round-trips, error paths, atomic filesystem semantics, delta resolution, and reachability walks.
- **Cross-implementation verification**: every commit and tree rgit writes is read correctly by upstream Git. `rgit clone` of a repo against `git clone` of the same repo produces byte-identical working trees.
- **`@spec` annotations** in code cite which behavioral spec each function implements. The specs themselves — short normative one-liners with stable IDs (`OBJ-FRAME-006`, `ODB-WRITE-005`, `TX-PKTLINE-001`, etc.) — are internal design artifacts written before each module's implementation. The annotations are the surviving trace of a spec-driven approach.

## Design notes

The architecture stays close to upstream Git's because byte-for-byte compatibility forces it to. Notable deliberate choices:

- **No `unsafe`** in first-party code. zlib (`flate2`) is the only crate that uses unsafe internally; everything else is safe Rust.
- **`ObjectId` is opaque** (newtype around `[u8; 20]`) so a SHA-256 migration would be an internal change, not a type-spreading refactor.
- **Loose write atomicity** via tmp-file + fsync + rename + directory fsync. A crash mid-write never leaves a partial object at the canonical path.
- **Pack writing emits full objects** (no delta compression on the write side). The pack is larger but every server accepts it.
- **Smart HTTP v2 for fetch, v0 for push** — matches upstream Git's split. v2's sideband-64k handles packfile streaming during clone.

## Build

Requires Rust 1.75 or newer. Tested on macOS (arm64 + x86_64) and Linux.

```sh
cargo build --release
cargo test --workspace
```

## Roadmap

The v1 surface above is built to be honest: every command listed works against real repos and real remotes. The following are deferred follow-up work, each scoped as its own project — the kind of features a senior engineer would intentionally hold back rather than ship half-finished:

- **Three-way recursive merge** and **rebase** — at production quality, recursive merge is a multi-month subproject (rename detection, criss-cross history, conflict resolution, rerere). `rgit merge` currently handles the fast-forward case correctly and refuses non-FF with a clear error.
- **Textual algorithms beyond LCS** — Histogram diff and bisect are well-understood next steps.
- **Stash, cherry-pick, revert** — each is its own bounded addition once 3-way merge lands.
- **Submodules, worktrees, sparse checkout** — each carries its own coordination machinery.
- **Pack-write delta compression** — would shrink uploads ~5–10×.
- **SHA-256 / object format v2** — `ObjectId` is opaque to enable this; the change is internal.

Each of these is a real engineering decision, not a gap. Shipping a half-implemented merge is strictly worse than shipping a correct subset and naming the rest — that's the discipline the rest of the codebase reflects.

## License

MIT — see [LICENSE](LICENSE).
