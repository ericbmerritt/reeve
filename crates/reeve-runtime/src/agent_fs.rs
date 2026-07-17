//! Filesystem layout types for the Reeve data directory.
//!
//! Two layout structs mirror two levels of the directory tree:
//!
//! - [`RuntimeLayout`] — the root data directory. Vends typed paths for
//!   cross-cutting files: `personas/`, `teams/`, `blacklist.toml`, etc.
//! - [`AgentDirs`] — the per-agent subtree under `agents/<name>/`. Vends
//!   paths for the agent's inbox, journal, status, cost, keypair, and
//!   profile snapshot.
//!
//! Neither struct contains any mutable state; all accessors return freshly
//! computed `PathBuf`s. Callers are responsible for I/O.
//!
//! Filesystem safety follows `specs/reeve-transport-security.md` §
//! Filesystem Safety: no symlink following (`O_NOFOLLOW`), mode `0o700` on
//! all directories, mode `0o600` on all files, atomic writes via temp-file →
//! fsync → rename.

use std::fmt;
use std::fs::File;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::fs_util::{ensure_directory, open_jsonl_file, FsCheckError};

// ── Mode constants ────────────────────────────────────────────────────────────

const AGENT_DIR_MODE: u32 = 0o700;

// ── RuntimeLayout ─────────────────────────────────────────────────────────────

/// Root-level filesystem layout for the Reeve data directory.
///
/// Holds the path once and vends typed accessors so callers never
/// reconstruct `data_dir.join("personas").join(name).join("profile.toml")`
/// by hand. If the on-disk layout changes, only the methods here need
/// updating.
///
/// ```text
/// <root>/
///   identities/<uuid>.toml         ← IdentityRegistry
///   personas/<name>/config.toml
///   personas/<name>/profile.toml
///   agents/<name>/…                ← see AgentDirs
///   teams/<name>.toml
///   engagements/<name>/record.toml
///   blacklist.toml
/// ```
#[derive(Debug, Clone)]
pub struct RuntimeLayout {
    root: PathBuf,
}

impl RuntimeLayout {
    /// Construct a layout rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The data root itself.
    pub fn root(&self) -> &Path {
        &self.root
    }

    // ── Personas ──────────────────────────────────────────────────────────────

    /// Directory for a named persona: `<root>/personas/<name>/`.
    pub fn persona_dir(&self, name: &str) -> PathBuf {
        self.root.join("personas").join(name)
    }

    /// Persona configuration: `<root>/personas/<name>/config.toml`.
    pub fn persona_config_path(&self, name: &str) -> PathBuf {
        self.persona_dir(name).join("config.toml")
    }

    /// Persona capability profile: `<root>/personas/<name>/profile.toml`.
    pub fn persona_profile_path(&self, name: &str) -> PathBuf {
        self.persona_dir(name).join("profile.toml")
    }

    // ── Agents ────────────────────────────────────────────────────────────────

    /// Root of the agents subtree: `<root>/agents/`.
    ///
    /// Root of the personas directory: `<root>/personas/`.
    ///
    /// Each subdirectory here is a named persona with its own `config.toml`.
    pub fn personas_root(&self) -> PathBuf {
        self.root.join("personas")
    }

    /// The TUI watcher watches this directory recursively; the agent registry
    /// TOML lives directly here (`registry.toml`).
    pub fn agents_root(&self) -> PathBuf {
        self.root.join("agents")
    }

    /// Agent registry file: `<root>/agents/registry.toml`.
    pub fn agent_registry_path(&self) -> PathBuf {
        self.agents_root().join("registry.toml")
    }

    /// Open an [`AgentDirs`] handle for the named agent.
    ///
    /// This is a path-construction call only — no I/O. Returns
    /// `Err(AgentFsError)` when `name` fails the agent-name validation rules.
    pub fn agent_dirs(&self, name: &str) -> Result<AgentDirs, AgentFsError> {
        AgentDirs::open(&self.root, name)
    }

    // ── Teams ─────────────────────────────────────────────────────────────────

    /// Team configuration file: `<root>/teams/<name>.toml`.
    pub fn team_config_path(&self, name: &str) -> PathBuf {
        self.root.join("teams").join(name).with_extension("toml")
    }

    // ── Engagements ───────────────────────────────────────────────────────────

    /// Root of the engagements subtree: `<root>/engagements/`.
    ///
    /// Each subdirectory is a named engagement; directories persist after
    /// close (names are never reused), so directory existence doubles as
    /// the name-permanence check.
    pub fn engagements_root(&self) -> PathBuf {
        self.root.join("engagements")
    }

    /// Directory for a named engagement: `<root>/engagements/<name>/`.
    pub fn engagement_dir(&self, name: &str) -> PathBuf {
        self.engagements_root().join(name)
    }

    /// Engagement record file: `<root>/engagements/<name>/record.toml`.
    pub fn engagement_record_path(&self, name: &str) -> PathBuf {
        self.engagement_dir(name).join("record.toml")
    }

