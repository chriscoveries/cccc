//! Read-only observations of an owned Devin TUI. Missing authority stays unknown.
//! Input invalidates completion before any byte is written, including failed writes.
use crate::process_tree::devin_binding::{Binding, Root};
use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;
use std::collections::HashSet;
use std::collections::hash_map::DefaultHasher;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const MAX_FILE: u64 = 8 * 1024 * 1024;
const MAX_MESSAGE: usize = 256 * 1024;
const MAX_CHAIN: usize = 128;

#[cfg(test)]
mod tests;

#[derive(Clone)]
pub struct DevinPtyState {
    pub state: &'static str,
    pub updated_at: String,
    pub actor_generation: String,
}

pub(crate) struct Tracker {
    root: Root,
    state: Mutex<Tracking>,
}

#[derive(Default)]
struct Tracking {
    binding: Option<Binding>,
    fence: Option<Anchor>,
    admitted: HashSet<String>,
    payloads: HashSet<u64>,
}

#[derive(Clone)]
struct Anchor {
    tip: Option<i64>,
    user: Option<String>,
    step: u64,
    expected: String,
}

#[derive(Deserialize)]
struct Message {
    message_id: String,
    role: String,
    content: String,
    tool_calls: Option<Vec<serde::de::IgnoredAny>>,
}

#[derive(PartialEq, Eq)]
struct Snapshot {
    tip: Option<i64>,
    message_id: Option<String>,
    role: String,
    content: String,
    tools: bool,
    user: Option<String>,
    user_text: Option<String>,
    chain: Vec<i64>,
    activity: i64,
    created: i64,
    tip_created: i64,
}

#[derive(Deserialize)]
struct Transcript {
    schema_version: String,
    session_id: String,
    steps: Vec<Step>,
}

#[derive(Deserialize)]
struct Step {
    step_id: u64,
    timestamp: String,
    source: String,
    message: String,
    tool_calls: Option<Vec<serde::de::IgnoredAny>>,
    metrics: Option<Metrics>,
}

#[derive(Deserialize)]
struct Metrics {
    completion_tokens: u64,
}

impl Tracker {
    pub(crate) fn new(
        pid: u32,
        cwd: PathBuf,
        group: String,
        actor: String,
        generation: String,
    ) -> Self {
        Self {
            root: Root::new(pid, cwd, group, actor, generation),
            state: Mutex::new(Tracking::default()),
        }
    }

    /// Called with the PTY input lane held, outside the Session lifecycle lock.
    pub(crate) fn invalidate(&self) {
        if let Ok(mut tracked) = self.state.lock() {
            tracked.fence = None;
        }
    }

    /// Only a complete managed submission supplies a native user-message join.
    /// Arbitrary terminal keystrokes can invalidate, but cannot create authority.
    pub(crate) fn begin_submission(&self, payload: &[u8], source_ids: &[String]) {
        let Ok(mut tracked) = self.state.lock() else {
            return;
        };
        let Some(expected) = tracked.admit(payload, source_ids) else {
            return;
        };
        let Some(binding) = self.binding(&mut tracked) else {
            return;
        };
        let Some(before) = read_snapshot(&binding, &self.root.cwd, None) else {
            return;
        };
        let Some(transcript) = read_transcript(&binding) else {
            return;
        };
        let Some(step) = transcript.steps.last() else {
            return;
        };
        let Some(after) = read_snapshot(&binding, &self.root.cwd, None) else {
            return;
        };
        if before != after || !binding.current(&self.root) {
            return;
        }
        tracked.fence = Some(Anchor {
            tip: after.tip,
            user: after.user,
            step: step.step_id,
            expected,
        });
    }

