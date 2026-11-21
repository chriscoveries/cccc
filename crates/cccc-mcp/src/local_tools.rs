use cccc_client::DaemonClient;
use cccc_core::HomeLayout;
use cccc_core::group_scope::EffectiveScope;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::ToolCallError;
use crate::router::{daemon, tool_result};

#[cfg(test)]
pub(crate) mod authority_tests;
#[cfg(test)]
mod web_file_tests;

pub async fn call(
    home: &HomeLayout,
    client: &DaemonClient,
    name: &str,
    args: Map<String, Value>,
) -> Result<Value, ToolCallError> {
    let file_action = match name {
        "cccc_file" => {
            if [
                "to",
                "text",
                "mode",
                "insight",
                "dst_group_id",
                "dst_instance_id",
                "idempotency_key",
            ]
            .iter()
            .any(|key| args.contains_key(*key))
            {
                return Err("cccc_file does not accept delivery arguments; use cccc_file_send to send an attachment".into());
            }
            let action = match args.get("action") {
                None => "read",
                Some(Value::String(action)) => action.as_str(),
                Some(_) => return Err("file action must be read, info or blob_path".into()),
            };
            if !matches!(action, "read" | "info" | "blob_path") {
                return Err(
                    "cccc_file only reads files; use cccc_file_send to send an attachment".into(),
                );
            }
            Some(action)
        }
        "cccc_file_send" => {
            if args.contains_key("action") {
                return Err("cccc_file_send sends an attachment and does not accept action; use cccc_file for reads".into());
            }
            Some("send")
        }
        _ => None,
    };
    let root = scope(client, &args).await?;
    // The daemon may have prepared the scope before Stop, while its reply was
    // still in flight. Keep using the captured identity, never a newer grant.
    revalidate(home, &args)?;
    let binding = args.get("_cccc_web_binding").cloned();
    let native_binding = args.get("_cccc_native_binding").cloned();
    let payload = match name {
        "cccc_repo" | "cccc_repo_edit" => {
            let operation = action(&args).to_owned();
            let tool = name.to_owned();
            let home = home.clone();
            tokio::task::spawn_blocking(move || {
                revalidate(&home, &args)?;
                crate::repo::call_tool(&root, &tool, &operation, &args)
            })
            .await
            .map_err(|error| format!("repository task failed: {error}"))??
        }
        "cccc_shell" => shell(&root, &args).await?,
        "cccc_git" => git(&root, &args).await?,
        "cccc_exec_command" => crate::local_sessions::start(home, &root, &args).await?,
        "cccc_write_stdin" => crate::local_sessions::write(home, &args).await?,
        "cccc_code_exec" => crate::code_mode::start(home, client, &root, &args).await?,
        "cccc_code_wait" => crate::code_mode::wait(home, client, &args).await?,
        "cccc_apply_patch" => apply_patch(&root, &args).await?,
        "cccc_file" | "cccc_file_send" => {
            let result = file(
                home,
                client,
                &root,
                &args,
                file_action.expect("file action"),
            )
            .await?;
            revalidate_binding(home, binding.as_ref())?;
            revalidate_native_binding(home, native_binding.as_ref())?;
            return Ok(result);
        }
        _ => return Err(format!("unsupported local tool: {name}").into()),
    };
    revalidate_binding(home, binding.as_ref())?;
    revalidate_native_binding(home, native_binding.as_ref())?;
    Ok(if matches!(name, "cccc_code_exec" | "cccc_code_wait") {
        crate::code_mode::tool_result(payload)
    } else {
        tool_result(payload)
    })
}

pub(crate) fn revalidate(home: &HomeLayout, args: &Map<String, Value>) -> Result<(), String> {
    if let Some(binding) = args.get("_cccc_native_binding") {
        if args.get("group_id") != binding.get("group_id")
            || args.get("by") != binding.get("actor_id")
            || args.contains_key("_cccc_web_binding")
        {
            return Err("permission_denied: Native request identity mismatch".into());
        }
        revalidate_native_binding(home, Some(binding))?;
    }
    let binding = args.get("_cccc_web_binding");
    if let Some(binding) = binding
        && (args.get("group_id") != binding.get("group_id")
            || args.get("by") != binding.get("actor_id"))
    {
        return Err(retired_authority());
    }
    revalidate_binding(home, binding)
}

fn revalidate_native_binding(home: &HomeLayout, binding: Option<&Value>) -> Result<(), String> {
    let Some(binding) = binding else {
        return Ok(());
    };
    let invalid =
        || "permission_denied: The captured native session authority is no longer valid".to_owned();
    let binding = serde_json::from_value(binding.clone()).map_err(|_| invalid())?;
    cccc_core::local_sessions::validate_binding(home, &binding).map_err(|_| invalid())
}

