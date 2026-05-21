//! rgit — a Rust reimplementation of Git.

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use rgit_core::gitignore::GitignoreSet;
use rgit_core::index::{Index, IndexEntry, Time};
use rgit_core::merge::MergeResult;
use rgit_core::object::{Blob, Commit, EntryMode, Object, ObjectId, ObjectKind, Signature};
use rgit_core::refs::HeadState;
use rgit_core::transport::{list_remote_refs, push, RefUpdate, TransportCredentials};
use rgit_core::workdir::WorkdirChange;
use rgit_core::Repository;
use std::io::Write as _;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "rgit", version, about = "A Rust reimplementation of Git.")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Initialize a new repository.
    Init {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Stage one or more files for the next commit.
    Add {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
    },
    /// Record the staged changes as a new commit.
    Commit {
        #[arg(short, long)]
        message: String,
    },
    /// Show the working-tree status.
    Status,
    /// Show commit history starting at HEAD.
    Log {
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
    },
    /// Push refs to a remote repository.
    Push {
        url: String,
        #[arg(long, default_value = "refs/heads/main")]
        ref_name: String,
    },
    /// Print object metadata or contents.
    CatFile {
        #[arg(short = 't', conflicts_with_all = ["pretty", "size"])]
        show_type: bool,
        #[arg(short = 'p', conflicts_with_all = ["show_type", "size"])]
        pretty: bool,
        #[arg(short = 's', conflicts_with_all = ["show_type", "pretty"])]
        size: bool,
        /// Object id (40-char hex or 4+-char prefix).
        id: String,
    },
    /// Compute the SHA-1 of a file as if it were a blob.
    HashObject {
        /// Also write the blob to the object database.
        #[arg(short, long)]
        write: bool,
        path: PathBuf,
    },
    /// List, create, or delete branches.
    Branch {
        /// Delete the named branch.
        #[arg(short, long)]
        delete: bool,
        /// Branch name. With no name, list all branches.
        name: Option<String>,
    },
    /// Switch HEAD to a branch or commit, updating the working tree.
    Checkout {
        /// Branch name, tag name, full id, or short-SHA prefix.
        target: String,
    },
    /// List, create, or delete tags.
    Tag {
        /// Delete the named tag.
        #[arg(short, long)]
        delete: bool,
        /// Tag name. With no name, list all tags.
        name: Option<String>,
        /// Object to tag (defaults to HEAD).
        id: Option<String>,
    },
    /// Show an object's contents with smart formatting.
    Show {
        /// Object id, ref name, or HEAD. Defaults to HEAD.
        target: Option<String>,
    },
    /// List the entries of a tree (or the tree of a commit / ref).
    LsTree {
        /// Tree id, commit id, or ref name.
        target: String,
    },
    /// Resolve a ref name or short id to a full 40-char SHA.
    RevParse { name: String },
    /// Fast-forward HEAD to `target` (a branch or commit). Errors if
    /// the merge would not be fast-forward; full three-way merge is
    /// deferred.
    Merge { target: String },
    /// Unified diff. With no args: working tree vs index. With
    /// `--cached`: index vs HEAD. With one ref: working tree vs ref.
    /// With two refs: ref-a vs ref-b.
    Diff {
        /// Compare the index against HEAD instead of the working tree
        /// against the index.
        #[arg(long)]
        cached: bool,
        refs: Vec<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init { path } => cmd_init(&path),
        Command::Add { paths } => cmd_add(paths),
        Command::Commit { message } => cmd_commit(&message),
        Command::Status => cmd_status(),
        Command::Log { limit } => cmd_log(limit),
        Command::Push { url, ref_name } => cmd_push(&url, &ref_name),
        Command::CatFile {
            show_type,
            pretty,
            size,
            id,
        } => cmd_cat_file(&id, show_type, pretty, size),
        Command::HashObject { write, path } => cmd_hash_object(&path, write),
        Command::Branch { delete, name } => cmd_branch(delete, name.as_deref()),
        Command::Checkout { target } => cmd_checkout(&target),
        Command::Tag { delete, name, id } => cmd_tag(delete, name.as_deref(), id.as_deref()),
        Command::Show { target } => cmd_show(target.as_deref()),
        Command::LsTree { target } => cmd_ls_tree(&target),
        Command::RevParse { name } => cmd_rev_parse(&name),
        Command::Merge { target } => cmd_merge(&target),
        Command::Diff { cached, refs } => cmd_diff(cached, &refs),
    }
}

