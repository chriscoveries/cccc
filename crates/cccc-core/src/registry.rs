use cccc_contracts::utc_now;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io;

use crate::fs::{read_json, with_exclusive_lock, write_json_committed, write_json_committed_with};
use crate::home::HomeLayout;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroupMeta {
    pub group_id: String,
    pub title: String,
    #[serde(default)]
    pub topic: String,
    pub path: String,
    #[serde(default)]
    pub default_scope_key: String,
    pub created_at: String,
    pub updated_at: String,
}

/// Durable activation binding. This is not a Group inventory.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MailWakeActivation {
    pub version: u8,
    pub activation_id: String,
    pub proof_digest: String,
    pub implementation: String,
    pub home_identity: String,
    pub phase: MailWakePhase,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MailWakePhase {
    Preparing,
    Active,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Registry {
    pub v: u8,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub groups: BTreeMap<String, GroupMeta>,
    #[serde(default)]
    pub defaults: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mail_wake_activation: Option<MailWakeActivation>,
    /// SHA-256 of the canonical Registry JSON object without this field.
    /// Absent on legacy registries until Mail-wake activation is introduced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mail_wake_registry_sha256: Option<String>,
    #[serde(skip)]
    home: Option<HomeLayout>,
}

impl Registry {
    #[must_use]
    pub fn empty(home: HomeLayout) -> Self {
        let now = utc_now();
        Self {
            v: 1,
            created_at: now.clone(),
            updated_at: now,
            groups: BTreeMap::new(),
            defaults: BTreeMap::new(),
            mail_wake_activation: None,
            mail_wake_registry_sha256: None,
            home: Some(home),
        }
    }

    pub fn load(home: &HomeLayout) -> io::Result<Self> {
        home.initialize().map_err(io::Error::other)?;
        let path = home.registry_path();
        if path.exists() {
            return Self::load_unlocked(home);
        }
        with_exclusive_lock(&registry_lock_path(home), || {
            if path.exists() {
                Self::load_unlocked(home)
            } else {
                if mail_wake_evidence_exists(home)? {
                    return Err(io::Error::other("registry missing with Mail wake evidence"));
                }
                let registry = Self::empty(home.clone());
                registry.save_unlocked()?;
                Ok(registry)
            }
        })
    }

    /// Read only: missing Registry is not a new-home or migration proof.
    pub fn load_existing(home: &HomeLayout) -> io::Result<Self> {
        let registry = Self::load_unlocked(home)?;
        if registry.v != 1
            || registry
                .groups
                .iter()
                .any(|(id, meta)| id.is_empty() || id != &meta.group_id)
        {
            return Err(io::Error::other("invalid existing registry"));
        }
        Ok(registry)
    }

    /// Mutate an already-present Registry without initializing the Home or
    /// manufacturing an empty Registry if the original was lost.
    pub fn mutate_existing<T>(
        home: &HomeLayout,
        change: impl FnOnce(&mut Self) -> io::Result<T>,
    ) -> io::Result<T> {
        with_exclusive_lock(&registry_lock_path(home), || {
            let mut registry = Self::load_existing(home)?;
            let result = change(&mut registry)?;
            registry.updated_at = utc_now();
            registry.seal_mail_wake()?;
            // Activation must receive an error on an ambiguous sync; readback
            // alone does not establish the required durability boundary.
            write_activation_registry(&home.registry_path(), &registry)?;
            Ok(result)
        })
    }

    pub fn mutate<T>(
        home: &HomeLayout,
        change: impl FnOnce(&mut Self) -> io::Result<T>,
    ) -> io::Result<T> {
        Self::mutate_with_writer(home, change, write_json_committed)
    }

    pub(crate) fn mutate_with_writer<T>(
        home: &HomeLayout,
        change: impl FnOnce(&mut Self) -> io::Result<T>,
        write: impl FnOnce(&std::path::Path, &Self) -> io::Result<()>,
    ) -> io::Result<T> {
        home.initialize().map_err(io::Error::other)?;
        with_exclusive_lock(&registry_lock_path(home), || {
            let mut registry = if home.registry_path().exists() {
                Self::load_unlocked(home)?
            } else {
                if mail_wake_evidence_exists(home)? {
                    return Err(io::Error::other("registry missing with Mail wake evidence"));
                }
                Self::empty(home.clone())
            };
            let result = change(&mut registry)?;
            registry.updated_at = utc_now();
            registry.save_unlocked_with(write)?;
            Ok(result)
        })
    }

    fn load_unlocked(home: &HomeLayout) -> io::Result<Self> {
        let value: serde_json::Value = read_json(&home.registry_path())?;
        verify_mail_wake_digest(&value)?;
        if value
            .get("mail_wake_activation")
            .is_none_or(serde_json::Value::is_null)
            && mail_wake_evidence_exists(home)?
        {
            return Err(io::Error::other("registry missing Mail wake activation"));
        }
        let mut registry: Self = serde_json::from_value(value).map_err(io::Error::other)?;
        registry.home = Some(home.clone());
        Ok(registry)
    }

    /// Seal the exact logical Registry object, excluding only its checksum.
    /// This is integrity detection, not an authorization or recovery proof.
    pub fn seal_mail_wake(&mut self) -> io::Result<()> {
        self.mail_wake_registry_sha256 = None;
        if self.mail_wake_activation.is_some() {
            let value = serde_json::to_value(&*self).map_err(io::Error::other)?;
            self.mail_wake_registry_sha256 = Some(registry_digest(value)?);
        }
        Ok(())
    }

    fn save_unlocked(&self) -> io::Result<()> {
        self.save_unlocked_with(write_json_committed)
    }

    fn save_unlocked_with(
        &self,
        write: impl FnOnce(&std::path::Path, &Self) -> io::Result<()>,
    ) -> io::Result<()> {
        let home = self
            .home
            .as_ref()
            .ok_or_else(|| io::Error::other("registry has no home"))?;
        let mut stored = self.clone();
        stored.updated_at = utc_now();
        stored.home = None;
        stored.seal_mail_wake()?;
        write_json_committed_with(&home.registry_path(), &stored, write)
    }
}

fn mail_wake_evidence_exists(home: &HomeLayout) -> io::Result<bool> {
    home.root()
        .join("state")
        .join("mail-wake.json")
        .try_exists()
}

fn write_activation_registry(path: &std::path::Path, registry: &Registry) -> io::Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("registry parent missing"))?;
    let mut bytes = serde_json::to_vec_pretty(registry).map_err(io::Error::other)?;
    bytes.push(b'\n');
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(&bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|error| error.error)?;
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| io::Error::other(format!("published; durability unknown: {error}")))
}