    /// The caller holds the input lane and rechecks the runtime registration.
    pub(crate) fn observe(&self) -> Option<DevinPtyState> {
        let mut tracked = self.state.lock().ok()?;
        let binding = self.binding(&mut tracked)?;
        let stop = tracked.fence.as_ref().and_then(|anchor| anchor.tip);
        let before = read_snapshot(&binding, &self.root.cwd, stop)?;
        let transcript = read_transcript(&binding)?;
        let after = read_snapshot(&binding, &self.root.cwd, stop)?;
        if before != after || !binding.current(&self.root) {
            return None;
        }
        let step = transcript.steps.last()?;
        // Provider persistence timestamps can trail transcript completion.
        // Ordering authority is the input fence and active ancestry, not a clock window.
        chrono::DateTime::parse_from_rfc3339(&step.timestamp).ok()?;
        let state = classify(&after, step, tracked.fence.as_ref())?;
        Some(DevinPtyState {
            state,
            updated_at: step.timestamp.clone(),
            actor_generation: self.root.generation.clone(),
        })
    }

    fn binding(&self, tracked: &mut Tracking) -> Option<Binding> {
        if let Some(binding) = tracked.binding.as_ref() {
            if binding.current(&self.root) {
                return Some(binding.clone());
            }
            tracked.binding = None;
            tracked.fence = None;
        }
        let binding = self.root.bind()?;
        // A new registration or native session starts unknown, never from old completion.
        tracked.binding = Some(binding.clone());
        tracked.fence = None;
        Some(binding)
    }
}

impl Tracking {
    fn admit(&mut self, payload: &[u8], source_ids: &[String]) -> Option<String> {
        // Even an unavailable baseline or an uncertain write retires old authority.
        self.fence = None;
        // A local retry cannot turn an older native input into a fresh completion.
        // Never evict identities: after the bound, hints stay unknown until restart.
        if source_ids.is_empty()
            || source_ids.len() > 64
            || source_ids
                .iter()
                .any(|id| id.is_empty() || id.len() > 128 || self.admitted.contains(id))
            || source_ids.iter().collect::<HashSet<_>>().len() != source_ids.len()
            || self.admitted.len() + source_ids.len() > 4096
        {
            return None;
        }
        self.admitted.extend(source_ids.iter().cloned());
        let payload = std::str::from_utf8(payload).ok()?;
        let expected = payload
            .strip_prefix("\u{1b}[200~")
            .and_then(|text| text.strip_suffix("\u{1b}[201~"))
            .unwrap_or(payload);
        if expected.is_empty() || expected.len() > MAX_MESSAGE || expected.contains('\u{1b}') {
            return None;
        }
        if !expected.ends_with(&format!(
            "[CCCC delivery sources: {}]",
            source_ids.join(",")
        )) {
            return None;
        }
        // Some native system notices omit their event IDs in rendered text.
        // Even different source IDs cannot prove a fresh turn for equal text.
        // Hash collisions only withhold a hint; they cannot admit a duplicate.
        let mut hasher = DefaultHasher::new();
        expected.hash(&mut hasher);
        if !self.payloads.insert(hasher.finish()) {
            return None;
        }
        Some(expected.to_owned())
    }
}

fn classify(snapshot: &Snapshot, step: &Step, anchor: Option<&Anchor>) -> Option<&'static str> {
    if step.source != "agent" || step.step_id == 0 || step.tool_calls.is_none() {
        return None;
    }
    if snapshot.role == "tool" || (snapshot.role == "assistant" && snapshot.tools) {
        return Some("working");
    }
    let anchor = anchor?;
    // A new row or timestamp is insufficient: follow the active native chain past
    // the pre-input tip to a different accepted user message and its final answer.
    if step.step_id <= anchor.step
        || step
            .metrics
            .as_ref()
            .is_none_or(|metrics| metrics.completion_tokens == 0)
        || snapshot.role != "assistant"
        || snapshot.tools
        || step
            .tool_calls
            .as_ref()
            .is_none_or(|calls| !calls.is_empty())
        || snapshot.content != step.message
        || snapshot.user.is_none()
        || snapshot.user == anchor.user
        || snapshot.user_text.as_deref() != Some(anchor.expected.as_str())
        || snapshot.message_id.is_none()
        || anchor.tip.is_some_and(|tip| !snapshot.chain.contains(&tip))
    {
        return None;
    }
    Some("idle")
}