    // ── Team rosters ──────────────────────────────────────────────────────────

    /// Root of the standing-team roster store: `<root>/rosters/`.
    ///
    /// Distinct from `teams/`, which holds operator-authored team
    /// *templates*; rosters are machine-managed durable state written when
    /// a template is formed into a standing team.
    pub fn rosters_root(&self) -> PathBuf {
        self.root.join("rosters")
    }

    // ── Identities ────────────────────────────────────────────────────────────

    /// Directory holding identity TOML files: `<root>/identities/`.
    ///
    /// This is the directory `IdentityRegistry::open` expects.
    pub fn identities_root(&self) -> PathBuf {
        self.root.join("identities")
    }

    // ── Blacklist ─────────────────────────────────────────────────────────────

    /// Global blacklist file: `<root>/blacklist.toml`.
    ///
    /// The daemon's `WatcherActor` polls this on every `INBOX_SCAN_INTERVAL`
    /// tick and swaps the in-memory `BlacklistRegistry` on each successful
    /// reload.
    pub fn blacklist_path(&self) -> PathBuf {
        self.root.join("blacklist.toml")
    }

    // ── System actors ─────────────────────────────────────────────────────────

    /// System-actor registry file: `<root>/system/registry.toml`.
    ///
    /// Distinct from `agents/registry.toml`: system actors (e.g. the estate
    /// coordinator) are runtime-internal, not model-backed, and have no
    /// incarnation — see [`crate::system_registry::SystemRegistry`].
    pub fn system_registry_path(&self) -> PathBuf {
        self.root.join("system").join("registry.toml")
    }
}

/// Error resolving the default reeve data root from the environment.
#[derive(Debug)]
pub enum DataRootError {
    /// Neither `$HOME` nor `$XDG_DATA_HOME` was set (or both were empty).
    MissingHome,
    /// The environment variable held a relative path, which would silently
    /// resolve against the process cwd.
    RelativeDir {
        var_name: &'static str,
        path: PathBuf,
    },
}

impl fmt::Display for DataRootError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingHome => {
                f.write_str("resolving the reeve data root requires HOME or XDG_DATA_HOME")
            }
            Self::RelativeDir { var_name, path } => {
                write!(f, "{var_name} must be absolute, got {}", path.display())
            }
        }
    }
}

impl std::error::Error for DataRootError {}

/// Resolve the default reeve data root (`$XDG_DATA_HOME/reeve`, falling back
/// to `$HOME/.local/share/reeve`) — the directory every other on-disk path
/// hangs off. Identity TOMLs live in its `identities/` subdirectory; see
/// [`RuntimeLayout`] for the full tree.
pub fn default_data_root() -> Result<PathBuf, DataRootError> {
    crate::fs_util::resolve_reeve_data_root(
        std::env::var_os("XDG_DATA_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
    .map_err(|e| match e {
        crate::fs_util::XdgBaseError::MissingHome => DataRootError::MissingHome,
        crate::fs_util::XdgBaseError::RelativeDir { var_name, path } => {
            DataRootError::RelativeDir { var_name, path }
        }
    })
}

/// Entries that pre-engagement layouts nested under `identities/` and that
/// now live at the data root.
const LEGACY_NESTED_ENTRIES: &[&str] = &[
    "agents",
    "personas",
    "teams",
    "audit",
    "blacklist.toml",
    "replay-ledger.jsonl",
    "delivery-ledger.jsonl",
];

/// Move state that older layouts nested under `<root>/identities/` up to
/// `<root>/`. Identity TOML files stay where they are — `identities/` was
/// always their correct home; everything else had accumulated there.
///
/// Idempotent and conservative: an entry moves only when it exists at the
/// legacy path and nothing occupies the destination; a populated destination
/// wins and the legacy entry is left in place for the operator to reconcile.
/// Returns the entry names that were moved (for startup logging).
///
/// Agent registry records store absolute `inbox_dir` paths, so moving
/// `agents/` also rewrites any record whose `inbox_dir` still points under
/// the legacy root — otherwise every dispatch path that resolves an inbox
/// through the registry would deliver to directories that no longer exist.
pub fn migrate_legacy_identities_nesting(root: &Path) -> io::Result<Vec<&'static str>> {
    let legacy_root = root.join("identities");
    let mut moved = Vec::new();
    for entry in LEGACY_NESTED_ENTRIES {
        let from = legacy_root.join(entry);
        let to = root.join(entry);
        if from.symlink_metadata().is_ok() && to.symlink_metadata().is_err() {
            std::fs::rename(&from, &to)?;
            moved.push(*entry);
        }
    }
    if moved.contains(&"agents") {
        rewrite_legacy_inbox_dirs(root, &legacy_root)?;
    }
    Ok(moved)
}

/// Rewrite `inbox_dir` fields in the migrated agent registry that still
/// point under the legacy `identities/` root.
fn rewrite_legacy_inbox_dirs(root: &Path, legacy_root: &Path) -> io::Result<()> {
    let registry_path = RuntimeLayout::new(root).agent_registry_path();
    if registry_path.symlink_metadata().is_err() {
        return Ok(());
    }
    let mut registry = crate::agent_registry::AgentRegistry::open(registry_path)
        .map_err(|e| io::Error::other(format!("open agent registry for migration: {e}")))?;
    let stale: Vec<crate::agent_registry::AgentRecord> = registry.list().cloned().collect();
    for mut record in stale {
        let Ok(suffix) = record
            .inbox_dir
            .strip_prefix(legacy_root)
            .map(Path::to_path_buf)
        else {
            continue;
        };
        record.inbox_dir = root.join(suffix);
        registry
            .register(record)
            .map_err(|e| io::Error::other(format!("rewrite inbox_dir during migration: {e}")))?;
    }
    Ok(())
}
const AGENT_FILE_MODE: u32 = 0o600;

// ── Error type ────────────────────────────────────────────────────────────────

/// Errors produced by agent filesystem operations.
///
/// Not `Clone` or `PartialEq` because [`io::Error`] is neither.
#[derive(Debug)]
pub enum AgentFsError {
    /// Underlying filesystem error (open, read, write, mkdir, rename).
    Io { path: PathBuf, source: io::Error },
    /// JSON serialization or deserialization error.
    Json(serde_json::Error),
}

impl fmt::Display for AgentFsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "agent fs IO at {}: {source}", path.display())
            }
            Self::Json(source) => write!(f, "agent fs json: {source}"),
        }
    }
}

