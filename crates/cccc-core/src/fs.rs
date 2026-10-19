use fs2::FileExt;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;

pub fn atomic_write(path: &Path, data: &[u8]) -> io::Result<()> {
    atomic_write_with(path, data, false)
}

/// Atomic replace that carries the destination's existing permissions onto the replacement.
///
/// Internal state files are fine with the temp file's private mode, but a user's workspace file
/// is not: replacing a `0755` script through a fresh temp file would silently drop its
/// executable bit and its group/other read access.
pub fn atomic_write_preserving_mode(path: &Path, data: &[u8]) -> io::Result<()> {
    atomic_write_with(path, data, true)
}

fn atomic_write_with(path: &Path, data: &[u8], preserve_mode: bool) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("path has no parent"))?;
    fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(data)?;
    temp.as_file().sync_all()?;
    // Applied before persist so the destination is never briefly owner-only.
    if preserve_mode && let Ok(existing) = fs::metadata(path) {
        temp.as_file().set_permissions(existing.permissions())?;
    }
    temp.persist(path).map_err(|error| error.error)?;
    sync_dir(parent)
}

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    bytes.push(b'\n');
    atomic_write(path, &bytes)
}

pub fn write_json_committed<T>(path: &Path, value: &T) -> io::Result<()>
where
    T: Serialize + DeserializeOwned + PartialEq,
{
    write_json_committed_with(path, value, write_json)
}

pub(crate) fn write_json_committed_with<T>(
    path: &Path,
    value: &T,
    write: impl FnOnce(&Path, &T) -> io::Result<()>,
) -> io::Result<()>
where
    T: Serialize + DeserializeOwned + PartialEq,
{
    match write(path, value) {
        Ok(()) => Ok(()),
        Err(error) => match read_json::<T>(path) {
            Ok(actual) if actual == *value => Ok(()),
            _ => Err(error),
        },
    }
}

pub fn write_secret_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    write_json(path, value)?;
    protect(path)
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> io::Result<T> {
    serde_json::from_slice(&fs::read(path)?).map_err(io::Error::other)
}

pub fn write_yaml<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let text = serde_yaml::to_string(value).map_err(io::Error::other)?;
    let text = quote_rfc3339_yaml_scalars(&text);
    atomic_write(path, text.as_bytes())
}

fn quote_rfc3339_yaml_scalars(text: &str) -> String {
    text.split_inclusive('\n')
        .map(|line| {
            let body = line.strip_suffix('\n').unwrap_or(line);
            let newline = if line.ends_with('\n') { "\n" } else { "" };
            let Some((prefix, scalar)) = yaml_scalar_parts(body) else {
                return line.to_owned();
            };
            if chrono::DateTime::parse_from_rfc3339(scalar).is_err() {
                return line.to_owned();
            }
            format!(
                "{prefix}{}{}",
                serde_json::to_string(scalar).unwrap_or_else(|_| "\"\"".into()),
                newline
            )
        })
        .collect()
}

fn yaml_scalar_parts(line: &str) -> Option<(&str, &str)> {
    let trimmed = line.trim_start();
    if let Some(scalar) = trimmed.strip_prefix("- ") {
        let prefix_len = line.len() - trimmed.len() + 2;
        return Some((&line[..prefix_len], scalar.trim()));
    }
    let separator = line.find(": ")?;
    let scalar_start = separator + 2;
    Some((&line[..scalar_start], line[scalar_start..].trim()))
}

pub fn write_yaml_committed<T>(path: &Path, value: &T) -> io::Result<()>
where
    T: Serialize + DeserializeOwned + PartialEq,
{
    match write_yaml(path, value) {
        Ok(()) => Ok(()),
        Err(error) => match read_yaml::<T>(path) {
            Ok(actual) if actual == *value => Ok(()),
            _ => Err(error),
        },
    }
}

pub fn write_secret_yaml<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    write_yaml(path, value)?;
    protect(path)
}

