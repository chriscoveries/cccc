//! Recognition/HOLD only. No radio or slider mutation and no family switching.
use anyhow::{Context, Result, bail};
use cccc_contracts::{Actor, ActorRuntime, web_model_pin::WebModelSelection};
use cccc_core::{GroupStore, HomeLayout, actors, web_model_connectors};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

use super::{BrowserSurfaces, Session};

pub(super) const INSPECT_PIN: &str = include_str!("chatgpt_model_pin.js");

pub(crate) struct ModelPinAuthority {
    home: HomeLayout,
    group: String,
    actor: Actor,
    binding: Option<(String, String, String)>,
}

impl ModelPinAuthority {
    pub(crate) fn load(home: &HomeLayout, group: &str, actor: &str) -> Result<Option<Self>> {
        let doc = GroupStore::new(home.clone())?.load(group)?;
        let mut actor = actors::find(&doc, actor).context("Actor removed")?.clone();
        if actor.runtime != ActorRuntime::WebModel {
            return Ok(None);
        }
        actors::normalize_web_model_pin(&mut actor)?;
        let binding = binding(home, group, &actor)?;
        Ok(Some(Self {
            home: home.clone(),
            group: group.into(),
            actor,
            binding,
        }))
    }

    pub(crate) fn revalidate(&self) -> Result<()> {
        let doc = GroupStore::new(self.home.clone())?.load(&self.group)?;
        let actor = actors::find(&doc, &self.actor.id).context("Actor removed")?;
        if actors::generation_identity(actor) != actors::generation_identity(&self.actor)
            || actor.runtime != self.actor.runtime
            || actor.web_model_pin_revision != self.actor.web_model_pin_revision
            || actor.web_model_pin != self.actor.web_model_pin
            || binding(&self.home, &self.group, actor)? != self.binding
        {
            bail!("model_pin_authority_changed");
        }
        Ok(())
    }

    fn accepts(&self, selection: WebModelSelection) -> Option<&'static str> {
        match &self.actor.web_model_pin {
            None => Some("observed"),
            Some(pin) if pin.accepts_primary_observation(selection) => Some("primary"),
            Some(pin) if pin.accepts_fallback_observation(selection) => Some("fallback"),
            _ => None,
        }
    }
}

fn binding(
    home: &HomeLayout,
    group: &str,
    actor: &Actor,
) -> Result<Option<(String, String, String)>> {
    let generation = actors::generation_identity(actor);
    Ok(web_model_connectors::load(home)?
        .iter()
        .find_map(|connector| {
            if connector["provider"] != "chatgpt_web" {
                return None;
            }
            let binding =
                web_model_connectors::binding_for_actor(connector, group, &actor.id, &generation)?;
            Some((
                connector["connector_id"].as_str()?.into(),
                binding["revision"].as_str()?.into(),
                binding["url"].as_str()?.into(),
            ))
        }))
}

#[derive(Clone, Default, Deserialize)]
#[serde(default)]
struct PinProbe {
    selection: Option<WebModelSelection>,
    family: String,
    power: Option<u8>,
    reason: String,
    busy: bool,
}

impl BrowserSurfaces {
    pub(crate) async fn model_pin_draft(&self, key: &str) -> Result<String> {
        let page = self
            .sessions
            .lock()
            .await
            .get(key)
            .map(|session| session.page.clone())
            .context("browser closed")?;
        Ok(page.evaluate(r#"(() => {
            const input=document.querySelector('[data-cccc-web-model-composer="cccc-web-model-composer"]');
            return String(input ? ('value' in input ? input.value : input.innerText || input.textContent || '') : '').replace(/\s+/g,' ').trim();
        })()"#).await?.into_value::<String>()?)
    }

