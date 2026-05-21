# rgit

A Rust reimplementation of Git from scratch.

Implements Git's object model, pack format, ref system, index file, and the Smart HTTP push protocol — enough to initialize a repository, stage changes, commit them, and push to a real GitHub remote.

**This repository was pushed to GitHub by `rgit` itself.** The commit history on `main` was authored by the binary built from this source tree. The dogfood test was the design goal.

## What works

```
rgit init <path>              create a new repository
rgit add <paths…>             stage files; recurses into directories
rgit commit -m <msg>          record a commit; advances HEAD's target ref
rgit status                   working-tree vs index vs HEAD
rgit log [-n N]               walk commit history from HEAD
rgit branch [-d] [<name>]     list / create / delete branches
rgit checkout <ref>           switch HEAD to a branch or commit
rgit tag [-d] [<name> [<id>]] list / create / delete tags
rgit show [<id>]              inspect an object with kind-aware formatting
rgit ls-tree <ref>            list entries of a tree (or a commit's tree)
rgit rev-parse <ref>          resolve a ref name or short id to a full SHA
rgit cat-file [-t|-p|-s] <id> low-level object inspection
rgit hash-object [-w] <file>  compute (and optionally store) a blob's id
rgit push <url>               upload refs + objects (Smart HTTP or SSH)
```

rgit's on-disk format is byte-for-byte compatible with upstream Git. `git log`, `git ls-tree`, `git cat-file`, and `git fsck` all read rgit-produced repositories without complaint. You can hand a repo back and forth between the two.

## What's intentionally out of scope (v1)

These are deferred work, not gaps in understanding. Each is a sub-project of its own; shipping half-finished versions would be worse than shipping none:

- **`clone` / `fetch`** — the v2 fetch protocol is its own implementation effort. Push uses the older v0 receive-pack, which is smaller.
- **Merge, rebase, cherry-pick, revert, stash** — three-way merge at production quality is a multi-month subproject. A half-implemented merge is the worst possible artifact for the portfolio frame.
- **`.gitignore`** — straightforward but bounded; `rgit status` currently surfaces every untracked file including build outputs.
- **Delta-encoded pack writes** — rgit emits full-object packs. The uploads are 5–10× larger than upstream's, but every server accepts them.
- **Textual diff (`rgit diff`)** — Myers/Histogram diff is its own project.
- **Submodules, worktrees, sparse checkout, SSH transport, Windows support** — each is a deliberate scope decision, not a missed feature.

## Quick start

```sh
cargo build --release

mkdir demo && cd demo
../target/release/rgit init .
echo "hello rgit" > README.md
../target/release/rgit add README.md
../target/release/rgit commit -m "initial commit"

# Upstream git can read what rgit just wrote.
git log --oneline
git ls-tree HEAD

# Push via HTTPS (uses a personal access token):
export GITHUB_USERNAME=your-username
export GITHUB_TOKEN=ghp_xxxxxxxxxxxx
../target/release/rgit push https://github.com/your/repo.git

# Or push via SSH (uses your existing ssh-agent / ~/.ssh config):
../target/release/rgit push git@github.com:your/repo.git
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
└── transport/   push via HTTPS (pkt-line + HTTP Basic auth) and SSH
                  (subprocess to system `ssh`, raw receive-pack over the pipe)
rgit-cli/src/
└── main.rs      CLI dispatch (clap-derive)
```

Each module owns its data, its errors, and its tests. No `unsafe` anywhere in first-party code (`#![forbid(unsafe_code)]` at the workspace level). `cargo clippy --workspace --all-targets -- -D warnings` is clean. `cargo fmt --check` is clean.

## Quality

- **221 unit tests** covering format round-trips, error paths, atomic filesystem semantics, and reachability walks.
- **Cross-implementation verification**: every commit and tree rgit writes is read correctly by upstream Git.
- **`@spec` annotations** in code cite which behavioral spec each function implements. The specs themselves — short normative one-liners with stable IDs (`OBJ-FRAME-006`, `ODB-WRITE-005`, etc.) — are internal design artifacts that I wrote before each module's implementation. The annotations are the surviving trace of a spec-driven approach: every meaningful behavior had a one-line written contract before any code ran, and a test before the implementation.

## Design notes

The architecture stays close to upstream Git's because byte-for-byte compatibility forced it to. Where I deviated:

- **No `unsafe`** in first-party code. zlib (via `flate2`) is the only crate that uses unsafe internally; everything else is safe Rust.
- **`ObjectId` is opaque** (newtype around `[u8; 20]`) so a SHA-256 migration would be an internal change rather than a type-spreading refactor.
- **Loose write atomicity** uses tmp-file + fsync + rename + directory fsync. A crash mid-write never leaves a partial object at the canonical path.
- **Pack-side reads memoize less than upstream**: the trade-off is a few extra syscalls for a smaller, clearer hot path. Switching to `mmap` is a one-module change behind the existing API.
- **Push sends full-object packs.** Delta compression on write is deferred; correctness over upload size for v1.

## Build

Requires Rust 1.75 or newer. Tested on macOS (arm64 + x86_64) and Linux.

```sh
cargo build --release
cargo test --workspace
```

## License

MIT — see [LICENSE](LICENSE).
