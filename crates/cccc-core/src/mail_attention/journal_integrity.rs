//! Validate relevant stored bytes before trusting the permissive history index.
//! Validation follows appended byte ranges; it retains neither bodies nor events.
use super::*;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::sync::Mutex;

struct Checked {
    revision: crate::ledger::SourceRevision,
    offset: u64,
    error: Option<String>,
}

pub(super) fn validate(path: &std::path::Path) -> io::Result<()> {
    static CHECKED: OnceLock<Mutex<BTreeMap<PathBuf, Checked>>> = OnceLock::new();
    let mut checked = CHECKED
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| io::Error::other("attention integrity lock poisoned"))?;
    // Append/compaction cannot expose an uncommitted partial fact while checked.
    let _writer = ledger::acquire_writer_lock(path)?;
    let revisions = ledger::revisions(path)?;
    if checked.len().saturating_add(revisions.len()) > 1024 {
        checked.clear();
    }
    for revision in revisions {
        let prior = checked.get(&revision.path);
        if let Some(prior) = prior.filter(|old| old.revision == revision) {
            if let Some(error) = &prior.error {
                return Err(io::Error::other(error.clone()));
            }
            continue;
        }
        let gzip = revision.path.extension().is_some_and(|e| e == "gz");
        // An earlier corrupt prefix cannot be certified by appending new bytes.
        let offset = prior
            .filter(|old| !gzip && old.error.is_none() && revision.len > old.revision.len)
            .map_or(0, |old| old.offset);
        let mut file = std::fs::File::open(&revision.path)?;
        let reader: Box<dyn Read> = if gzip {
            Box::new(flate2::read::GzDecoder::new(file))
        } else {
            file.seek(SeekFrom::Start(offset))?;
            Box::new(file)
        };
        let mut reader = BufReader::new(reader);
        let mut consumed = offset;
        let mut line = Vec::new();
        let result = (|| {
            loop {
                let read = reader.read_until(b'\n', &mut line)?;
                if read == 0 {
                    break;
                }
                consumed += read as u64;
                let value = serde_json::from_slice::<Value>(&line);
                let relevant = value
                    .as_ref()
                    .ok()
                    .and_then(|v| v.get("kind"))
                    .and_then(Value::as_str)
                    == Some("mail.attention")
                    || (value.is_err()
                        && line
                            .windows(b"mail.attention".len())
                            .any(|w| w == b"mail.attention"));
                if relevant {
                    let value = value
                        .map_err(|_| io::Error::other("malformed mail attention journal bytes"))?;
                    if value
                        .get("id")
                        .and_then(Value::as_str)
                        .is_none_or(str::is_empty)
                        || value
                            .get("ts")
                            .and_then(Value::as_str)
                            .is_none_or(|s| DateTime::parse_from_rfc3339(s).is_err())
                        || serde_json::from_value::<Event>(value).is_err()
                    {
                        return Err(io::Error::other("invalid stored mail attention identity"));
                    }
                }
                line.clear();
            }
            Ok(())
        })();
        let error = result.as_ref().err().map(ToString::to_string);
        checked.insert(
            revision.path.clone(),
            Checked {
                revision,
                offset: consumed,
                error,
            },
        );
        result?;
    }
    Ok(())
}
