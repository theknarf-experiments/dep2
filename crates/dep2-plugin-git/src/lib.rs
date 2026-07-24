//! Git history streaming plugin, backed by gitoxide (`gix`).
//!
//! Emits the repository's commit history as relations, so Datalog programs can
//! join VERSION HISTORY with whatever else the engine ingests (ASTs, imports,
//! call graphs) — churn hotspots, co-change coupling, ownership, staleness.
//!
//!   commit(id, author, email, time, message)   one row per commit; `time` is
//!                                              unix seconds, `message` the
//!                                              subject line
//!   commit_parent(id, parent)                  the commit DAG
//!   commit_file(id, file, change)              files touched per commit
//!                                              (change: A/M/D/R), from a tree
//!                                              diff against the FIRST parent
//!
//! Paths are repo-relative with `/` separators — the same convention as the
//! fs/treesitter plugins, so `commit_file.file` joins their relations directly
//! when both sources share a root.
//!
//! Config keys:
//!   - `root`        (required) the repository (or any dir inside it).
//!   - `max_commits` (optional) history depth cap, newest first (default 20000).
//!
//! Work units are COMMIT IDS: the engine shards them across workers, and
//! `set_wanted` skips the (expensive) tree diffs entirely when no rule reads
//! `commit_file`. Polling follows HEAD: new commits stream in append-only;
//! rewritten history adds the new ids (rows for orphaned commits remain — they
//! are still reachable objects and content-addressed facts stay true).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use dep2_plugin::{
    ColumnDef, DataSchema, DataType, DataValue, Plugin, PluginContext, Source, StreamOutput,
    StreamingDataProvider, StreamingDataSource, ValueSink,
};

pub struct GitPlugin;

impl Plugin for GitPlugin {
    fn name(&self) -> &str {
        "git"
    }

    fn setup(&self, ctx: &mut PluginContext) {
        ctx.register(self.name());
        ctx.register_streaming_data_provider(Box::new(GitProvider));
    }
}

const KNOWN_KEYS: &[&str] = &["root", "max_commits"];
const COMMIT_RELATION: &str = "commit";
const PARENT_RELATION: &str = "commit_parent";
const FILE_RELATION: &str = "commit_file";
const DEFAULT_MAX_COMMITS: usize = 20_000;

fn col(name: &str, data_type: DataType) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        data_type,
    }
}

struct GitProvider;

impl StreamingDataProvider for GitProvider {
    fn name(&self) -> &str {
        "git"
    }

    fn open_stream(
        &self,
        config: &HashMap<String, String>,
    ) -> Result<Box<dyn StreamingDataSource>, String> {
        for key in config.keys() {
            if !KNOWN_KEYS.contains(&key.as_str()) {
                return Err(format!(
                    "git: unknown config attribute '{}' (known: {})",
                    key,
                    KNOWN_KEYS.join(", ")
                ));
            }
        }
        let root = PathBuf::from(
            config
                .get("root")
                .ok_or("git provider requires 'root' config attribute")?,
        );
        // Fail fast on an unopenable repo (a bad root otherwise surfaces as a
        // silently empty history).
        gix::discover(&root).map_err(|e| format!("git: can't open '{}': {}", root.display(), e))?;
        let max_commits = match config.get("max_commits") {
            Some(s) => s
                .parse::<usize>()
                .map_err(|_| format!("git: bad max_commits '{}'", s))?,
            None => DEFAULT_MAX_COMMITS,
        };
        Ok(Box::new(GitStreamingSource {
            root,
            max_commits,
            want_files: true,
        }))
    }
}

struct GitStreamingSource {
    root: PathBuf,
    max_commits: usize,
    want_files: bool,
}

impl StreamingDataSource for GitStreamingSource {
    fn outputs(&self) -> Vec<StreamOutput> {
        vec![
            StreamOutput {
                relation: COMMIT_RELATION.to_string(),
                schema: DataSchema {
                    columns: vec![
                        col("id", DataType::String),
                        col("author", DataType::String),
                        col("email", DataType::String),
                        col("time", DataType::Integer),
                        col("message", DataType::String),
                    ],
                },
            },
            StreamOutput {
                relation: PARENT_RELATION.to_string(),
                schema: DataSchema {
                    columns: vec![col("id", DataType::String), col("parent", DataType::String)],
                },
            },
            StreamOutput {
                relation: FILE_RELATION.to_string(),
                schema: DataSchema {
                    columns: vec![
                        col("id", DataType::String),
                        col("file", DataType::String),
                        col("change", DataType::String),
                    ],
                },
            },
        ]
    }

    fn set_wanted(&mut self, wanted: &HashSet<String>) {
        // Tree diffs dominate ingestion cost; skip them when nothing reads
        // commit_file.
        self.want_files = wanted.contains(FILE_RELATION);
    }