fn cmd_diff(cached: bool, refs: &[String]) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let repo = Repository::open(&cwd)?;
    let output = match (cached, refs.len()) {
        (true, 0) => repo.diff_index_vs_head()?,
        (false, 0) => repo.diff_working_vs_index()?,
        (false, 2) => {
            let a = resolve_to_tree(&repo, &refs[0])?;
            let b = resolve_to_tree(&repo, &refs[1])?;
            repo.diff_trees(&a, &b)?
        }
        (true, _) => return Err(anyhow!("--cached does not take ref arguments")),
        (false, 1) => {
            // Working tree vs ref: realize the ref's tree paths and
            // compare against working tree. Implemented as: diff
            // HEAD-tree vs ref-tree if HEAD differs from ref. Simpler
            // alternative for v1 — defer; emit a hint.
            return Err(anyhow!(
                "single-ref diff (working tree vs ref) is not implemented; \
                 use `rgit diff <ref-a> <ref-b>` for two-tree comparison \
                 or `rgit diff` / `rgit diff --cached` for working-tree diffs"
            ));
        }
        _ => return Err(anyhow!("too many ref arguments")),
    };
    std::io::stdout().write_all(output.as_bytes())?;
    Ok(())
}

fn resolve_to_tree(repo: &Repository, name: &str) -> Result<ObjectId> {
    let id = resolve_ref_or_id(repo, name)?;
    let obj = repo.read_object(&id)?;
    match obj {
        Object::Commit(c) => Ok(c.tree),
        Object::Tree(_) => Ok(id),
        _ => Err(anyhow!("{name} is not a commit or tree")),
    }
}

fn cmd_merge(target: &str) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let repo = Repository::open(&cwd)?;
    let target_id = resolve_ref_or_id(&repo, target)?;
    match repo.merge_fast_forward(&target_id)? {
        MergeResult::UpToDate => println!("Already up to date."),
        MergeResult::FastForwarded { from, to } => println!(
            "Fast-forwarded from {} to {}",
            &from.to_hex()[..7],
            &to.to_hex()[..7],
        ),
    }
    Ok(())
}

/// Resolve a ref name, short id, or full SHA to an `ObjectId`.
/// Tries (in order): exact ref, refs/heads/<name>, refs/tags/<name>,
/// short-SHA via `Repository::resolve_id`.
fn resolve_ref_or_id(repo: &Repository, name: &str) -> Result<ObjectId> {
    if name == "HEAD" || name.starts_with("refs/") {
        return repo
            .read_ref(name)
            .with_context(|| format!("resolving {name}"));
    }
    for prefix in ["refs/heads/", "refs/tags/", "refs/remotes/"] {
        let candidate = format!("{prefix}{name}");
        if let Ok(id) = repo.read_ref(&candidate) {
            return Ok(id);
        }
    }
    repo.resolve_id(name)
        .with_context(|| format!("not a ref or id: {name}"))
}

fn cmd_init(path: &Path) -> Result<()> {
    let repo = Repository::init(path, false)?;
    println!(
        "Initialized empty rgit repository in {}",
        repo.git_dir().display()
    );
    Ok(())
}

fn cmd_add(paths: Vec<PathBuf>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let repo = Repository::open(&cwd)?;
    let work_dir = repo
        .work_dir()
        .ok_or_else(|| anyhow!("not in a work tree"))?
        .to_path_buf();
    let ignores = GitignoreSet::load(&work_dir)?;
    let mut index = repo.read_index()?;
    for path in paths {
        add_path(&repo, &work_dir, &cwd, &path, &ignores, &mut index)?;
    }
    repo.write_index(&index)?;
    Ok(())
}