impl std::error::Error for AgentFsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Json(source) => Some(source),
        }
    }
}

impl AgentFsError {
    fn from_fs(err: FsCheckError) -> Self {
        match err {
            FsCheckError::Io { path, source } => Self::Io { path, source },
            FsCheckError::Symlink { path } => Self::Io {
                path,
                source: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "agent directory is a symlink; runtime refuses to follow it",
                ),
            },
            FsCheckError::NotADirectory { path } => Self::Io {
                path,
                source: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "agent path exists but is not a directory",
                ),
            },
            FsCheckError::WrongMode {
                path,
                actual,
                expected,
            } => Self::Io {
                path,
                source: io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("agent directory has mode 0o{actual:o}, expected 0o{expected:o}"),
                ),
            },
        }
    }
}

// ── AgentDirs ─────────────────────────────────────────────────────────────────

/// The directory structure for a named agent under `{data_dir}/agents/{name}/`.
///
/// The lead agent in the walking skeleton lives at `agents/lead/`. The name is
/// a human-readable role label (e.g., `"lead"`), not a UUID. UUID-keyed
/// per-identity inbox trees are managed by [`crate::inbox::InboxLayout`];
/// `AgentDirs` is the role-level overlay for conversation history, status, and
/// cost tracking.
///
/// Path accessors return computed `PathBuf`s on each call rather than caching
/// them as fields. The struct is cheap to construct and the extra allocation is
/// dominated by actual I/O at every call site.
#[derive(Debug)]
pub struct AgentDirs {
    /// `{data_dir}/agents/{name}/`
    root: PathBuf,
}

impl AgentDirs {
    /// Create all required subdirectories with mode `0o700` and return a
    /// handle.
    ///
    /// Directories created:
    /// - `{data_dir}/agents/`
    /// - `{data_dir}/agents/{name}/`
    /// - `{data_dir}/agents/{name}/inbox/`
    /// - `{data_dir}/agents/{name}/inbox/tmp/`
    /// - `{data_dir}/agents/{name}/inbox/new/`
    /// - `{data_dir}/agents/{name}/inbox/cur/`
    /// - `{data_dir}/agents/{name}/inbox/quarantine/`
    /// - `{data_dir}/agents/{name}/inbox/archive/`
    /// - `{data_dir}/agents/{name}/log/`
    ///
    /// Idempotent: existing directories with the correct mode succeed.
    pub fn provision(data_dir: &Path, name: &str) -> Result<Self, AgentFsError> {
        validate_agent_name(name)?;
        let root = root_path(data_dir, name);
        let inbox = root.join("inbox");
        let dirs = dirs_to_provision(data_dir, &root, &inbox);
        for dir in &dirs {
            ensure_directory(dir, AGENT_DIR_MODE).map_err(AgentFsError::from_fs)?;
        }
        Ok(Self { root })
    }

    /// Return a handle to an agent directory tree that was provisioned earlier.
    ///
    /// Does not create or verify any directories. Useful when the caller knows
    /// the layout was already provisioned and only needs path accessors.
    pub fn open(data_dir: &Path, name: &str) -> Result<Self, AgentFsError> {
        validate_agent_name(name)?;
        Ok(Self {
            root: root_path(data_dir, name),
        })
    }