pub fn read_yaml<T: DeserializeOwned>(path: &Path) -> io::Result<T> {
    let text = fs::read_to_string(path)?;
    validate_yaml_tags(&path.display().to_string(), &text)?;
    let value: serde_yaml::Value = serde_yaml::from_str(&text).map_err(io::Error::other)?;
    serde_yaml::from_value(value).map_err(io::Error::other)
}

/// CCCC never evaluates YAML tags, and `serde_yaml` does not preserve them
/// either: it drops an unknown tag such as `!!js` and hands back the plain
/// scalar, so a Value walk cannot see one. That silent drop is the defect —
/// firestar's `mcp-cccc` entry carried `!!js process.env.CCCC_HOME`, the tag
/// evaporated, and the string `process.env.CCCC_HOME` was used as the value.
/// Only the source text still knows a tag was written, so the check reads the
/// text and names the tag, line and key instead of silently accepting it.
fn tagged_yaml_source(text: &str) -> Option<(String, usize, String)> {
    for (index, line) in text.lines().enumerate() {
        let mut in_single = false;
        let mut in_double = false;
        let mut previous = '\0';
        let chars: Vec<char> = line.chars().collect();
        let mut position = 0;
        while position < chars.len() {
            let current = chars[position];
            if current == '\'' && !in_double {
                in_single = !in_single;
            } else if current == '"' && !in_single && previous != '\\' {
                in_double = !in_double;
            } else if current == '!' && !in_single && !in_double {
                let tagged = if chars.get(position + 1) == Some(&'!') {
                    let mut end = position + 2;
                    while end < chars.len() && (chars[end].is_alphanumeric() || chars[end] == '_') {
                        end += 1;
                    }
                    Some(chars[position..end].iter().collect::<String>())
                } else if chars.get(position + 1) == Some(&'<') {
                    chars[position..]
                        .iter()
                        .position(|item| *item == '>')
                        .map(|end| chars[position..=position + end].iter().collect::<String>())
                } else {
                    None
                };
                if let Some(tag) = tagged {
                    let key = line
                        .split_once(':')
                        .map(|(key, _)| key.trim().to_owned())
                        .filter(|key| !key.is_empty())
                        .unwrap_or_else(|| line.trim().to_owned());
                    return Some((tag, index + 1, key));
                }
            }
            previous = current;
            position += 1;
        }
    }
    None
}

/// Reject YAML tags in a document CCCC reads or is about to write, such as the
/// runtime MCP profile. Generated config is held to the same rule as config
/// CCCC reads: plain values only, so a tag can never reach a runtime loader
/// that silently degrades it to `{}`.
pub fn validate_yaml_tags(context: &str, text: &str) -> io::Result<()> {
    if let Some((tag, line, key)) = tagged_yaml_source(text) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{context}: YAML tag `{tag}` at line {line} (key `{key}`): CCCC does not evaluate YAML tags; use a plain value (YAML scalars are passed through verbatim)"
            ),
        ));
    }
    Ok(())
}

pub fn with_exclusive_lock<T>(
    path: &Path,
    operation: impl FnOnce() -> io::Result<T>,
) -> io::Result<T> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    file.lock_exclusive()?;
    let result = operation();
    // The descriptor is dropped on return and releases the advisory lock even if an explicit
    // unlock reports an OS-level error. Do not turn a committed operation into an ambiguous
    // failure after its durable write has already completed.
    let _ = FileExt::unlock(&file);
    result
}