pub(crate) fn revalidate_binding(home: &HomeLayout, binding: Option<&Value>) -> Result<(), String> {
    let Some(binding) = binding else {
        return Ok(());
    };
    if (binding["transport"] == "mcp_events" && binding["dots_stage"] != "execution")
        || (binding["transport"] == "grok_webhook" && binding["grok_stage"] != "execution")
    {
        return Err(retired_authority());
    }
    cccc_core::web_model_connectors::validate_binding(home, binding)
        .map_err(|_| retired_authority())
}

fn retired_authority() -> String {
    "permission_denied: The captured Web Model authority is no longer valid".into()
}

async fn scope(client: &DaemonClient, args: &Map<String, Value>) -> Result<PathBuf, ToolCallError> {
    let group_id = args
        .get("group_id")
        .cloned()
        .ok_or("group_id is required")?;
    let mut request = Map::new();
    request.insert("group_id".into(), group_id);
    for key in ["by", "_cccc_web_binding", "_cccc_native_binding"] {
        if let Some(value) = args.get(key) {
            request.insert(key.into(), value.clone());
        }
    }
    let result = daemon(client, "group_show", request).await?;
    // The daemon owns the running process. Re-resolving Actor configuration in
    // this MCP process would move tools before the actual owner is restarted.
    let scope: EffectiveScope =
        serde_json::from_value(result.get("effective_scope").cloned().unwrap_or_default())
            .map_err(|_| "caller has no effective local scope")?;
    scope
        .path
        .canonicalize()
        .ok()
        .filter(|path| path.is_dir())
        .ok_or_else(|| "caller has no effective local scope".into())
}

async fn one_shot(root: &Path, cmd: Vec<String>, seconds: u64) -> Result<Value, String> {
    let (program, arguments) = cmd.split_first().ok_or("command is required")?;
    let mut command = cccc_runtime::background_command(program);
    command.args(arguments).current_dir(root);
    capture(&mut command, seconds, 2_000_000).await
}

async fn shell(root: &Path, args: &Map<String, Value>) -> Result<Value, String> {
    let cmd = command(args)?;
    let (program, arguments) = cmd.split_first().ok_or("command is required")?;
    let mut process = cccc_runtime::background_command(program);
    process
        .args(arguments)
        .current_dir(command_cwd(root, args)?)
        .envs(command_env(args)?);
    capture(&mut process, timeout(args), output_limit(args)).await
}

async fn capture(
    command: &mut std::process::Command,
    seconds: u64,
    limit: usize,
) -> Result<Value, String> {
    let output = cccc_runtime::capture_command(
        command,
        None,
        std::time::Duration::from_secs(seconds),
        limit,
    )
    .await
    .map_err(|error| error.to_string())?;
    Ok(json!({
        "exit_code":output.status.code(),
        "stdout":String::from_utf8_lossy(&output.stdout),
        "stderr":String::from_utf8_lossy(&output.stderr),
        "stdout_truncated":output.stdout_truncated,
        "stderr_truncated":output.stderr_truncated,
    }))
}

async fn git(root: &Path, args: &Map<String, Value>) -> Result<Value, String> {
    let action = action(args);
    let mut command = vec!["git".into()];
    match action {
        "status" => command.extend(["status".into(), "--short".into()]),
        "diff" => {
            command.push("diff".into());
            if args.get("staged").and_then(Value::as_bool).unwrap_or(false) {
                command.push("--staged".into());
            }
            append_git_paths(root, args, &mut command)?;
        }
        "log" => {
            let count = args
                .get("count")
                .and_then(Value::as_u64)
                .unwrap_or(20)
                .clamp(1, 100);
            command.extend([
                "log".into(),
                format!("-{count}"),
                "--oneline".into(),
                "--decorate".into(),
            ]);
        }
        "add" => {
            command.push("add".into());
            if args
                .get("all_changes")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                command.push("-A".into());
            } else {
                append_git_paths(root, args, &mut command)?;
                if command.len() == 2 {
                    return Err("path, paths, or all_changes=true is required for git add".into());
                }
            }
        }
        "commit" => {
            let message = args
                .get("message")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or("message is required for git commit")?;
            command.extend(["commit".into(), "-m".into(), message.into()]);
        }
        _ => return Err("git action must be status, diff, log, add, or commit".into()),
    }
    one_shot(root, command, timeout(args)).await
}

fn append_git_paths(
    root: &Path,
    args: &Map<String, Value>,
    command: &mut Vec<String>,
) -> Result<(), String> {
    let mut paths = args
        .get("paths")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if let Some(path) = args
        .get("path")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        paths.push(path.into());
    }
    if !paths.is_empty() {
        command.push("--".into());
    }
    for path in paths {
        crate::repo::resolve(root, &path, true)?;
        command.push(path);
    }
    Ok(())
}