    /// Root of this agent's directory: `{data_dir}/agents/{name}/`.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Log directory: `root/log/`.
    pub fn log_dir(&self) -> PathBuf {
        self.root.join("log")
    }

    /// Conversation journal: `root/log/conversation.jsonl`.
    pub fn conversation_path(&self) -> PathBuf {
        self.root.join("log").join("conversation.jsonl")
    }

    /// Last-written status file: `root/status`.
    ///
    /// Written atomically by [`AtomicFileWriter`]; content is a short
    /// machine-readable token (e.g., `"idle"`, `"working"`).
    pub fn status_path(&self) -> PathBuf {
        self.root.join("status")
    }

    /// Cumulative cost file: `root/cost`.
    ///
    /// Written atomically by [`AtomicFileWriter`]; content is a numeric string
    /// (e.g., `"0.0042"`).
    pub fn cost_path(&self) -> PathBuf {
        self.root.join("cost")
    }

    /// Persona config for this agent: `root/agent.toml`.
    ///
    /// Written by the agent spawner and read by the agent actor.
    pub fn agent_toml_path(&self) -> PathBuf {
        self.root.join("agent.toml")
    }

    /// Maildir inbox root: `root/inbox/`.
    ///
    /// Passed to [`crate::supervisor::WatcherActor`] as the watch target for
    /// incoming messages.
    pub fn inbox_root(&self) -> PathBuf {
        self.root.join("inbox")
    }

    /// Per-agent ed25519 seed file: `root/identity.key`.
    ///
    /// Written and read by [`crate::agent_registry::generate_or_load_keypair`].
    pub fn identity_key_path(&self) -> PathBuf {
        self.root.join("identity.key")
    }

    /// Capability profile snapshot for this agent: `root/profile.toml`.
    ///
    /// Written by the `SpawnCoordinator` at spawn time from the persona's
    /// `profile.toml`; read by tool actors to gate `InvokeTool` calls.
    /// Immutable for the agent's lifetime.
    pub fn profile_path(&self) -> PathBuf {
        self.root.join("profile.toml")
    }
}

// ── ConversationEntry ─────────────────────────────────────────────────────────

/// A single entry in the agent's conversation journal.
///
/// Serialized as JSON with a `"type"` discriminator field (`snake_case`).
/// Walking-skeleton scope: only the four variants needed for phase 7 are
/// defined. The full spec allows richer event types; they will be added as
/// variants here when downstream tasks require them.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConversationEntry {
    /// A message received from an external sender (inbound from the inbox).
    Inbound {
        /// Stable identifier from the message envelope.
        message_id: String,
        /// Identity that signed the envelope. `Option` for backward
        /// compatibility with journals written before sender attribution
        /// was wired through; legacy entries deserialize with `None` and
        /// downstream renderers fall back to an "unknown" label.
        #[serde(default)]
        sender_id: Option<reeve_types::IdentityId>,
        payload: String,
        #[serde(with = "time::serde::rfc3339")]
        timestamp_utc: OffsetDateTime,
    },
    /// A message produced by the agent and sent to a recipient.
    Outbound {
        payload: String,
        #[serde(with = "time::serde::rfc3339")]
        timestamp_utc: OffsetDateTime,
    },
    /// A call made to a language model.
    ModelCall {
        input_tokens: u32,
        output_tokens: u32,
        /// Model identifier string (e.g., `"claude-opus-4-7"`).
        model: String,
        #[serde(with = "time::serde::rfc3339")]
        timestamp_utc: OffsetDateTime,
    },
    /// A system-level annotation (startup, shutdown, error, restart).
    System {
        message: String,
        #[serde(with = "time::serde::rfc3339")]
        timestamp_utc: OffsetDateTime,
    },
    /// A tool invocation requested by the agent in an assistant turn. Paired
    /// with a [`ConversationEntry::ToolResult`] entry by `tool_use_id`.
    ToolUse {
        /// Provider-assigned identifier echoed in the matching result.
        tool_use_id: String,
        /// Tool name the agent invoked.
        name: String,
        /// Arguments the agent supplied.
        input: serde_json::Value,
        #[serde(with = "time::serde::rfc3339")]
        timestamp_utc: OffsetDateTime,
    },
    /// The result of a tool invocation, paired by `tool_use_id` to the
    /// originating [`ConversationEntry::ToolUse`] entry.
    ToolResult {
        /// Identifier of the [`ConversationEntry::ToolUse`] this answers.
        tool_use_id: String,
        /// Tool output as a string (structured outputs are JSON-encoded).
        content: String,
        /// `true` when the tool execution failed.
        is_error: bool,
        #[serde(with = "time::serde::rfc3339")]
        timestamp_utc: OffsetDateTime,
    },
}

// ── ConversationThread ────────────────────────────────────────────────────────