    /// A short bounded menu observation. Menu toggles never select a model.
    pub(crate) async fn check_model_pin(
        &self,
        key: &str,
        authority: &ModelPinAuthority,
    ) -> Result<Value> {
        let operation = self.key_operation(key).await;
        let _operation = operation.lock().await;
        let session = self
            .sessions
            .lock()
            .await
            .get(key)
            .cloned()
            .context("browser closed")?;
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            self.observe_model_pin(key, &session, authority),
        )
        .await;
        // Close only the picker, never Escape a provider response.
        let _ = tokio::time::timeout(
            Duration::from_millis(300),
            session.page.evaluate(CLOSE_MENU),
        )
        .await;
        let health = match result {
            Ok(Ok(health)) => health,
            _ => self.pin_health(
                &session,
                authority,
                &PinProbe::default(),
                "picker_unreadable",
                None,
            ),
        };
        self.record_model_pin(key, &session, &health).await;
        Ok(health)
    }

    pub(super) async fn record_model_pin(&self, key: &str, session: &Session, health: &Value) {
        let mut sessions = self.sessions.lock().await;
        if let Some(current) = sessions
            .get_mut(key)
            .filter(|current| same_session(current, session))
        {
            current.metadata["model_pin"] = health.clone();
        }
    }

    pub(super) async fn observe_model_pin(
        &self,
        key: &str,
        session: &Session,
        authority: &ModelPinAuthority,
    ) -> Result<Value> {
        authority.revalidate()?;
        let opened = session
            .page
            .evaluate(OPEN_MENU)
            .await?
            .into_value::<bool>()?;
        if !opened {
            bail!("picker_unreadable");
        }
        let mut previous = None;
        let mut last = PinProbe::default();
        for _ in 0..12 {
            authority.revalidate()?;
            self.require_pin_session(key, session, authority).await?;
            let probe = session
                .page
                .evaluate(format!("({INSPECT_PIN})()"))
                .await?
                .into_value::<PinProbe>()?;
            if probe.busy {
                return Ok(self.pin_health(session, authority, &probe, "provider_busy", None));
            }
            let effective = probe
                .selection
                .and_then(|selection| authority.accepts(selection));
            if effective.is_some() && previous == probe.selection {
                authority.revalidate()?;
                return Ok(self.pin_health(session, authority, &probe, "", effective));
            }
            previous = probe.selection;
            if probe.selection.is_some() && effective.is_none() {
                return Ok(self.pin_health(
                    session,
                    authority,
                    &probe,
                    "model_mismatch_set_by_hand",
                    None,
                ));
            }
            if probe.reason == "forbidden_model" {
                return Ok(self.pin_health(
                    session,
                    authority,
                    &probe,
                    "forbidden_model_set_by_hand",
                    None,
                ));
            }
            last = probe;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(self.pin_health(
            session,
            authority,
            &last,
            "selection_unrecognized_set_by_hand",
            None,
        ))
    }

    pub(super) async fn require_pin_session(
        &self,
        key: &str,
        session: &Session,
        authority: &ModelPinAuthority,
    ) -> Result<()> {
        let sessions = self.sessions.lock().await;
        if !sessions.get(key).is_some_and(|current| {
            same_session(current, session)
                && current.metadata["actor_identity"] == super::actor_identity(&authority.actor)
        }) {
            bail!("model_pin_page_changed");
        }
        Ok(())
    }

    fn pin_health(
        &self,
        session: &Session,
        authority: &ModelPinAuthority,
        probe: &PinProbe,
        reason: &str,
        effective: Option<&str>,
    ) -> Value {
        json!({
            "state":if effective.is_some(){"ready"}else{"held"},
            "reason":reason,"requested":authority.actor.web_model_pin,
            "observed":probe.selection,"observed_family":probe.family,"observed_power":probe.power,
            "effective":if effective.is_some(){json!({"selection":probe.selection,"source":effective})}else{Value::Null},
            "checked_at":cccc_contracts::utc_now(),"policy_revision":authority.actor.web_model_pin_revision,
            "actor_generation":actors::generation_identity(&authority.actor),
            "page_target_id":session.page.target_id(),"surface_started_at":session.started_at,
            "last_drift_correction_at":null,"automatic_cross_family_switches":0
        })
    }
}

fn same_session(left: &Session, right: &Session) -> bool {
    left.page.target_id() == right.page.target_id() && Arc::ptr_eq(&left.owner, &right.owner)
}

const OPEN_MENU: &str = r#"(() => {
    const buttons = Array.from(document.querySelectorAll('button[aria-label="Select ChatGPT model"]'));
    if (buttons.length !== 1) return false;
    const button=buttons[0];
    if (button.disabled || button.getAttribute('aria-disabled')==='true') return false;
    if (button.getAttribute('aria-expanded')!=='true') button.click();
    return true;
})()"#;

pub(super) const CLOSE_MENU: &str = r#"(() => {
    const button=document.querySelector('button[aria-label="Select ChatGPT model"]');
    if (button?.getAttribute('aria-expanded')==='true') button.click();
})()"#;

pub(super) fn guarded_form_script(
    prompt: &str,
    needles: &[String],
    url: &str,
    selection: &Value,
    inspect_submission: &str,
    messages: &str,
    request_submit: &str,
) -> String {
    let payload = json!({"prompt":prompt,"needles":needles,"url":url,"selection":selection});
    let guard = include_str!("chatgpt_model_pin_submit.js");
    format!(
        "({guard})({payload},({inspect_submission}),({messages}),({request_submit}),({INSPECT_PIN}))"
    )
}