pub(super) async fn apply_patch(root: &Path, args: &Map<String, Value>) -> Result<Value, String> {
    let patch = args
        .get("patch")
        .or_else(|| args.get("input"))
        .and_then(Value::as_str)
        .ok_or("patch is required")?;
    if patch.trim_start().starts_with("*** Begin Patch") {
        let changed = crate::local_patch::apply(root, patch)?;
        return Ok(json!({"applied":true,"files":changed}));
    }
    let mut command = cccc_runtime::background_command("git");
    command.args(["apply", "-"]).current_dir(root);
    let output = cccc_runtime::capture_command(
        &mut command,
        Some(patch.as_bytes()),
        std::time::Duration::from_secs(timeout(args)),
        2_000_000,
    )
    .await
    .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    Ok(json!({"applied":true}))
}

async fn file(
    home: &HomeLayout,
    client: &DaemonClient,
    root: &Path,
    args: &Map<String, Value>,
    action: &str,
) -> Result<Value, ToolCallError> {
    if action == "send" {
        return send_files(client, root, args).await;
    }
    let raw = first_non_blank(args, &["path", "rel_path"]).ok_or("path is required")?;
    if args.contains_key("dst_instance_id") {
        return Err(
            "Connect does not expose remote file tools; use the received local attachment path"
                .into(),
        );
    }
    let web_root = web_file_root();
    if action == "blob_path" || action == "info" {
        let path = file_path(home, args, root, raw, &web_root)?;
        // The resolved path is canonical; a link here is a swap raced in
        // after resolution, so Web Model callers refuse it outright.
        if args.contains_key("_cccc_web_binding")
            && path
                .symlink_metadata()
                .map(|meta| meta.file_type().is_symlink())
                .unwrap_or(false)
        {
            return Err("permission_denied: path must remain inside the allowed directory".into());
        }
        return Ok(tool_result(
            json!({"path":path,"bytes":path.metadata().map(|meta|meta.len()).unwrap_or(0)}),
        ));
    }
    let web_bound = args.contains_key("_cccc_web_binding");
    let args = args.clone();
    let home = home.clone();
    let root = root.to_path_buf();
    let raw = raw.to_owned();
    tokio::task::spawn_blocking(move || {
        revalidate(&home, &args)?;
        let path = file_path(&home, &args, &root, &raw, &web_root)?;
        if web_bound {
            crate::file_read::read_no_follow(&path, &args)
        } else {
            crate::file_read::read(&path, &args)
        }
    })
    .await
    .map_err(|error| format!("file read task failed: {error}"))?
    .map_err(Into::into)
}

/// Resolve one `cccc_file` path. A caller carrying `_cccc_web_binding` is
/// confined to the host-configured public directory, which must be set; a
/// `state/blobs/` name resolves only when the blob is carried by that
/// actor's own conversation. Every other caller keeps the effective-scope
/// resolution.
fn file_path(
    home: &HomeLayout,
    args: &Map<String, Value>,
    root: &Path,
    raw: &str,
    web_root: &WebFileRoot,
) -> Result<PathBuf, String> {
    if raw.starts_with("state/blobs/") {
        let group_id = args
            .get("group_id")
            .and_then(Value::as_str)
            .ok_or("group_id is required")?;
        if args.contains_key("_cccc_web_binding") {
            web_blob_path(home, group_id, args, raw)?;
        }
        return cccc_core::blobs::resolve(home, group_id, raw).map_err(|error| error.to_string());
    }
    if args.contains_key("_cccc_web_binding") {
        return web_file_path(web_file_root_dir(web_root)?, raw, false);
    }
    crate::repo::resolve(root, &scope_relative(root, raw), false)
}

/// Admit a `state/blobs/` read for a Web Model caller only when the blob is
/// carried by a message it sent or that was delivered to it.
fn web_blob_path(
    home: &HomeLayout,
    group_id: &str,
    args: &Map<String, Value>,
    raw: &str,
) -> Result<(), String> {
    let actor_id = args.get("by").and_then(Value::as_str).unwrap_or_default();
    let name = raw.strip_prefix("state/blobs/").unwrap_or(raw);
    if web_blob_attached(home, group_id, actor_id, name)? {
        Ok(())
    } else {
        Err("permission_denied: blob is not part of this conversation".into())
    }
}

