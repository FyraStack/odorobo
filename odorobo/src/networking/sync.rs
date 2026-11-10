use std::collections::HashMap;

use crate::messages::network::{NetworkUpdate, SyncNetwork};
use crate::network::Network;
use kameo::prelude::ActorId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SyncDecision {
    Apply,
    Acknowledge,
}

impl SyncDecision {
    /// Acknowledges when there is nothing left to apply.
    const fn resolve(acknowledge: bool) -> Self {
        if acknowledge {
            Self::Acknowledge
        } else {
            Self::Apply
        }
    }
}

#[derive(PartialEq, Eq, Hash)]
enum Resource {
    Routed(u32),
    Bridged(String),
}

fn resource(network: &Network) -> Resource {
    network
        .vni()
        .map_or_else(|| Resource::Bridged(network.id.clone()), Resource::Routed)
}

struct SeenIntent {
    intent: SyncNetwork,
    applied: bool,
}

struct Owner {
    manager_id: ActorId,
    revision: u64,
    network: Option<Network>,
}

#[derive(Default)]
pub(super) struct NetworkSyncFence {
    seen: HashMap<String, SeenIntent>,
    owners: HashMap<Resource, Owner>,
}

impl NetworkSyncFence {
    pub fn begin(&mut self, intent: &SyncNetwork) -> Result<SyncDecision, &'static str> {
        self.adopt_owner(intent)?;
        let obsolete = self.is_obsolete(&intent.update);
        self.replay(intent, obsolete)
    }

    /// Validates the ownership context and adopts it into the resource fence.
    fn adopt_owner(&mut self, intent: &SyncNetwork) -> Result<(), &'static str> {
        let key = resource(intent.update.network());
        if intent.owners_revision < intent.revision {
            return Err("network ownership revision precedes the update");
        }
        if intent
            .active_owner
            .as_ref()
            .is_some_and(|owner| resource(owner) != key)
        {
            return Err("network ownership describes a different resource");
        }
        let replace_owner = match self.owners.get(&key) {
            Some(owner) if owner.manager_id == intent.manager_id => {
                if owner.revision == intent.owners_revision && owner.network != intent.active_owner
                {
                    return Err("conflicting network ownership has the same revision");
                }
                owner.revision <= intent.owners_revision
            }
            _ => true,
        };
        if replace_owner {
            self.owners.insert(
                key,
                Owner {
                    manager_id: intent.manager_id,
                    revision: intent.owners_revision,
                    network: intent.active_owner.clone(),
                },
            );
        }
        Ok(())
    }

    /// Returns whether current ownership already covers the update, so it needs no mutation.
    fn is_obsolete(&self, update: &NetworkUpdate) -> bool {
        // active owner is already the same as the update
        let active = self.active_owner(update.network());
        match update {
            NetworkUpdate::Ensure(expected) => active != Some(expected),
            NetworkUpdate::Remove(_) => active.is_some(),
        }
    }

    /// Replays the last-seen intent, recording the newest one per network ID.
    fn replay(
        &mut self,
        intent: &SyncNetwork,
        obsolete: bool,
    ) -> Result<SyncDecision, &'static str> {
        let id = &intent.update.network().id;
        if let Some(previous) = self.seen.get(id)
            && previous.intent.manager_id == intent.manager_id
        {
            if previous.intent.revision > intent.revision {
                return Ok(SyncDecision::Acknowledge);
            }
            if previous.intent.revision == intent.revision {
                if !previous.intent.same_update(intent) {
                    return Err("conflicting network updates have the same revision");
                }
                return Ok(SyncDecision::resolve(previous.applied || obsolete));
            }
        }
        self.seen.insert(
            id.clone(),
            SeenIntent {
                intent: intent.clone(),
                applied: obsolete,
            },
        );
        Ok(SyncDecision::resolve(obsolete))
    }

    pub fn active_owner(&self, network: &Network) -> Option<&Network> {
        self.owners
            .get(&resource(network))
            .and_then(|owner| owner.network.as_ref())
    }

    pub fn acknowledge(&mut self, intent: &SyncNetwork) {
        if let Some(previous) = self.seen.get_mut(&intent.update.network().id)
            && previous.intent.same_update(intent)
        {
            previous.applied = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::{NetworkProvider, NetworkState};

    fn ensure(revision: u64) -> SyncNetwork {
        let network = Network {
            id: "sync-test".to_owned(),
            subnet: "10.40.10.0/24".parse().unwrap(),
            gateway: None,
            provider: NetworkProvider::Routed,
            state: Some(NetworkState { vni: 1001 }),
            mtu: None,
            bridge: None,
            dhcp: None,
            upstream: None,
        };
        SyncNetwork {
            manager_id: ActorId::new(1),
            revision,
            update: NetworkUpdate::Ensure(network.clone()),
            owners_revision: revision,
            active_owner: Some(network),
        }
    }

    fn removal(revision: u64) -> SyncNetwork {
        let mut intent = ensure(revision);
        intent.update = NetworkUpdate::Remove(intent.update.network().clone());
        intent.active_owner = None;
        intent
    }

    #[test]
    fn failed_updates_retry_but_lost_acknowledgements_do_not_reapply() {
        let mut fence = NetworkSyncFence::default();
        let intent = ensure(1);
        assert_eq!(fence.begin(&intent), Ok(SyncDecision::Apply));
        assert_eq!(fence.begin(&intent), Ok(SyncDecision::Apply));
        fence.acknowledge(&intent);
        assert_eq!(fence.begin(&intent), Ok(SyncDecision::Acknowledge));
    }

    #[test]
    fn deletion_fences_old_ensures_and_recreation_fences_old_deletions() {
        let mut fence = NetworkSyncFence::default();
        let original = ensure(1);
        let removed = removal(2);
        let replacement = ensure(3);
        assert_eq!(fence.begin(&removed), Ok(SyncDecision::Apply));
        fence.acknowledge(&removed);
        assert_eq!(fence.begin(&original), Ok(SyncDecision::Acknowledge));
        assert_eq!(fence.begin(&replacement), Ok(SyncDecision::Apply));
        fence.acknowledge(&replacement);
        assert_eq!(fence.begin(&removed), Ok(SyncDecision::Acknowledge));
    }

    #[test]
    fn restart_ownership_snapshot_protects_a_reused_vni_before_any_ensure() {
        let mut fence = NetworkSyncFence::default();
        let replacement = ensure(3);
        let mut retired = removal(2);
        retired.owners_revision = 3;
        retired.active_owner = Some(replacement.update.network().clone());
        assert_eq!(fence.begin(&retired), Ok(SyncDecision::Acknowledge));
        assert_eq!(fence.begin(&replacement), Ok(SyncDecision::Apply));
    }

    #[test]
    fn old_removal_cannot_erase_new_ownership_of_the_same_vni() {
        let mut fence = NetworkSyncFence::default();
        let mut replacement = ensure(3);
        if let NetworkUpdate::Ensure(network) = &mut replacement.update {
            network.id = "replacement".to_owned();
        }
        replacement.active_owner = Some(replacement.update.network().clone());
        assert_eq!(fence.begin(&replacement), Ok(SyncDecision::Apply));
        fence.acknowledge(&replacement);
        assert_eq!(fence.begin(&removal(2)), Ok(SyncDecision::Acknowledge));
        assert_eq!(
            fence.active_owner(replacement.update.network()),
            Some(replacement.update.network())
        );
    }

    #[test]
    fn equal_revision_cannot_change_the_payload_and_manager_restart_has_a_new_scope() {
        let mut fence = NetworkSyncFence::default();
        let intent = ensure(1);
        assert_eq!(fence.begin(&intent), Ok(SyncDecision::Apply));
        let mut conflicting = intent.clone();
        conflicting.update = NetworkUpdate::Remove(intent.update.network().clone());
        assert_eq!(
            fence.begin(&conflicting),
            Err("conflicting network updates have the same revision")
        );
        conflicting.manager_id = ActorId::new(2);
        conflicting.active_owner = None;
        assert_eq!(fence.begin(&conflicting), Ok(SyncDecision::Apply));
    }
}