fn add_path(
    repo: &Repository,
    work_dir: &Path,
    cwd: &Path,
    path: &Path,
    ignores: &GitignoreSet,
    index: &mut Index,
) -> Result<()> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let abs = abs
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", abs.display()))?;
    let work_dir_canon = work_dir
        .canonicalize()
        .unwrap_or_else(|_| work_dir.to_path_buf());
    let rel = abs
        .strip_prefix(&work_dir_canon)
        .map_err(|_| anyhow!("path outside work tree: {}", path.display()))?;
    let rel_str = rel.to_string_lossy();
    let rel_bytes_for_ignore = rel_str.as_bytes();

    let meta = std::fs::symlink_metadata(&abs)?;
    if meta.is_dir() {
        // Honor gitignore for directory recursion: skip ignored dirs.
        if !rel_bytes_for_ignore.is_empty() && ignores.is_ignored(rel_bytes_for_ignore, true) {
            return Ok(());
        }
        for entry in std::fs::read_dir(&abs)? {
            let entry = entry?;
            let name = entry.file_name();
            if name == ".git" {
                continue;
            }
            add_path(repo, work_dir, cwd, &entry.path(), ignores, index)?;
        }
        return Ok(());
    }
    // File / symlink — skip if ignored.
    if !rel_bytes_for_ignore.is_empty() && ignores.is_ignored(rel_bytes_for_ignore, false) {
        return Ok(());
    }

    let rel_bytes = rel.to_string_lossy().into_owned().into_bytes();
    let (data, stat_mode) = if meta.is_symlink() {
        let target = std::fs::read_link(&abs)?;
        let bytes = target.to_string_lossy().into_owned().into_bytes();
        (bytes, 0o120000u32)
    } else {
        let bytes = std::fs::read(&abs)?;
        let executable = meta.mode() & 0o111 != 0;
        let stat_mode = if executable { 0o100755 } else { 0o100644 };
        (bytes, stat_mode)
    };
    let size = data.len() as u32;
    let blob = Object::Blob(Blob::new(data));
    let id = repo.write_object(&blob)?;
    index.upsert(IndexEntry {
        ctime: Time {
            secs: meta.ctime() as u32,
            nanos: meta.ctime_nsec() as u32,
        },
        mtime: Time {
            secs: meta.mtime() as u32,
            nanos: meta.mtime_nsec() as u32,
        },
        dev: 0,
        ino: 0,
        mode: stat_mode,
        uid: 0,
        gid: 0,
        size,
        id,
        assume_valid: false,
        stage: 0,
        path: rel_bytes,
    });
    Ok(())
}

fn cmd_commit(message: &str) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let repo = Repository::open(&cwd)?;
    let index = repo.read_index()?;
    if index.entries().is_empty() {
        return Err(anyhow!("nothing to commit (empty index)"));
    }
    let tree_id = repo.build_tree_from_index(&index)?;

    let parents = match repo.read_ref("HEAD") {
        Ok(id) => vec![id],
        Err(_) => vec![],
    };

    let name = std::env::var("GIT_AUTHOR_NAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "unknown".to_string());
    let email = std::env::var("GIT_AUTHOR_EMAIL").unwrap_or_else(|_| format!("{name}@localhost"));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let raw_sig = format!("{name} <{email}> {now} +0000");
    let sig = Signature {
        raw: raw_sig.into_bytes(),
        name: Some(name.clone().into_bytes()),
        email: Some(email.into_bytes()),
        timestamp: Some(now),
        timezone: Some(b"+0000".to_vec()),
    };

    let mut msg = message.to_string();
    if !msg.ends_with('\n') {
        msg.push('\n');
    }
    let commit = Commit {
        tree: tree_id,
        parents,
        author: sig.clone(),
        committer: sig,
        extra_headers: vec![],
        message: msg.into_bytes(),
    };
    let commit_id = repo.write_object(&Object::Commit(commit))?;

    match repo.read_head()? {
        HeadState::Symbolic(target) => {
            repo.write_ref(&target, &commit_id)?;
            println!(
                "[{} {}] {}",
                target.strip_prefix("refs/heads/").unwrap_or(&target),
                &commit_id.to_hex()[..7],
                message
            );
        }
        HeadState::Detached(_) => {
            repo.set_head_detached(&commit_id)?;
            println!("[detached HEAD {}] {}", &commit_id.to_hex()[..7], message);
        }
    }
    Ok(())
}