/// Append-only JSONL writer for an agent's conversation journal.
///
/// Within a process, concurrent appends are serialized by `Mutex<File>`.
/// Cross-process appends rely on POSIX `O_APPEND` atomicity: POSIX guarantees
/// `O_APPEND` makes the seek-then-write atomic (no two writers interleave),
/// but does not guarantee write atomicity for regular files. In practice,
/// single-call `write()` for entries well under 4096 bytes is non-torn on
/// Linux and macOS. Each entry must stay well under this bound; the
/// walking-skeleton variants satisfy it. Cross-process callers should keep
/// entries small; the in-process `Mutex` serializes concurrent writes within
/// a single process.
///
/// `Clone` is not derived. Wrap in `Arc<ConversationThread>` for shared
/// access.
#[derive(Debug)]
pub struct ConversationThread {
    path: PathBuf,
    file: Mutex<File>,
}

impl ConversationThread {
    /// Open or create the conversation journal at `path`.
    ///
    /// The file is opened with `O_APPEND | O_NOFOLLOW` so a symlink placed at
    /// `path` after the parent directory was created surfaces as an error
    /// rather than being silently followed. Mode `0o600` is applied on Unix.
    pub fn open(path: &Path) -> Result<Self, AgentFsError> {
        let file = open_jsonl_file(path, AGENT_FILE_MODE).map_err(|source| AgentFsError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(Self {
            path: path.to_path_buf(),
            file: Mutex::new(file),
        })
    }

    /// Serialize `entry` to JSON and append it as a newline-terminated record.
    ///
    /// `sync_data()` is called before returning so the record is durable
    /// before the caller proceeds. This matters for conversation replay after
    /// a crash: a record in the OS write buffer is not a record.
    ///
    /// # Atomicity
    ///
    /// `O_APPEND` makes the write atomic at the OS level for payloads ≤
    /// `PIPE_BUF` bytes. A crash mid-write produces either a complete prior
    /// line or no new line — no torn entries.
    #[must_use = "dropped Result means a journal entry may be silently lost"]
    pub fn append(&self, entry: &ConversationEntry) -> Result<(), AgentFsError> {
        let mut json_bytes = serde_json::to_vec(entry).map_err(AgentFsError::Json)?;
        json_bytes.push(b'\n');

        let mut file = self
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        file.write_all(&json_bytes)
            .map_err(|source| AgentFsError::Io {
                path: self.path.clone(),
                source,
            })?;
        file.sync_data().map_err(|source| AgentFsError::Io {
            path: self.path.clone(),
            source,
        })
    }
}

// ── AtomicFileWriter ──────────────────────────────────────────────────────────

/// Atomic overwrite writer for small text files (status, cost).
///
/// Uses the `NamedTempFile` → fsync → persist pattern so a crash mid-write
/// leaves the target either at its previous value or at the new value — never
/// in a partially-written state.
///
/// Both files live inside the agent root, which is on the same filesystem as
/// the temp file, so the rename underlying `persist` is always atomic.
#[derive(Debug)]
pub struct AtomicFileWriter {
    path: PathBuf,
    /// Parent directory of `path`; must be on the same filesystem so that
    /// `NamedTempFile::new_in` → `persist` is an atomic rename.
    dir: PathBuf,
}

impl AtomicFileWriter {
    /// Construct a writer for `path`.
    ///
    /// Returns `Err` if `path` has no parent directory (e.g., a bare filename
    /// with no preceding components). Any path rooted under a data directory
    /// satisfies this invariant. The parent is not verified to exist at
    /// construction time; an absent parent surfaces as an error on the first
    /// [`AtomicFileWriter::write`] call.
    pub fn new(path: PathBuf) -> Result<Self, AgentFsError> {
        let dir = path
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| AgentFsError::Io {
                path: path.clone(),
                source: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "AtomicFileWriter path must have a parent directory",
                ),
            })?;
        Ok(Self { path, dir })
    }

    /// Atomically overwrite the target file with `content`.
    ///
    /// Steps:
    /// 1. `NamedTempFile::new_in(&self.dir)` — temp file on same filesystem.
    /// 2. `apply_file_perms(tmp, 0o600)` — restrict before writing.
    /// 3. `write_all(content)` — data in buffer.
    /// 4. `sync_all()` — flush data + metadata to storage.
    /// 5. `persist(&self.path)` — atomic rename replaces the target.
    /// 6. `sync_directory` — flush directory entry to storage.
    pub fn write(&self, content: &str) -> Result<(), AgentFsError> {
        crate::fs_util::atomic_write_file(
            &self.path,
            &self.dir,
            content.as_bytes(),
            AGENT_FILE_MODE,
        )
        .map_err(|source| AgentFsError::Io {
            path: self.path.clone(),
            source,
        })
    }
}

// ── Private helpers ───────────────────────────────────────────────────────────

