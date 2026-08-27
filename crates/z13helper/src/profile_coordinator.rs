//! Main-thread profile identity and asynchronous-operation coordination.
//!
//! This module deliberately contains no GTK or daemon types.  `AppState` owns
//! one instance on the GTK thread and uses its tokens to decide whether a
//! worker completion still describes the user's current intent.

use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileOperationToken {
    pub id: String,
    pub revision: u64,
    pub operation: u64,
    pub editing_epoch: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyToken {
    pub id: String,
    pub revision: u64,
    pub on_battery: bool,
    pub serial: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplyRequest {
    Started(ApplyToken),
    Queued,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyCompletion {
    pub current: bool,
    pub next: Option<ApplyToken>,
}

/// Runtime-only GUI state.  None of this is serialized into config v1.
#[derive(Debug)]
pub struct ProfileCoordinator {
    active_id: String,
    editing_id: String,
    editing_epoch: u64,
    config_revision: u64,
    profile_revisions: HashMap<String, u64>,
    operation_serials: HashMap<String, u64>,
    next_serial: u64,
    applying: Option<ApplyToken>,
    pending_apply: Option<ApplyToken>,
}

impl ProfileCoordinator {
    pub fn new(active_id: String, profile_ids: impl IntoIterator<Item = String>) -> Self {
        let profile_ids: HashSet<String> = profile_ids.into_iter().collect();
        let editing_id = if profile_ids.contains(&active_id) {
            active_id.clone()
        } else {
            profile_ids
                .iter()
                .next()
                .cloned()
                .unwrap_or(active_id.clone())
        };
        let profile_revisions = profile_ids.into_iter().map(|id| (id, 0)).collect();
        Self {
            active_id,
            editing_id,
            editing_epoch: 0,
            config_revision: 0,
            profile_revisions,
            operation_serials: HashMap::new(),
            next_serial: 0,
            applying: None,
            pending_apply: None,
        }
    }

    pub fn active_id(&self) -> &str {
        &self.active_id
    }

    pub fn editing_id(&self) -> &str {
        &self.editing_id
    }

    pub fn activate(&mut self, id: &str) -> bool {
        if !self.profile_revisions.contains_key(id) || self.active_id == id {
            return false;
        }
        self.active_id = id.into();
        self.config_revision = self.config_revision.wrapping_add(1);
        true
    }

    pub fn select_editing(&mut self, id: &str) -> bool {
        if !self.profile_revisions.contains_key(id) || self.editing_id == id {
            return false;
        }
        self.editing_id = id.into();
        self.editing_epoch = self.editing_epoch.wrapping_add(1);
        self.config_revision = self.config_revision.wrapping_add(1);
        true
    }

    pub fn mark_config_changed(&mut self) {
        self.config_revision = self.config_revision.wrapping_add(1);
    }

    pub fn mark_profile_changed(&mut self, id: &str) -> bool {
        let Some(revision) = self.profile_revisions.get_mut(id) else {
            return false;
        };
        *revision = revision.wrapping_add(1);
        self.config_revision = self.config_revision.wrapping_add(1);
        true
    }

    pub fn add_profile(&mut self, id: &str) {
        self.profile_revisions.entry(id.into()).or_insert(0);
        self.config_revision = self.config_revision.wrapping_add(1);
    }

    pub fn remove_profile(
        &mut self,
        id: &str,
        fallback_active: &str,
        fallback_editing: &str,
    ) -> bool {
        if self.profile_revisions.remove(id).is_none() {
            return false;
        }
        self.operation_serials.remove(id);
        if self.active_id == id {
            self.active_id = fallback_active.into();
        }
        if self.editing_id == id {
            self.editing_id = fallback_editing.into();
            self.editing_epoch = self.editing_epoch.wrapping_add(1);
        }
        self.config_revision = self.config_revision.wrapping_add(1);
        true
    }

    pub fn begin_operation(&mut self, id: &str) -> Option<ProfileOperationToken> {
        let revision = self.profile_revisions.get(id).copied()?;
        let operation = self
            .operation_serials
            .entry(id.into())
            .and_modify(|serial| *serial = serial.wrapping_add(1))
            .or_insert(0);
        Some(ProfileOperationToken {
            id: id.into(),
            revision,
            operation: *operation,
            editing_epoch: self.editing_epoch,
        })
    }

    pub fn is_current_operation(&self, token: &ProfileOperationToken) -> bool {
        self.profile_revisions.get(&token.id) == Some(&token.revision)
            && self.operation_serials.get(&token.id) == Some(&token.operation)
    }

    pub fn is_current_editor(&self, token: &ProfileOperationToken) -> bool {
        self.is_current_operation(token)
            && self.editing_id == token.id
            && self.editing_epoch == token.editing_epoch
    }

    fn next_serial(&mut self) -> u64 {
        self.next_serial = self.next_serial.wrapping_add(1);
        self.next_serial
    }

    fn new_apply_token(&mut self, on_battery: bool) -> ApplyToken {
        ApplyToken {
            id: self.active_id.clone(),
            revision: self
                .profile_revisions
                .get(&self.active_id)
                .copied()
                .unwrap_or(0),
            on_battery,
            serial: self.next_serial(),
        }
    }

    pub fn request_apply(&mut self, on_battery: bool) -> ApplyRequest {
        let token = self.new_apply_token(on_battery);
        if self.applying.is_some() {
            self.pending_apply = Some(token);
            ApplyRequest::Queued
        } else {
            self.applying = Some(token.clone());
            ApplyRequest::Started(token)
        }
    }

    pub fn is_current_apply(&self, token: &ApplyToken, on_battery: bool) -> bool {
        self.active_id == token.id
            && self.profile_revisions.get(&token.id) == Some(&token.revision)
            && token.on_battery == on_battery
    }

    /// Complete a started apply.  A started operation is never cancelled.  If
    /// a newer request exists, return it for immediate worker scheduling; if
    /// the completion was stale, synthesize a request for the newest active
    /// profile so hardware converges to current UI intent.
    pub fn complete_apply(&mut self, token: &ApplyToken, on_battery: bool) -> ApplyCompletion {
        if self.applying.as_ref() != Some(token) {
            return ApplyCompletion {
                current: false,
                next: None,
            };
        }
        self.applying = None;
        let current = self.is_current_apply(token, on_battery);
        let pending = self.pending_apply.take();
        let had_pending = pending.is_some();
        let next = pending
            .filter(|pending| self.is_current_apply(pending, on_battery))
            .or_else(|| (had_pending || !current).then(|| self.new_apply_token(on_battery)));
        if let Some(next) = next.as_ref() {
            self.applying = Some(next.clone());
        }
        ApplyCompletion { current, next }
    }
}

#[cfg(test)]
mod tests {
    use super::{ApplyRequest, ProfileCoordinator};

    fn coordinator() -> ProfileCoordinator {
        ProfileCoordinator::new("a".into(), ["a".into(), "b".into(), "custom".into()])
    }

    #[test]
    fn editing_a_survives_activation_of_b() {
        let mut coordinator = coordinator();
        let restore = coordinator.begin_operation("a").unwrap();

        assert!(coordinator.activate("b"));
        assert_eq!(coordinator.active_id(), "b");
        assert_eq!(coordinator.editing_id(), "a");
        assert!(coordinator.is_current_operation(&restore));
    }

    #[test]
    fn stale_completion_is_rejected_after_profile_edit() {
        let mut coordinator = coordinator();
        let restore = coordinator.begin_operation("a").unwrap();
        assert!(coordinator.mark_profile_changed("a"));
        assert!(!coordinator.is_current_operation(&restore));
    }

    #[test]
    fn rename_and_remove_target_editing_profile() {
        let mut coordinator = coordinator();
        assert!(coordinator.select_editing("custom"));
        assert_eq!(coordinator.editing_id(), "custom");
        assert!(coordinator.remove_profile("custom", "a", "a"));
        assert_eq!(coordinator.active_id(), "a");
        assert_eq!(coordinator.editing_id(), "a");
    }

    #[test]
    fn newest_apply_wins_while_started_work_finishes() {
        let mut coordinator = coordinator();
        let ApplyRequest::Started(first) = coordinator.request_apply(false) else {
            panic!("first apply must start")
        };
        assert!(coordinator.activate("b"));
        assert!(matches!(
            coordinator.request_apply(false),
            ApplyRequest::Queued
        ));
        let completion = coordinator.complete_apply(&first, false);
        let next = completion.next.expect("newest apply should be scheduled");
        assert_eq!(next.id, "b");
    }
}