fn cmd_status() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let repo = Repository::open(&cwd)?;
    let work_dir = repo
        .work_dir()
        .ok_or_else(|| anyhow!("not in a work tree"))?
        .to_path_buf();
    let ignores = GitignoreSet::load(&work_dir)?;
    let status = repo.status()?;

    let mut filtered: Vec<&WorkdirChange> = Vec::with_capacity(status.changes.len());
    for change in &status.changes {
        if let WorkdirChange::Untracked(path) = change {
            if ignores.is_ignored(path, false) {
                continue;
            }
        }
        filtered.push(change);
    }

    if filtered.is_empty() {
        println!("nothing to commit, working tree clean");
        return Ok(());
    }
    for change in filtered {
        match change {
            WorkdirChange::Staged(p) => println!("staged:    {}", String::from_utf8_lossy(p)),
            WorkdirChange::Modified(p) => println!("modified:  {}", String::from_utf8_lossy(p)),
            WorkdirChange::Deleted(p) => println!("deleted:   {}", String::from_utf8_lossy(p)),
            WorkdirChange::Untracked(p) => println!("untracked: {}", String::from_utf8_lossy(p)),
        }
    }
    Ok(())
}

fn cmd_log(limit: usize) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let repo = Repository::open(&cwd)?;
    let mut current = Some(repo.read_ref("HEAD")?);
    let mut count = 0;
    while let Some(id) = current {
        if count >= limit {
            break;
        }
        let obj = repo.read_object(&id)?;
        let Object::Commit(commit) = obj else { break };
        println!("commit {}", id.to_hex());
        println!("Author: {}", String::from_utf8_lossy(&commit.author.raw));
        println!();
        for line in String::from_utf8_lossy(&commit.message).lines() {
            println!("    {line}");
        }
        println!();
        current = commit.parents.first().copied();
        count += 1;
    }
    Ok(())
}

fn cmd_push(url: &str, ref_name: &str) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let repo = Repository::open(&cwd)?;
    let new_id = repo.read_ref(ref_name)?;

    let is_ssh = url.starts_with("git@") || url.starts_with("ssh://");

    // For HTTPS, build credentials and pre-query the remote state for
    // old_id. For SSH, both are handled inside `transport::push`:
    // ssh-agent supplies auth, and the SSH ref advertisement (read on
    // the same connection) supplies old_id.
    let (creds, old_id) = if is_ssh {
        (None, ObjectId::ZERO)
    } else {
        let username = std::env::var("RGIT_USERNAME")
            .or_else(|_| std::env::var("GITHUB_USERNAME"))
            .map_err(|_| anyhow!("set RGIT_USERNAME or GITHUB_USERNAME"))?;
        let token = std::env::var("RGIT_TOKEN")
            .or_else(|_| std::env::var("GITHUB_TOKEN"))
            .map_err(|_| anyhow!("set RGIT_TOKEN or GITHUB_TOKEN"))?;
        let creds = TransportCredentials { username, token };
        let remote = list_remote_refs(url, Some(&creds))?;
        let old_id = remote
            .iter()
            .find(|r| r.name == ref_name)
            .map(|r| r.id)
            .unwrap_or(ObjectId::ZERO);
        if old_id == new_id {
            println!("Everything up to date");
            return Ok(());
        }
        (Some(creds), old_id)
    };

    let updates = vec![RefUpdate {
        old_id,
        new_id,
        ref_name: ref_name.to_string(),
    }];
    let result = push(&repo, url, creds.as_ref(), &updates)?;
    if !result.unpack_ok {
        return Err(anyhow!("server failed to unpack pack"));
    }
    for (name, r) in &result.per_ref {
        match r {
            Ok(()) => println!("ok      {name}"),
            Err(reason) => println!("failed  {name}: {reason}"),
        }
    }
    Ok(())
}