#[cfg(unix)]
fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(unix)]
fn protect(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn protect(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn sync_dir(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{quote_rfc3339_yaml_scalars, read_yaml};

    #[test]
    fn python_yaml_loaders_keep_rfc3339_values_as_strings() {
        let output = quote_rfc3339_yaml_scalars(
            "created_at: 2026-07-31T01:02:03.456789Z\nitems:\n- 2026-07-31T01:02:03Z\n",
        );
        assert_eq!(
            output,
            "created_at: \"2026-07-31T01:02:03.456789Z\"\nitems:\n- \"2026-07-31T01:02:03Z\"\n"
        );
    }

    /// The managed profile the pre-fix daemon actually wrote on firestar. This
    /// is the incident input, not a hand-written approximation: four `!!js`
    /// tags (lines 11 and 20-22). `serde_yaml` keeps no trace of them - it
    /// hands back the inner scalar - which is exactly the silent degradation
    /// the check exists to stop. The reader must refuse the file, not load it.
    #[test]
    fn real_tagged_profile_is_rejected_before_the_parser_drops_the_tag() {
        // Byte-exact capture: /var/lib/cccc/runtimes/deepseek/0.1.0-rc.6/
        // profiles/cccc-acp/cordis.yml on firestar, mtime 2026-10-19
        // 16:39:28 UTC, 647 bytes, sha256
        // d5acf7b985c068462093f52ec52ed541d8cf1dc6d743f106f647743b052cfed9.
        let text = include_str!("../tests/fixtures/real_deepseek_acp_cordis_tagged.yml");
        assert_eq!(text.len(), 647, "fixture must stay a byte-exact capture");

        // Pin the reason the check reads source text instead of a value.
        let parsed: serde_yaml::Value = serde_yaml::from_str(text).expect("parses");
        let entries = parsed.as_sequence().expect("app composition is a list");
        assert_eq!(
            entries[1]["config"]["persistenceRoot"],
            serde_yaml::Value::String("process.env.CCCC_DEEPSEEK_SESSION_ROOT".into()),
            "serde_yaml drops the unknown tag and keeps the scalar"
        );
        assert_eq!(
            entries[2]["config"]["env"]["CCCC_HOME"],
            serde_yaml::Value::String("process.env.CCCC_HOME".into()),
            "the MCP child env is the tag that made cccc fail to start"
        );

        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp
            .path()
            .join("runtimes/deepseek/0.1.0-rc.6/profiles/cccc-acp/cordis.yml");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("profile home");
        std::fs::write(&path, text).expect("write config");
        let error = read_yaml::<serde_yaml::Value>(&path).expect_err("tag must be rejected");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        let message = error.to_string();
        assert!(message.contains("cordis.yml"), "{message}");
        assert!(message.contains("!!js"), "{message}");
        assert!(message.contains("line 11"), "{message}");
        assert!(message.contains("persistenceRoot"), "{message}");
        assert!(message.contains("use a plain value"), "{message}");
    }

    #[test]
    fn plain_yaml_still_loads() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.yaml");
        std::fs::write(
            &path,
            "mcp_servers:\n  cccc:\n    env:\n      CCCC_HOME: /var/lib/cccc\n",
        )
        .expect("write config");
        let value = read_yaml::<serde_yaml::Value>(&path).expect("plain yaml loads");
        assert_eq!(
            value["mcp_servers"]["cccc"]["env"]["CCCC_HOME"],
            serde_yaml::Value::String("/var/lib/cccc".into())
        );
    }

    /// A document CCCC is about to write is held to the same rule as one it
    /// reads: the generated runtime profile must never carry `!!js`.
    #[test]
    fn generated_config_with_a_tag_is_rejected_before_it_is_written() {
        let error = super::validate_yaml_tags(
            "runtimes/deepseek/profiles/cccc-acp/cordis.yml",
            "- id: cccc-mcp\n  config:\n    env:\n      CCCC_HOME: !!js process.env.CCCC_HOME\n",
        )
        .expect_err("a tagged generated config must be rejected");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        let message = error.to_string();
        assert!(message.contains("cordis.yml"), "{message}");
        assert!(message.contains("!!js"), "{message}");
        assert!(message.contains("line 4"), "{message}");
        assert!(message.contains("CCCC_HOME"), "{message}");
        super::validate_yaml_tags(
            "cordis.yml",
            "- id: cccc-mcp\n  config:\n    command: '/usr/local/bin/cccc'\n",
        )
        .expect("plain generated config is accepted");
        // A `!!` inside a quoted scalar is data, not a tag.
        super::validate_yaml_tags("notes.yml", "note: 'ship !!js as text'\n")
            .expect("quoted tag text is data");
    }
}