/// Whether the blob `name` (the part after `state/blobs/`) is carried by an
/// attachment on a message the actor sent or that was delivered to it. The
/// ledger is the only record of which conversation owns a stored blob.
fn web_blob_attached(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
    name: &str,
) -> Result<bool, String> {
    if actor_id.is_empty() {
        return Ok(false);
    }
    let store = cccc_core::GroupStore::new(home.clone()).map_err(|error| error.to_string())?;
    let group = store.load(group_id).map_err(|error| error.to_string())?;
    let ledger = store
        .ledger_path(group_id)
        .map_err(|error| error.to_string())?;
    let mut attached = false;
    cccc_core::ledger::visit_newest_first(&ledger, |event| {
        let involved =
            event.by == actor_id || cccc_core::inbox::is_for_actor(&group, &event, actor_id);
        let carried = involved
            && event
                .data
                .get("attachments")
                .and_then(Value::as_array)
                .is_some_and(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.get("path"))
                        .filter_map(Value::as_str)
                        .any(|path| path.strip_prefix("state/blobs/").unwrap_or(path) == name)
                });
        if carried {
            attached = true;
            return std::ops::ControlFlow::Break(());
        }
        std::ops::ControlFlow::Continue(())
    })
    .map_err(|error| error.to_string())?;
    Ok(attached)
}

/// The host environment variable naming the one directory Web Model file
/// tools may reach.
const WEB_FILE_ROOT_ENV: &str = "CCCC_WEB_MODEL_FILE_ROOT";

/// The resolved `CCCC_WEB_MODEL_FILE_ROOT` setting for one call.
#[derive(Debug)]
enum WebFileRoot {
    /// Not configured: Web Model file tools are denied; the setting is
    /// required on any host that exposes them to a Web Model actor.
    Unset,
    /// Configured but not a usable directory: every confined path is denied.
    Invalid,
    /// The canonical allowed directory.
    Root(PathBuf),
}

/// Read the setting at the point of enforcement so each call sees live
/// configuration. Unset and unusable values both deny rather than silently
/// widening access.
fn web_file_root() -> WebFileRoot {
    let Some(raw) = std::env::var_os(WEB_FILE_ROOT_ENV) else {
        return WebFileRoot::Unset;
    };
    if raw.is_empty() {
        return WebFileRoot::Unset;
    }
    match PathBuf::from(raw).canonicalize() {
        Ok(root) if root.is_dir() => WebFileRoot::Root(root),
        _ => WebFileRoot::Invalid,
    }
}

/// The directory a Web Model caller is confined to. Only reached for
/// `_cccc_web_binding` callers; an absent or unusable setting denies rather
/// than falling back to effective-scope resolution.
fn web_file_root_dir(web_root: &WebFileRoot) -> Result<&Path, String> {
    match web_root {
        WebFileRoot::Root(root) => Ok(root),
        WebFileRoot::Invalid => Err(format!(
            "permission_denied: {WEB_FILE_ROOT_ENV} is not a usable directory"
        )),
        WebFileRoot::Unset => Err(format!(
            "permission_denied: {WEB_FILE_ROOT_ENV} is not configured"
        )),
    }
}

/// Resolve `raw` inside `root`, the only directory a Web Model caller's file
/// tools may reach. Relative paths join the root; absolute paths must already
/// name a location inside it.
/// Canonicalization refuses `..` components and symlinked spellings that land
/// outside the root. `allow_missing` admits a not-yet-existing path by lexical
/// containment so a send retry still reaches the daemon's idempotent lookup.
fn web_file_path(root: &Path, raw: &str, allow_missing: bool) -> Result<PathBuf, String> {
    let root = root.canonicalize().map_err(|error| error.to_string())?;
    let candidate = Path::new(raw);
    if candidate
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err("permission_denied: path must remain inside the allowed directory".into());
    }
    let candidate = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };
    let checked = match candidate.canonicalize() {
        Ok(path) => path,
        Err(error) if !allow_missing => {
            return Err(match error.kind() {
                std::io::ErrorKind::NotFound => format!("path not found: {raw}"),
                _ => error.to_string(),
            });
        }
        Err(_) => {
            // A send retry may name a source removed since the first
            // attempt. Resolve the deepest existing ancestor so a symlinked
            // directory cannot smuggle an outside spelling through the
            // lexical containment check below.
            let mut missing: Vec<&OsStr> = Vec::new();
            let mut ancestor = candidate.as_path();
            loop {
                match ancestor.canonicalize() {
                    Ok(dir) => {
                        let mut resolved = dir;
                        for part in missing.iter().rev() {
                            resolved.push(*part);
                        }
                        break resolved;
                    }
                    Err(_) => match (ancestor.file_name(), ancestor.parent()) {
                        (Some(name), Some(parent)) => {
                            missing.push(name);
                            ancestor = parent;
                        }
                        _ => break candidate.clone(),
                    },
                }
            }
        }
    };
    if !checked.starts_with(&root) {
        return Err("permission_denied: path escapes the allowed directory".into());
    }
    Ok(checked)
}

/// Files one message may carry; IM bridges split larger sets on their own.
const MAX_FILES_PER_SEND: usize = 10;