fn cmd_cat_file(id_str: &str, show_type: bool, pretty: bool, size: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let repo = Repository::open(&cwd)?;
    let id = repo.resolve_id(id_str)?;
    let (kind, payload) = repo.read_object_raw(&id)?;
    if show_type {
        println!("{}", kind.as_str());
    } else if size {
        println!("{}", payload.len());
    } else if pretty {
        match kind {
            ObjectKind::Blob | ObjectKind::Commit | ObjectKind::Tag => {
                std::io::stdout().write_all(&payload)?;
            }
            ObjectKind::Tree => {
                let obj = repo.read_object(&id)?;
                if let Object::Tree(tree) = obj {
                    for entry in &tree.entries {
                        let mode_str = std::str::from_utf8(entry.mode.as_octal()).unwrap_or("?");
                        let kind_str = match entry.mode {
                            EntryMode::Tree => "tree",
                            EntryMode::Gitlink => "commit",
                            _ => "blob",
                        };
                        println!(
                            "{mode_str:0>6} {kind_str} {}\t{}",
                            entry.id.to_hex(),
                            String::from_utf8_lossy(&entry.name)
                        );
                    }
                }
            }
        }
    } else {
        return Err(anyhow!("must specify one of -t, -p, -s"));
    }
    Ok(())
}

fn cmd_hash_object(path: &Path, write: bool) -> Result<()> {
    let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let id = if write {
        let cwd = std::env::current_dir()?;
        let repo = Repository::open(&cwd)?;
        repo.write_object(&Object::Blob(Blob::new(data)))?
    } else {
        ObjectId::compute(ObjectKind::Blob, &data)
    };
    println!("{}", id.to_hex());
    Ok(())
}

fn cmd_branch(delete: bool, name: Option<&str>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let repo = Repository::open(&cwd)?;

    match (delete, name) {
        (true, Some(name)) => {
            let full = format!("refs/heads/{name}");
            repo.delete_ref(&full)?;
            println!("Deleted branch {name}");
        }
        (true, None) => return Err(anyhow!("--delete requires a branch name")),
        (false, Some(name)) => {
            let head_id = repo.read_ref("HEAD")?;
            let full = format!("refs/heads/{name}");
            repo.write_ref(&full, &head_id)?;
        }
        (false, None) => {
            let head_branch = match repo.read_head()? {
                HeadState::Symbolic(t) => Some(t),
                HeadState::Detached(_) => None,
            };
            let refs = repo.list_refs("refs/heads/")?;
            for (name, _) in refs {
                let short = name.strip_prefix("refs/heads/").unwrap_or(&name);
                let marker = if head_branch.as_deref() == Some(&name) {
                    '*'
                } else {
                    ' '
                };
                println!("{marker} {short}");
            }
        }
    }
    Ok(())
}

fn cmd_checkout(target: &str) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let repo = Repository::open(&cwd)?;

    // Resolve target to a commit id.
    let commit_id = resolve_ref_or_id(&repo, target)?;

    // Resolve commit's tree.
    let obj = repo.read_object(&commit_id)?;
    let tree_id = match obj {
        Object::Commit(c) => c.tree,
        Object::Tree(_) => commit_id, // explicit tree id
        _ => return Err(anyhow!("not a commit or tree: {target}")),
    };

    // Capture the set of currently-tracked paths so we can remove any
    // that the new tree doesn't have.
    let old_paths: std::collections::HashSet<Vec<u8>> = repo
        .read_index()?
        .entries()
        .iter()
        .filter(|e| e.stage == 0)
        .map(|e| e.path.clone())
        .collect();

    let index = repo.checkout(&tree_id)?;

    let new_paths: std::collections::HashSet<Vec<u8>> = index
        .entries()
        .iter()
        .filter(|e| e.stage == 0)
        .map(|e| e.path.clone())
        .collect();
    let work_dir = repo
        .work_dir()
        .ok_or_else(|| anyhow!("no work tree"))?
        .to_path_buf();
    for stale in old_paths.difference(&new_paths) {
        let path_str = std::str::from_utf8(stale).unwrap_or("");
        let abs = work_dir.join(path_str);
        let _ = std::fs::remove_file(&abs);
    }

    repo.write_index(&index)?;

    // Update HEAD. If `target` resolves to a branch, set HEAD symbolic;
    // otherwise detach at the commit.
    let branch_ref = if target.starts_with("refs/") {
        Some(target.to_string())
    } else {
        let candidate = format!("refs/heads/{target}");
        if repo.read_ref(&candidate).is_ok() {
            Some(candidate)
        } else {
            None
        }
    };
    match branch_ref {
        Some(branch) => {
            repo.set_head_symbolic(&branch)?;
            let short = branch.strip_prefix("refs/heads/").unwrap_or(&branch);
            println!("Switched to branch '{short}'");
        }
        None => {
            repo.set_head_detached(&commit_id)?;
            println!("HEAD is now at {} (detached)", &commit_id.to_hex()[..7]);
        }
    }
    Ok(())
}

