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
    let value: serde_yaml::Value =
        serde_yaml::from_reader(File::open(path)?).map_err(io::Error::other)?;
    reject_yaml_tags(path, &value)?;
    serde_yaml::from_value(value).map_err(io::Error::other)
}

/// CCCC never evaluates YAML tags. The parser hands an unknown tag such as
/// `!!js` through as an opaque node, and every downstream consumer reads that
/// node as an empty value — firestar's `mcp-cccc` entry loaded as `{}` for
/// exactly this reason and failed plugin load with no useful message. Name the
/// tag and its key path instead of silently degrading.
fn yaml_tag_error(context: &str, value: &serde_yaml::Value) -> Option<String> {
    let (tag, at) = tagged_yaml_node(value, "")?;
    Some(format!(
        "{context}: unknown YAML tag `{tag}` at `{at}`: CCCC does not evaluate YAML tags; use a plain value (YAML scalars are passed through verbatim)"
    ))
}

fn tagged_yaml_node(value: &serde_yaml::Value, at: &str) -> Option<(String, String)> {
    let here = || {
        if at.is_empty() {
            "<root>".to_owned()
        } else {
            at.to_owned()
        }
    };
    match value {
        serde_yaml::Value::Tagged(tagged) => Some((tagged.tag.to_string(), here())),
        serde_yaml::Value::Mapping(map) => {
            for (key, item) in map {
                let key = match key {
                    serde_yaml::Value::String(name) => name.clone(),
                    other => format!("{other:?}"),
                };
                let child = if at.is_empty() {
                    key
                } else {
                    format!("{at}.{key}")
                };
                if let Some(found) = tagged_yaml_node(item, &child) {
                    return Some(found);
                }
            }
            None
        }
        serde_yaml::Value::Sequence(items) => {
            for (index, item) in items.iter().enumerate() {
                let child = if at.is_empty() {
                    format!("[{index}]")
                } else {
                    format!("{at}[{index}]")
                };
                if let Some(found) = tagged_yaml_node(item, &child) {
                    return Some(found);
                }
            }
            None
        }
        _ => None,
    }
}

fn reject_yaml_tags(path: &Path, value: &serde_yaml::Value) -> io::Result<()> {
    match yaml_tag_error(&path.display().to_string(), value) {
        Some(message) => Err(io::Error::new(io::ErrorKind::InvalidData, message)),
        None => Ok(()),
    }
}

/// Reject YAML tags in a document CCCC is about to write, such as the runtime
/// MCP profile. Generated config is held to the same rule as config CCCC
/// reads: plain values only, so a tag can never reach a runtime loader that
/// silently degrades it to `{}`.
pub fn validate_yaml_tags(context: &str, text: &str) -> io::Result<()> {
    let value: serde_yaml::Value = serde_yaml::from_str(text).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{context}: invalid YAML: {error}"),
        )
    })?;
    match yaml_tag_error(context, &value) {
        Some(message) => Err(io::Error::new(io::ErrorKind::InvalidData, message)),
        None => Ok(()),
    }
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

    /// The captured entry from the firestar `mcp-cccc` incident: the tag
    /// evaluated to `{}` and the plugin failed to load with no hint about why.
    #[test]
    fn unknown_yaml_tag_is_rejected_with_the_tag_and_key_path() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.yaml");
        std::fs::write(
            &path,
            "mcp_servers:\n  cccc:\n    command: /usr/local/bin/cccc\n    args: [mcp]\n    env:\n      CCCC_HOME: !!js process.env.CCCC_HOME\n",
        )
        .expect("write config");
        let error = read_yaml::<serde_yaml::Value>(&path).expect_err("tag must be rejected");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        let message = error.to_string();
        assert!(message.contains("!!js"), "{message}");
        assert!(
            message.contains("mcp_servers.cccc.env.CCCC_HOME"),
            "{message}"
        );
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
        assert!(message.contains("[0].config.env.CCCC_HOME"), "{message}");
        super::validate_yaml_tags(
            "cordis.yml",
            "- id: cccc-mcp\n  config:\n    command: '/usr/local/bin/cccc'\n",
        )
        .expect("plain generated config is accepted");
    }
}