/// `cccc_file_send` takes one path, or a list sent together as one message
/// with one caption, so a set of screenshots arrives as one message. The flag
/// is whether the caller passed a list, which decides the receipt's shape.
fn send_paths(args: &Map<String, Value>) -> Result<(Vec<&str>, bool), String> {
    // Refused even beside path, so no caller thinks a second set of files was sent.
    if args.contains_key("paths") {
        return Err(format!(
            "invalid_args: use path, not paths; path takes a file path or a list of 1 to {MAX_FILES_PER_SEND} file paths"
        ));
    }
    let Some(Value::Array(values)) = args.get("path") else {
        return first_non_blank(args, &["path", "rel_path"])
            .map(|raw| (vec![raw], false))
            .ok_or_else(|| "path is required".to_owned());
    };
    let paths: Vec<&str> = values
        .iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
        .collect();
    if paths.is_empty() || paths.len() != values.len() || paths.len() > MAX_FILES_PER_SEND {
        return Err(format!(
            "path must be a file path or a list of 1 to {MAX_FILES_PER_SEND} file paths"
        ));
    }
    Ok((paths, true))
}

/// A path to send, checked by form only: inside the effective tool workspace
/// and not a stored blob. The daemon resolves links and reads the file after
/// its retry lookup, so a retry still gets its receipt after a source file is
/// removed. Paths stay under this call's workspace root.
fn send_path(root: &Path, raw: &str) -> Result<String, String> {
    if raw.starts_with("state/blobs/") {
        return Err("send expects a file under the effective tool scope".into());
    }
    let relative = scope_relative(root, raw);
    let relative = Path::new(&relative);
    if relative.is_absolute()
        || relative
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err("path must be relative and remain inside the active scope".into());
    }
    Ok(root.join(relative).to_string_lossy().into_owned())
}

