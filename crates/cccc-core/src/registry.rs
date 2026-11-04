use cccc_contracts::utc_now;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;

use crate::fs::{
    read_json, with_exclusive_lock, write_json, write_json_committed, write_json_committed_with,
};
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
            // Activation must receive an error on an ambiguous sync; readback
            // alone does not establish the required durability boundary.
            write_json(&home.registry_path(), &registry)?;
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
                Self::empty(home.clone())
            };
            let result = change(&mut registry)?;
            registry.updated_at = utc_now();
            registry.save_unlocked_with(write)?;
            Ok(result)
        })
    }

    fn load_unlocked(home: &HomeLayout) -> io::Result<Self> {
        let mut registry: Self = read_json(&home.registry_path())?;
        registry.home = Some(home.clone());
        Ok(registry)
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
        write_json_committed_with(&home.registry_path(), &stored, write)
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
        Registry::mutate(&home, |registry| {
            registry.defaults.insert("control".into(), "group".into());
            Ok(())
        })
        .expect("ordinary writer");
        assert_eq!(
            Registry::load_existing(&home)
                .expect("read")
                .mail_wake_activation,
            Some(header)
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