fn cmd_tag(delete: bool, name: Option<&str>, target: Option<&str>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let repo = Repository::open(&cwd)?;

    match (delete, name) {
        (true, Some(name)) => {
            let full = format!("refs/tags/{name}");
            repo.delete_ref(&full)?;
            println!("Deleted tag {name}");
        }
        (true, None) => return Err(anyhow!("--delete requires a tag name")),
        (false, Some(name)) => {
            let id = match target {
                Some(t) => resolve_ref_or_id(&repo, t)?,
                None => repo.read_ref("HEAD")?,
            };
            let full = format!("refs/tags/{name}");
            repo.write_ref(&full, &id)?;
        }
        (false, None) => {
            let refs = repo.list_refs("refs/tags/")?;
            for (name, _) in refs {
                let short = name.strip_prefix("refs/tags/").unwrap_or(&name);
                println!("{short}");
            }
        }
    }
    Ok(())
}

fn cmd_show(target: Option<&str>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let repo = Repository::open(&cwd)?;
    let target = target.unwrap_or("HEAD");
    let id = resolve_ref_or_id(&repo, target)?;
    let obj = repo.read_object(&id)?;

    match obj {
        Object::Commit(c) => {
            println!("commit {}", id.to_hex());
            println!("Author: {}", String::from_utf8_lossy(&c.author.raw));
            println!("Committer: {}", String::from_utf8_lossy(&c.committer.raw));
            println!();
            for line in String::from_utf8_lossy(&c.message).lines() {
                println!("    {line}");
            }
        }
        Object::Tree(t) => {
            print_tree(&t);
        }
        Object::Blob(b) => {
            std::io::stdout().write_all(&b.data)?;
        }
        Object::Tag(t) => {
            println!("tag {}", id.to_hex());
            println!("Tagger: {}", String::from_utf8_lossy(&t.tagger.raw));
            println!("Target: {} ({})", t.object.to_hex(), t.object_kind.as_str());
            println!();
            std::io::stdout().write_all(&t.message)?;
        }
    }
    Ok(())
}

fn cmd_ls_tree(target: &str) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let repo = Repository::open(&cwd)?;
    let id = resolve_ref_or_id(&repo, target)?;
    let obj = repo.read_object(&id)?;
    let tree = match obj {
        Object::Tree(t) => t,
        Object::Commit(c) => {
            let tree_obj = repo.read_object(&c.tree)?;
            match tree_obj {
                Object::Tree(t) => t,
                _ => return Err(anyhow!("commit's tree id does not resolve to a tree")),
            }
        }
        _ => return Err(anyhow!("not a tree or commit: {target}")),
    };
    print_tree(&tree);
    Ok(())
}

fn print_tree(tree: &rgit_core::object::Tree) {
    for entry in &tree.entries {
        let mode_str = std::str::from_utf8(entry.mode.as_octal()).unwrap_or("?");
        let kind_str = match entry.mode {
            EntryMode::Tree => "tree",
            EntryMode::Gitlink => "commit",
            _ => "blob",
        };
        println!(
            "{mode_str:0>6} {kind_str} {}\t{}",
            entry.id.to_hex(),
            String::from_utf8_lossy(&entry.name)
        );
    }
}

fn cmd_rev_parse(name: &str) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let repo = Repository::open(&cwd)?;
    let id = resolve_ref_or_id(&repo, name)?;
    println!("{}", id.to_hex());
    Ok(())
}