fn read_transcript(binding: &Binding) -> Option<Transcript> {
    let path = binding
        .directory
        .join("transcripts")
        .join(format!("{}.json", binding.session));
    let mut file = File::open(&path).ok()?;
    let before = file.metadata().ok()?;
    if !before.is_file() || before.len() > MAX_FILE {
        return None;
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX_FILE + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    let after = file.metadata().ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let current = std::fs::metadata(&path).ok()?;
        if current.ino() != after.ino() || current.dev() != after.dev() {
            return None;
        }
    }
    if bytes.len() as u64 != before.len()
        || before.len() != after.len()
        || before.modified().ok()? != after.modified().ok()?
    {
        return None;
    }
    let transcript: Transcript = serde_json::from_slice(&bytes).ok()?;
    if transcript.schema_version != "ATIF-v1.7" || transcript.session_id != binding.session {
        return None;
    }
    Some(transcript)
}

fn read_snapshot(binding: &Binding, cwd: &Path, stop: Option<i64>) -> Option<Snapshot> {
    let db = Connection::open_with_flags(
        binding.directory.join("sessions.db"),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    db.busy_timeout(Duration::ZERO).ok()?;
    db.pragma_update(None, "query_only", true).ok()?;
    db.pragma_update(None, "trusted_schema", false).ok()?;
    let ticks = AtomicUsize::new(0);
    db.progress_handler(
        1000,
        Some(move || ticks.fetch_add(1, Ordering::Relaxed) >= 127),
    )
    .ok()?;
    let transaction = db.unchecked_transaction().ok()?;
    let (directory, created, activity, tip): (String, i64, i64, Option<i64>) = transaction.query_row(
        "SELECT working_directory,created_at,last_activity_at,main_chain_id FROM sessions WHERE id=?1",
        [&binding.session], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))).ok()?;
    // Session creation can follow the lock/process by minutes (or be older on
    // resume). Identity comes from the owned process and its current lock.
    if Path::new(&directory).canonicalize().ok()? != cwd || created <= 0 {
        return None;
    }
    let mut snapshot = Snapshot {
        tip,
        message_id: None,
        role: String::new(),
        content: String::new(),
        tools: false,
        user: None,
        user_text: None,
        chain: Vec::new(),
        activity,
        created,
        tip_created: 0,
    };
    let mut next = tip;
    let mut seen = HashSet::new();
    for _ in 0..MAX_CHAIN {
        let Some(node) = next else { break };
        if !seen.insert(node) {
            return None;
        }
        let (parent, encoded, created): (Option<i64>, String, i64) = transaction.query_row(
            "SELECT parent_node_id,chat_message,created_at FROM message_nodes WHERE session_id=?1 AND node_id=?2 AND length(CAST(chat_message AS BLOB))<=?3 ORDER BY row_id DESC LIMIT 1",
            rusqlite::params![binding.session, node, MAX_MESSAGE],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).ok()?;
        let message: Message = serde_json::from_str(&encoded).ok()?;
        if message.message_id.is_empty() {
            return None;
        }
        if message.role == "assistant" && message.tool_calls.is_none() {
            return None;
        }
        snapshot.chain.push(node);
        if snapshot.message_id.is_none() {
            snapshot.message_id = Some(message.message_id.clone());
            snapshot.role = message.role.clone();
            snapshot.content = message.content.clone();
            snapshot.tools = message
                .tool_calls
                .as_ref()
                .is_some_and(|calls| !calls.is_empty());
            snapshot.tip_created = created;
        }
        if message.role == "user" && snapshot.user.is_none() {
            snapshot.user = Some(message.message_id);
            snapshot.user_text = Some(message.content);
        }
        if stop == Some(node) || (stop.is_none() && snapshot.user.is_some()) {
            next = None;
            break;
        }
        next = parent;
    }
    if next.is_some() {
        return None;
    }
    transaction.commit().ok()?;
    Some(snapshot)
}
