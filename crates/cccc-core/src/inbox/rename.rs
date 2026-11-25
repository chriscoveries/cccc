use super::*;

pub fn validate_actor_cursor_rename(
    home: &HomeLayout,
    group_id: &str,
    new: &str,
) -> io::Result<()> {
    with_cursors(home, group_id, |_, cursors| {
        if cursors.contains_key(new) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "destination actor has a Mail cursor",
            ));
        }
        Ok(())
    })
}

pub fn rename_actor_cursor(
    home: &HomeLayout,
    group_id: &str,
    old: &str,
    new: &str,
) -> io::Result<()> {
    with_cursors(home, group_id, |cursor_path, cursors| {
        let Some(cursor) = cursors.remove(old) else {
            return Ok(());
        };
        if cursors.contains_key(new) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "destination actor has a Mail cursor",
            ));
        }
        cursors.insert(new.to_owned(), cursor);
        let ledger_path = GroupStore::new(home.clone())?.ledger_path(group_id)?;
        write_json(
            cursor_path,
            &json!({"schema":MAIL_CURSOR_SCHEMA,"cursors":cursors}),
        )?;
        projection::note_cursor(&ledger_path);
        Ok(())
    })
}

fn with_cursors(
    home: &HomeLayout,
    group_id: &str,
    change: impl FnOnce(&std::path::Path, &mut Map<String, Value>) -> io::Result<()>,
) -> io::Result<()> {
    let cursor_path = path(home, group_id)?;
    let lock_path = cursor_path.with_file_name("read_cursors.json.lock");
    with_exclusive_lock(&lock_path, || {
        // A recorded Mail read is authoritative even if its cursor write was
        // interrupted. Settle it under the same lock before changing the key.
        recover_pending_locked(home, group_id)?;
        let mut cursors = match read_json::<Value>(&cursor_path) {
            Ok(doc) if doc.get("schema").and_then(Value::as_u64) == Some(MAIL_CURSOR_SCHEMA) => doc
                .get("cursors")
                .and_then(Value::as_object)
                .cloned()
                .ok_or_else(|| io::Error::other("Mail cursor document is missing cursors"))?,
            Ok(_) => Map::new(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Map::new(),
            Err(error) => return Err(error),
        };
        change(&cursor_path, &mut cursors)
    })
}