/// Resolve every `cccc_file_send` source for a Web Model caller inside the
/// configured public directory. An absent or unusable setting denies; other
/// callers keep the effective-scope resolution.
fn web_send_paths(
    args: &Map<String, Value>,
    raws: &[&str],
    web_root: &WebFileRoot,
) -> Result<Option<Vec<PathBuf>>, String> {
    if !args.contains_key("_cccc_web_binding") {
        return Ok(None);
    }
    let root = web_file_root_dir(web_root)?;
    raws.iter()
        .map(|raw| {
            if raw.starts_with("state/blobs/") {
                return Err("send expects a file under the effective tool scope".into());
            }
            web_file_path(root, raw, true)
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

async fn send_files(
    client: &DaemonClient,
    root: &Path,
    args: &Map<String, Value>,
) -> Result<Value, ToolCallError> {
    let web_root = web_file_root();
    send_files_scoped(client, root, args, &web_root).await
}

/// `cccc_file_send` body. `web_root` is the host-configured public directory
/// resolved once per call.
async fn send_files_scoped(
    client: &DaemonClient,
    root: &Path,
    args: &Map<String, Value>,
    web_root: &WebFileRoot,
) -> Result<Value, ToolCallError> {
    let (raws, listed) = send_paths(args)?;
    // A Web Model caller may only send files from the configured public
    // directory. Every source is resolved before either send branch runs, so
    // one bad path sends nothing.
    let web_paths = web_send_paths(args, &raws, web_root)?;
    let mut request = args.clone();
    let default = crate::argument_normalization::default_new_message_mode(args, false);
    let message_mode = crate::argument_normalization::message_mode(&mut request, default)?;
    for key in ["action", "path", "rel_path"] {
        request.remove(key);
    }
    request.insert("message_mode".into(), Value::String(message_mode));
    crate::argument_normalization::normalize_message_author(&mut request);
    crate::argument_normalization::normalize_recipients(&mut request);
    if args.contains_key("dst_instance_id") {
        let scoped: Vec<String> = match &web_paths {
            Some(paths) => paths
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
            None => raws.iter().map(|raw| scope_relative(root, raw)).collect(),
        };
        request.insert("paths".into(), json!(scoped));
        crate::mapping::connect_destination(&mut request)?;
        let result = daemon(client, "connect_send_files", request).await?;
        return Ok(tool_result(
            json!({"accepted":true,"queued":result.get("queued"),"result":result}),
        ));
    }
    // The retry key names the logical send, as it does for Connect.
    crate::argument_normalization::alias(&mut request, "idempotency_key", "client_id");
    // Check every path before sending any: one bad path sends nothing.
    let paths: Vec<String> = match &web_paths {
        Some(paths) => paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
        None => raws
            .iter()
            .map(|raw| send_path(root, raw))
            .collect::<Result<Vec<_>, _>>()?,
    };
    request.insert("paths".into(), json!(paths));
    let result = daemon(client, "send_files", request).await?;
    let attachments = result["event"]["data"]["attachments"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    Ok(tool_result(if listed {
        json!({"sent":true,"attachments":attachments,"result":result})
    } else {
        json!({"sent":true,"attachment":attachments.first().cloned().unwrap_or(Value::Null),"result":result})
    }))
}

pub(super) fn command(args: &Map<String, Value>) -> Result<Vec<String>, String> {
    if let Some(values) = args.get("command").and_then(Value::as_array) {
        return Ok(values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect());
    }
    let raw = first_non_blank(args, &["cmd", "command"]).ok_or("cmd is required")?;
    shell_words::split(raw).map_err(|error| error.to_string())
}

pub(super) fn command_cwd(root: &Path, args: &Map<String, Value>) -> Result<PathBuf, String> {
    if ["cwd", "workdir"]
        .iter()
        .any(|key| args.get(*key).is_some_and(|value| !value.is_string()))
    {
        return Err("cwd and workdir must be relative directory strings".into());
    }
    let cwd = first_non_blank(args, &["cwd", "workdir"]).unwrap_or(".");
    let path = crate::repo::resolve(root, cwd, false)?;
    if !path.is_dir() {
        return Err("cwd must be a directory inside the effective tool scope".into());
    }
    Ok(path)
}

pub(super) fn command_env(args: &Map<String, Value>) -> Result<BTreeMap<String, String>, String> {
    let Some(env) = args.get("env") else {
        return Ok(BTreeMap::new());
    };
    let env = env
        .as_object()
        .ok_or("env must be an object of string values")?;
    env.iter()
        .map(|(key, value)| {
            let value = value.as_str().ok_or("env values must be strings")?;
            if key.is_empty() || key.contains(['=', '\0']) || value.contains('\0') {
                return Err("env contains an invalid variable name or value".into());
            }
            Ok((key.clone(), value.to_owned()))
        })
        .collect()
}

fn output_limit(args: &Map<String, Value>) -> usize {
    args.get("max_output_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(200_000)
        .clamp(1, 1_000_000) as usize
}

/// The file tools accept an absolute path inside the scope as the same file as
/// its relative path. Any other absolute path is left for `repo::resolve`,
/// which refuses it.
fn scope_relative(root: &Path, raw: &str) -> String {
    let path = Path::new(raw);
    if !path.is_absolute() {
        return raw.to_owned();
    }
    let canonical_root = root.canonicalize().ok();
    [
        Some(root.to_path_buf()),
        Some(cccc_runtime::compatible_path(root).to_path_buf()),
        canonical_root.clone(),
    ]
    .into_iter()
    .flatten()
    .find_map(|base| path.strip_prefix(base).ok().map(Path::to_path_buf))
    // Windows paths can also differ in case or UNC spelling. Compare the
    // actual existing file to the canonical scope before accepting it.
    .or_else(|| {
        path.canonicalize()
            .ok()?
            .strip_prefix(canonical_root.as_ref()?)
            .ok()
            .map(Path::to_path_buf)
    })
    .map_or_else(
        || raw.to_owned(),
        |rest| rest.to_string_lossy().into_owned(),
    )
}

fn first_non_blank<'a>(args: &'a Map<String, Value>, names: &[&str]) -> Option<&'a str> {
    names.iter().find_map(|name| {
        args.get(*name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    })
}

fn action(args: &Map<String, Value>) -> &str {
    args.get("action").and_then(Value::as_str).unwrap_or("read")
}
fn timeout(args: &Map<String, Value>) -> u64 {
    args.get("timeout_s")
        .or_else(|| args.get("timeout_seconds"))
        .and_then(Value::as_u64)
        .unwrap_or(60)
        .clamp(1, 600)
}

#[cfg(test)]
mod tests {
    use super::{command, send_paths, timeout};
    use crate::local_patch::apply as apply_codex_patch;
    use serde_json::json;

    #[cfg(windows)]
    #[test]
    fn absolute_file_path_matches_a_canonical_windows_scope() {
        let root = std::path::Path::new(r"\\?\C:\workspace\project");
        assert_eq!(
            super::scope_relative(root, r"C:\workspace\project\notes\result.txt"),
            r"notes\result.txt"
        );
        let outside = r"C:\workspace\other\secret.txt";
        assert_eq!(super::scope_relative(root, outside), outside);
        assert_eq!(
            super::scope_relative(root, r"\\?\C:\workspace\project\result.txt"),
            "result.txt"
        );

        let temp = tempfile::tempdir().expect("scope");
        let root = temp.path().join("MixedScope");
        std::fs::create_dir(&root).expect("scope");
        std::fs::write(root.join("Result.txt"), "fixture").expect("file");
        let canonical_root = root.canonicalize().expect("canonical scope");
        let other_case = root
            .join("result.txt")
            .to_string_lossy()
            .to_ascii_uppercase();
        assert_eq!(
            super::scope_relative(&canonical_root, &other_case),
            "Result.txt"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_honors_cwd_environment_and_output_limit() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("subdir");
        std::fs::create_dir(&cwd).expect("fixture operation");
        let args = json!({"command":["sh","-c","printf '%s\\n%s' \"$PWD\" \"$CCCC_TOOL_CONTRACT_VALUE\""],"cwd":"subdir","env":{"CCCC_TOOL_CONTRACT_VALUE":"fixture-value"}});
        let result = super::shell(temp.path(), args.as_object().expect("arguments"))
            .await
            .expect("fixture operation");
        assert_eq!(result["exit_code"], 0);
        assert_eq!(
            result["stdout"],
            format!(
                "{}\nfixture-value",
                cwd.canonicalize()
                    .expect("canonical fixture directory")
                    .display()
            )
        );
        let args = json!({"command":["sh","-c","printf 0123456789; printf failure >&2; exit 7"],"max_output_bytes":4});
        let result = super::shell(temp.path(), args.as_object().expect("arguments"))
            .await
            .expect("fixture operation");
        assert_eq!(result["exit_code"], 7);
        assert_eq!(result["stdout"], "0123");
        assert_eq!(result["stderr"], "fail");
        assert_eq!(result["stdout_truncated"], true);
        assert_eq!(result["stderr_truncated"], true);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_interprets_operators_only_when_explicitly_requested() {
        let temp = tempfile::tempdir().expect("tempdir");
        let args = json!({"command":"printf '%s ' 'a|b' '&&' '$HOME'"});
        let result = super::shell(temp.path(), args.as_object().expect("arguments"))
            .await
            .expect("fixture operation");
        assert_eq!(result["stdout"], "a|b && $HOME ");
        let args = json!({"command":"sh -c 'printf first && printf second | cat > result.txt'"});
        let result = super::shell(temp.path(), args.as_object().expect("arguments"))
            .await
            .expect("fixture operation");
        assert_eq!(result["exit_code"], 0);
        assert_eq!(result["stdout"], "first");
        assert_eq!(
            std::fs::read_to_string(temp.path().join("result.txt")).expect("fixture operation"),
            "second"
        );
    }

    #[test]
    fn command_options_keep_workspace_boundaries_and_schema_defaults() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("subdir");
        std::fs::create_dir(&cwd).expect("fixture operation");
        std::fs::write(temp.path().join("file"), "data").expect("fixture operation");
        for path in ["../", "file", "missing"] {
            assert!(
                super::command_cwd(
                    temp.path(),
                    json!({"cwd":path}).as_object().expect("arguments")
                )
                .is_err()
            );
        }
        assert!(
            super::command_cwd(
                temp.path(),
                json!({"cwd":temp.path()}).as_object().expect("arguments")
            )
            .is_err()
        );
        assert!(
            super::command_cwd(
                temp.path(),
                json!({"cwd":123}).as_object().expect("arguments")
            )
            .is_err()
        );
        assert_eq!(
            super::command_cwd(
                temp.path(),
                json!({"workdir":"subdir"}).as_object().expect("arguments")
            )
            .expect("fixture operation"),
            cwd.canonicalize().expect("canonical fixture directory")
        );
        let defaults = serde_json::Map::new();
        assert_eq!(super::timeout(&defaults), 60);
        assert_eq!(super::output_limit(&defaults), 200_000);
        assert_eq!(
            super::output_limit(
                json!({"max_output_bytes":999999999})
                    .as_object()
                    .expect("arguments")
            ),
            1_000_000
        );
        assert!(
            super::command_env(
                json!({"env":{"PRIVATE_VALUE":123}})
                    .as_object()
                    .expect("arguments")
            )
            .is_err()
        );
        assert!(
            !super::command_env(
                json!({"env":{"BAD=NAME":"sensitive"}})
                    .as_object()
                    .expect("arguments")
            )
            .expect_err("invalid environment")
            .contains("sensitive")
        );
    }

    #[cfg(unix)]
    #[test]
    fn command_cwd_rejects_symlink_escape() {
        let temp = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("tempdir");
        std::os::unix::fs::symlink(outside.path(), temp.path().join("escape"))
            .expect("fixture operation");
        assert!(
            super::command_cwd(
                temp.path(),
                json!({"cwd":"escape"}).as_object().expect("arguments")
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timed_out_shell_cannot_continue_writing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let error = super::one_shot(
            temp.path(),
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "sleep 1.3; printf late > marker".into(),
            ],
            1,
        )
        .await
        .expect_err("timeout");
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert!(error.contains("timed out"));
        assert!(
            !temp.path().join("marker").exists(),
            "timed-out tool continued mutating files"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_shell_cannot_continue_writing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let pending = super::one_shot(
            temp.path(),
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "sleep 0.3; printf late > marker".into(),
            ],
            10,
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), pending)
                .await
                .is_err()
        );
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert!(
            !temp.path().join("marker").exists(),
            "cancelled tool continued mutating files"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_reports_truncated_output_and_preserves_failure_status() {
        let temp = tempfile::tempdir().expect("tempdir");
        let output = super::one_shot(
            temp.path(),
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "head -c 2000001 /dev/zero; printf error >&2; exit 7".into(),
            ],
            3,
        )
        .await
        .expect("captured failure");
        assert_eq!(output["exit_code"], 7);
        assert_eq!(output["stdout"].as_str().expect("stdout").len(), 2_000_000);
        assert_eq!(output["stdout_truncated"], true);
        assert_eq!(output["stderr"], "error");
        assert_eq!(output["stderr_truncated"], false);
    }

    #[tokio::test]
    async fn unified_patch_uses_the_finite_command_path() {
        let temp = tempfile::tempdir().expect("tempdir");
        // git apply follows Git's line-ending settings, and Git for Windows sets
        // core.autocrlf system-wide by default; pin it to compare exact bytes.
        for args in [&["init", "-q"][..], &["config", "core.autocrlf", "false"]] {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(temp.path())
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?}");
        }
        std::fs::write(temp.path().join("file.txt"), "before\n").expect("fixture");
        let args = json!({"patch":"diff --git a/file.txt b/file.txt\n--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-before\n+after\n"}).as_object().cloned().expect("args");
        let result = super::apply_patch(temp.path(), &args)
            .await
            .expect("apply unified diff");
        assert_eq!(result["applied"], true);
        assert_eq!(
            std::fs::read_to_string(temp.path().join("file.txt")).expect("file"),
            "after\n"
        );
        assert!(
            super::apply_patch(temp.path(), &args).await.is_err(),
            "rejected patch must remain an error"
        );
    }

    #[test]
    fn shell_timeout_uses_the_schema_field_and_keeps_the_legacy_alias() {
        let current = json!({"timeout_s":600,"timeout_seconds":15})
            .as_object()
            .cloned()
            .expect("arguments");
        assert_eq!(timeout(&current), 600);
        let legacy = json!({"timeout_seconds":45})
            .as_object()
            .cloned()
            .expect("legacy arguments");
        assert_eq!(timeout(&legacy), 45);
    }

    #[test]
    fn shell_command_skips_an_empty_primary_alias() {
        let args = json!({"cmd":" ","command":"printf fallback"})
            .as_object()
            .cloned()
            .expect("arguments");

        assert_eq!(command(&args).expect("command"), ["printf", "fallback"]);
    }

    #[test]
    fn file_send_refuses_paths_and_parses_mode_like_message_send() {
        let args = |value: serde_json::Value| value.as_object().cloned().expect("arguments");
        for value in [
            json!({"paths":["a.png"]}),
            json!({"path":"a.png","paths":["b.png"]}),
            json!({"path":["a.png"],"paths":["b.png"]}),
        ] {
            assert!(
                send_paths(&args(value.clone()))
                    .expect_err("paths")
                    .starts_with("invalid_args: use path, not paths;"),
                "{value}"
            );
        }
        let mode = |value: serde_json::Value| {
            crate::argument_normalization::message_mode(&mut args(value), "mail")
        };
        assert_eq!(
            mode(json!({"mode":"request_reply"})).expect("explicit"),
            "request_reply"
        );
        for value in [
            json!({"mode":"attention"}),
            json!({"mode":null}),
            json!({"mode":3}),
            json!({"message_mode":"send"}),
        ] {
            assert!(mode(value.clone()).is_err(), "{value}");
        }
    }

    #[test]
    fn applies_codex_add_update_and_delete_sections() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("edit.txt"), "alpha\nbeta\ngamma\n").expect("edit");
        std::fs::write(temp.path().join("delete.txt"), "gone\n").expect("delete");
        let patch = "*** Begin Patch\n*** Update File: edit.txt\n@@\n alpha\n-beta\n+changed\n gamma\n*** Add File: added.txt\n+new\n+file\n*** Delete File: delete.txt\n*** End Patch";
        let files = apply_codex_patch(temp.path(), patch).expect("patch");
        assert_eq!(files, ["edit.txt", "added.txt", "delete.txt"]);
        assert_eq!(
            std::fs::read_to_string(temp.path().join("edit.txt")).expect("edit"),
            "alpha\nchanged\ngamma\n"
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("added.txt")).expect("added"),
            "new\nfile\n"
        );
        assert!(!temp.path().join("delete.txt").exists());
    }
}