/// Rejects names that would enable path traversal outside the `agents/` subtree.
pub(crate) fn validate_agent_name(name: &str) -> Result<(), AgentFsError> {
    if name.is_empty()
        || name.contains('/')
        || name.contains('\0')
        || name == "."
        || name == ".."
        || name.chars().any(char::is_control)
    {
        return Err(AgentFsError::Io {
            path: PathBuf::from(name),
            source: io::Error::new(
                io::ErrorKind::InvalidInput,
                "agent name must be a non-empty single path component with no slashes",
            ),
        });
    }
    Ok(())
}

fn root_path(data_dir: &Path, name: &str) -> PathBuf {
    data_dir.join("agents").join(name)
}

fn dirs_to_provision<'a>(data_dir: &'a Path, root: &'a Path, inbox: &'a Path) -> [PathBuf; 9] {
    [
        data_dir.join("agents"),
        root.to_path_buf(),
        inbox.to_path_buf(),
        inbox.join("tmp"),
        inbox.join("new"),
        inbox.join("cur"),
        inbox.join("quarantine"),
        inbox.join("archive"),
        root.join("log"),
    ]
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;

    use tempfile::tempdir;

    #[test]
    fn migration_moves_legacy_entries_and_keeps_identity_tomls() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        let legacy = root.join("identities");
        fs::create_dir_all(legacy.join("agents").join("lead")).unwrap();
        fs::create_dir_all(legacy.join("personas").join("lead")).unwrap();
        fs::write(legacy.join("blacklist.toml"), b"entries = []").unwrap();
        fs::write(
            legacy.join("11111111-1111-1111-1111-111111111111.toml"),
            b"x",
        )
        .unwrap();

        let moved = migrate_legacy_identities_nesting(root).unwrap();

        assert_eq!(moved, vec!["agents", "personas", "blacklist.toml"]);
        assert!(root.join("agents").join("lead").is_dir());
        assert!(root.join("personas").join("lead").is_dir());
        assert!(root.join("blacklist.toml").is_file());
        assert!(
            legacy
                .join("11111111-1111-1111-1111-111111111111.toml")
                .is_file(),
            "identity TOMLs must stay in identities/"
        );
    }

    #[test]
    fn migration_rewrites_registry_inbox_dirs_under_legacy_root() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        let legacy = root.join("identities");
        let legacy_inbox = legacy.join("agents").join("lead").join("inbox");
        ensure_directory(&legacy_inbox, 0o700).unwrap();

        let mut registry =
            crate::agent_registry::AgentRegistry::open(legacy.join("agents").join("registry.toml"))
                .unwrap();
        registry
            .register(crate::agent_registry::AgentRecord {
                name: crate::agent_registry::ValidatedAgentName::new("lead").unwrap(),
                identity_id: reeve_types::IdentityId::new().unwrap(),
                inbox_dir: legacy_inbox,
                persona_name: None,
                spawned_at: OffsetDateTime::from_unix_timestamp(1_760_000_000).unwrap(),
                status: crate::agent_registry::AgentStatus::Running,
                stopped_reason: None,
            })
            .unwrap();
        drop(registry);

        let moved = migrate_legacy_identities_nesting(root).unwrap();
        assert!(moved.contains(&"agents"));

        let migrated = crate::agent_registry::AgentRegistry::open(
            RuntimeLayout::new(root).agent_registry_path(),
        )
        .unwrap();
        let record = migrated.lookup("lead").unwrap();
        assert_eq!(
            record.inbox_dir,
            root.join("agents").join("lead").join("inbox"),
            "inbox_dir must be rewritten to the migrated location"
        );
        assert!(record.inbox_dir.is_dir(), "rewritten path must exist");
    }

    #[test]
    fn migration_is_idempotent_and_never_clobbers_destination() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        let legacy = root.join("identities");
        fs::create_dir_all(legacy.join("teams")).unwrap();
        fs::write(legacy.join("teams").join("old.toml"), b"legacy").unwrap();
        fs::create_dir_all(root.join("teams")).unwrap();
        fs::write(root.join("teams").join("new.toml"), b"current").unwrap();

        let moved = migrate_legacy_identities_nesting(root).unwrap();
        assert!(moved.is_empty(), "populated destination must win");
        assert!(legacy.join("teams").join("old.toml").is_file());
        assert!(root.join("teams").join("new.toml").is_file());

        let again = migrate_legacy_identities_nesting(root).unwrap();
        assert!(again.is_empty());
    }

    #[test]
    fn migration_on_fresh_layout_is_a_no_op() {
        let tmp = tempdir().unwrap();
        let moved = migrate_legacy_identities_nesting(tmp.path()).unwrap();
        assert!(moved.is_empty());
    }

    // T1: provision creates all expected directories at the correct relative
    // paths.
    #[test]
    fn provision_creates_all_directories() {
        let tmp = tempdir().unwrap();
        let dirs = AgentDirs::provision(tmp.path(), "lead").unwrap();

        let root = dirs.root();
        assert!(root.is_dir(), "root missing");
        assert!(root.join("inbox").is_dir(), "inbox missing");
        assert!(root.join("inbox").join("tmp").is_dir(), "inbox/tmp missing");
        assert!(root.join("inbox").join("new").is_dir(), "inbox/new missing");
        assert!(root.join("inbox").join("cur").is_dir(), "inbox/cur missing");
        assert!(
            root.join("inbox").join("quarantine").is_dir(),
            "inbox/quarantine missing"
        );
        assert!(
            root.join("inbox").join("archive").is_dir(),
            "inbox/archive missing"
        );
        assert!(root.join("log").is_dir(), "log missing");

        // agents/ parent also created
        assert!(tmp.path().join("agents").is_dir(), "agents/ missing");
    }

    // T2: append then read — two distinct variants produce two parseable lines
    // with the correct "type" fields.
    #[test]
    fn conversation_thread_append_and_read() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("conversation.jsonl");
        let thread = ConversationThread::open(&path).unwrap();

        thread
            .append(&ConversationEntry::Inbound {
                message_id: String::from("msg-1"),
                sender_id: None,
                payload: String::from("hello"),
                timestamp_utc: OffsetDateTime::now_utc(),
            })
            .unwrap();

        thread
            .append(&ConversationEntry::Outbound {
                payload: String::from("world"),
                timestamp_utc: OffsetDateTime::now_utc(),
            })
            .unwrap();

        let content = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "expected exactly 2 lines");

        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(first["type"], "inbound");
        assert_eq!(second["type"], "outbound");
    }

    // T3: reopening the file does not truncate — the original entry survives.
    #[test]
    fn conversation_thread_is_append_only() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("conversation.jsonl");

        // First open: write one entry.
        {
            let thread = ConversationThread::open(&path).unwrap();
            thread
                .append(&ConversationEntry::System {
                    message: String::from("started"),
                    timestamp_utc: OffsetDateTime::now_utc(),
                })
                .unwrap();
        }

        // Second open: write another entry.
        {
            let thread = ConversationThread::open(&path).unwrap();
            thread
                .append(&ConversationEntry::System {
                    message: String::from("continued"),
                    timestamp_utc: OffsetDateTime::now_utc(),
                })
                .unwrap();
        }

        let content = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "expected 2 lines after two opens");

        // The first entry must still be the first line.
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(
            first["message"], "started",
            "first entry was overwritten on second open"
        );
    }

    // T4: AtomicFileWriter writes then overwrites — second write replaces first.
    #[test]
    fn atomic_file_writer_write_and_overwrite() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("status");
        let writer = AtomicFileWriter::new(path.clone()).unwrap();

        writer.write("idle").unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, "idle");

        writer.write("working").unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, "working");
    }

    // T5: each ConversationEntry variant serializes with the correct snake_case
    // "type" tag.
    #[test]
    fn conversation_entry_json_has_type_tag() {
        let now = OffsetDateTime::now_utc();
        let cases: &[(&str, ConversationEntry)] = &[
            (
                "inbound",
                ConversationEntry::Inbound {
                    message_id: String::from("x"),
                    sender_id: None,
                    payload: String::from("p"),
                    timestamp_utc: now,
                },
            ),
            (
                "outbound",
                ConversationEntry::Outbound {
                    payload: String::from("p"),
                    timestamp_utc: now,
                },
            ),
            (
                "model_call",
                ConversationEntry::ModelCall {
                    input_tokens: 10,
                    output_tokens: 20,
                    model: String::from("claude-opus-4-7"),
                    timestamp_utc: now,
                },
            ),
            (
                "system",
                ConversationEntry::System {
                    message: String::from("ok"),
                    timestamp_utc: now,
                },
            ),
        ];

        for (expected_type, entry) in cases {
            let json = serde_json::to_string(entry).unwrap();
            let value: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert_eq!(
                value["type"].as_str().unwrap(),
                *expected_type,
                "wrong type tag for {expected_type}: {json}",
            );
        }
    }

    // T6: AgentDirs::open returns the same root as provision without creating
    // directories. Path accessors return paths rooted at the expected locations.
    #[test]
    fn agent_dirs_open_and_path_accessors() {
        let tmp = tempdir().unwrap();
        let dirs = AgentDirs::provision(tmp.path(), "lead").unwrap();

        // open() produces the same root as provision().
        let opened = AgentDirs::open(tmp.path(), "lead").unwrap();
        assert_eq!(dirs.root(), opened.root());

        // Path accessors.
        let root = dirs.root().to_path_buf();
        assert_eq!(dirs.log_dir(), root.join("log"));
        assert_eq!(
            dirs.conversation_path(),
            root.join("log").join("conversation.jsonl")
        );
        assert_eq!(dirs.status_path(), root.join("status"));
        assert_eq!(dirs.cost_path(), root.join("cost"));
        assert_eq!(dirs.agent_toml_path(), root.join("agent.toml"));
        assert_eq!(dirs.inbox_root(), root.join("inbox"));
    }

    // T7: provision is idempotent — calling it twice with the correct modes
    // succeeds and the directory tree is unchanged.
    #[test]
    fn provision_is_idempotent() {
        let tmp = tempdir().unwrap();
        AgentDirs::provision(tmp.path(), "lead").unwrap();
        AgentDirs::provision(tmp.path(), "lead").unwrap();
    }

    // T8: AgentFsError Display impls are non-empty and contain path context.
    #[test]
    fn agent_fs_error_display_impls() {
        let path = PathBuf::from("synthetic/test-path");

        let io_err = AgentFsError::Io {
            path: path.clone(),
            source: io::Error::from(io::ErrorKind::PermissionDenied),
        };
        let rendered = io_err.to_string();
        assert!(!rendered.is_empty());
        assert!(rendered.contains("synthetic/test-path"), "Io: {rendered}");

        let serde_err = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
        let ser_err = AgentFsError::Json(serde_err);
        let rendered = ser_err.to_string();
        assert!(!rendered.is_empty());
        assert!(rendered.contains("json"), "Json: {rendered}");
    }

    // T9: AgentFsError::source returns the underlying error for the Io variant
    // and the serde error for the Json variant.
    #[test]
    fn agent_fs_error_source() {
        use std::error::Error as _;

        let path = PathBuf::from("x");
        let io_err = AgentFsError::Io {
            path: path.clone(),
            source: io::Error::from(io::ErrorKind::NotFound),
        };
        assert!(io_err.source().is_some(), "Io source should be Some");

        let serde_err = serde_json::from_str::<serde_json::Value>("bad").unwrap_err();
        let ser_err = AgentFsError::Json(serde_err);
        assert!(ser_err.source().is_some(), "Json source should be Some");
    }

    // T10: AgentFsError::from_fs maps all FsCheckError variants to Io with
    // informative messages. Exercises the Symlink, NotADirectory, and WrongMode
    // branches of from_fs that are not reached by provision tests.
    #[test]
    fn agent_fs_error_from_fs_all_branches() {
        use crate::fs_util::FsCheckError;

        let path = PathBuf::from("p");

        let sym = AgentFsError::from_fs(FsCheckError::Symlink { path: path.clone() });
        let sym_msg = sym.to_string();
        assert!(sym_msg.contains("symlink"), "Symlink branch: {sym_msg}");

        let not_dir = AgentFsError::from_fs(FsCheckError::NotADirectory { path: path.clone() });
        let not_dir_msg = not_dir.to_string();
        assert!(
            not_dir_msg.contains("not a directory"),
            "NotADirectory branch: {not_dir_msg}"
        );

        let wrong_mode = AgentFsError::from_fs(FsCheckError::WrongMode {
            path: path.clone(),
            actual: 0o755,
            expected: 0o700,
        });
        let wrong_mode_msg = wrong_mode.to_string();
        assert!(
            wrong_mode_msg.contains("755") || wrong_mode_msg.contains("mode"),
            "WrongMode branch: {wrong_mode_msg}"
        );

        let io_err = AgentFsError::from_fs(FsCheckError::Io {
            path,
            source: io::Error::from(io::ErrorKind::Other),
        });
        let io_msg = io_err.to_string();
        assert!(!io_msg.is_empty(), "Io branch: {io_msg}");
    }

    // G1: provision rejects an empty agent name.
    #[test]
    fn provision_rejects_empty_name() {
        let tmp = tempdir().unwrap();
        let result = AgentDirs::provision(tmp.path(), "");
        assert!(
            matches!(result, Err(AgentFsError::Io { .. })),
            "expected Io error for empty name, got {result:?}",
        );
    }

    // G2: provision rejects a name containing a slash.
    #[test]
    fn provision_rejects_slash_in_name() {
        let tmp = tempdir().unwrap();
        let result = AgentDirs::provision(tmp.path(), "foo/bar");
        assert!(
            matches!(result, Err(AgentFsError::Io { .. })),
            "expected Io error for slash in name, got {result:?}",
        );
    }

    // G3: provision rejects `..` as the agent name.
    #[test]
    fn provision_rejects_dotdot_name() {
        let tmp = tempdir().unwrap();
        let result = AgentDirs::provision(tmp.path(), "..");
        assert!(
            matches!(result, Err(AgentFsError::Io { .. })),
            "expected Io error for '..' name, got {result:?}",
        );
    }

    // G4: open rejects an empty agent name.
    #[test]
    fn open_rejects_empty_name() {
        let tmp = tempdir().unwrap();
        let result = AgentDirs::open(tmp.path(), "");
        assert!(
            matches!(result, Err(AgentFsError::Io { .. })),
            "expected Io error for empty name, got {result:?}",
        );
    }
}