fn registry_digest(mut value: serde_json::Value) -> io::Result<String> {
    let object = value
        .as_object_mut()
        .ok_or_else(|| io::Error::other("invalid registry object"))?;
    object.remove("mail_wake_registry_sha256");
    let bytes = serde_json::to_vec(&value).map_err(io::Error::other)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn verify_mail_wake_digest(value: &serde_json::Value) -> io::Result<()> {
    let object = value
        .as_object()
        .ok_or_else(|| io::Error::other("invalid registry object"))?;
    let active = object
        .get("mail_wake_activation")
        .is_some_and(|entry| !entry.is_null());
    match (active, object.get("mail_wake_registry_sha256")) {
        (false, None) => Ok(()),
        (true, Some(serde_json::Value::String(digest)))
            if *digest == registry_digest(value.clone())? =>
        {
            Ok(())
        }
        _ => Err(io::Error::other("Mail wake registry integrity unknown")),
    }
}

fn registry_lock_path(home: &HomeLayout) -> std::path::PathBuf {
    home.root().join("registry.json.lock")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::write_json;

    #[test]
    fn wake_header_survives_registry_mutation_and_no_create_read_does_not_rebuild() {
        let capture: cccc_contracts::Event = serde_json::from_str(include_str!(
            "../../cccc-daemon/src/ops/mail_wake/fixtures/mail.json"
        ))
        .expect("captured Mail envelope");
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        assert!(Registry::load_existing(&home).is_err());
        assert!(!home.registry_path().exists());
        Registry::load(&home).expect("existing registry setup");
        // Labeled header control, not an authorization or a real activation.
        let header = MailWakeActivation {
            version: 1,
            activation_id: capture.id,
            proof_digest: "a".repeat(64),
            implementation: "control".into(),
            home_identity: "control".into(),
            phase: MailWakePhase::Active,
        };
        Registry::mutate_existing(&home, |registry| {
            registry.mail_wake_activation = Some(header.clone());
            Ok(())
        })
        .expect("install control");
        let first_checksum = Registry::load_existing(&home)
            .expect("checked registry")
            .mail_wake_registry_sha256
            .expect("checksum");
        assert_eq!(first_checksum.len(), 64);
        Registry::mutate(&home, |registry| {
            registry.defaults.insert("control".into(), "group".into());
            Ok(())
        })
        .expect("ordinary writer");
        let updated = Registry::load_existing(&home).expect("checked updated registry");
        assert_eq!(updated.mail_wake_activation, Some(header));
        assert_ne!(
            updated.mail_wake_registry_sha256.as_deref(),
            Some(first_checksum.as_str())
        );
        std::fs::remove_file(home.registry_path()).expect("labeled registry loss");
        assert!(Registry::load_existing(&home).is_err());
        assert!(Registry::mutate_existing(&home, |_| Ok(())).is_err());
        assert!(!home.registry_path().exists());
    }

    #[test]
    fn committed_registry_replace_survives_a_trailing_sync_error() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        Registry::load(&home).expect("initialize");
        let result = Registry::mutate_with_writer(
            &home,
            |registry| {
                registry.defaults.insert("scope".into(), "g_test".into());
                Ok(())
            },
            |path, value| {
                write_json(path, value)?;
                Err(io::Error::other("injected sync_dir failure"))
            },
        );
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            Registry::load(&home).expect("registry").defaults["scope"],
            "g_test"
        );
    }
}