    fn seed_units(&self) -> Vec<String> {
        walk_ids(&self.root, self.max_commits, &HashSet::new()).unwrap_or_default()
    }

    fn open(&self) -> Box<dyn Source> {
        Box::new(GitWorker {
            root: self.root.clone(),
            max_commits: self.max_commits,
            want_files: self.want_files,
            repo: gix::discover(&self.root).ok(),
            seen: HashSet::new(),
            last_head: None,
        })
    }
}

/// Newest-first commit ids reachable from HEAD, skipping `known`, capped.
fn walk_ids(
    root: &std::path::Path,
    max: usize,
    known: &HashSet<String>,
) -> Result<Vec<String>, String> {
    let repo = gix::discover(root).map_err(|e| e.to_string())?;
    let head = repo.head_id().map_err(|e| e.to_string())?;
    let walk = repo
        .rev_walk([head.detach()])
        .all()
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for info in walk {
        let Ok(info) = info else { break };
        let id = info.id.to_string();
        if known.contains(&id) {
            // Newest-first walk: once we hit known history, the rest is known.
            break;
        }
        out.push(id);
        if out.len() >= max {
            break;
        }
    }
    Ok(out)
}

struct GitWorker {
    root: PathBuf,
    max_commits: usize,
    want_files: bool,
    repo: Option<gix::Repository>,
    /// Commit ids this worker has been asked about (drives poll dedup).
    seen: HashSet<String>,
    last_head: Option<String>,
}

impl Source for GitWorker {
    fn ingest(&mut self, unit: &str, sink: &mut dyn ValueSink) {
        self.seen.insert(unit.to_string());
        let Some(repo) = &self.repo else { return };
        let Ok(oid) = gix::ObjectId::from_hex(unit.as_bytes()) else {
            return;
        };
        let Ok(commit) = repo.find_commit(oid) else {
            return;
        };

        // commit(id, author, email, time, message)
        let (author, email) = match commit.author() {
            Ok(sig) => (sig.name.to_string(), sig.email.to_string()),
            Err(_) => (String::new(), String::new()),
        };
        let time = commit.time().map(|t| t.seconds).unwrap_or(0);
        let message = commit
            .message()
            .map(|m| m.summary().to_string())
            .unwrap_or_default();
        sink.push(
            COMMIT_RELATION,
            &[
                DataValue::String(unit.to_string()),
                DataValue::String(author),
                DataValue::String(email),
                DataValue::Integer(time),
                DataValue::String(message),
            ],
            1,
        );

        // commit_parent(id, parent)
        for parent in commit.parent_ids() {
            sink.push(
                PARENT_RELATION,
                &[
                    DataValue::String(unit.to_string()),
                    DataValue::String(parent.to_string()),
                ],
                1,
            );
        }

        // commit_file(id, file, change): tree diff vs the FIRST parent (or the
        // empty tree for a root commit).
        if !self.want_files {
            return;
        }
        let Ok(tree) = commit.tree() else { return };
        let parent_tree = commit
            .parent_ids()
            .next()
            .and_then(|p| repo.find_commit(p.detach()).ok())
            .and_then(|p| p.tree().ok())
            .unwrap_or_else(|| repo.empty_tree());
        let Ok(mut platform) = parent_tree.changes() else {
            return;
        };
        let _ = platform.for_each_to_obtain_tree(&tree, |change| {
            use gix::object::tree::diff::Change;
            // The walk visits directory (tree) entries too; only blobs are
            // files. Rewrites track the destination entry's mode.
            let (kind, location, mode) = match &change {
                Change::Addition {
                    location,
                    entry_mode,
                    ..
                } => ("A", location, entry_mode),
                Change::Deletion {
                    location,
                    entry_mode,
                    ..
                } => ("D", location, entry_mode),
                Change::Modification {
                    location,
                    entry_mode,
                    ..
                } => ("M", location, entry_mode),
                Change::Rewrite {
                    location,
                    entry_mode,
                    ..
                } => ("R", location, entry_mode),
            };
            if !mode.is_blob() {
                return Ok::<_, std::convert::Infallible>(std::ops::ControlFlow::Continue(()));
            }
            sink.push(
                FILE_RELATION,
                &[
                    DataValue::String(unit.to_string()),
                    DataValue::String(location.to_string()),
                    DataValue::String(kind.to_string()),
                ],
                1,
            );
            Ok::<_, std::convert::Infallible>(std::ops::ControlFlow::Continue(()))
        });
    }

    fn poll_changes(&mut self) -> Vec<String> {
        let Some(repo) = &self.repo else {
            return Vec::new();
        };
        let head = match repo.head_id() {
            Ok(id) => id.to_string(),
            Err(_) => return Vec::new(),
        };
        if self.last_head.as_deref() == Some(head.as_str()) {
            return Vec::new();
        }
        self.last_head = Some(head);
        walk_ids(&self.root, self.max_commits, &self.seen).unwrap_or_default()
    }
}
