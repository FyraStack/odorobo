use super::{
    CachedActorKind, CachedVMActor, SchedulerActor, VmLifecycle, VmPlacement, VmTombstoneState,
    scheduling::{
        affinity_delta, evaluate_affinity_rule, evaluate_table_value, has_capacity,
        pending_resources_for_agent,
    },
};

use crate::manifest::{
    AffinityRequirement, AffinityRule, AffinityStrictness, AffinityType, Boot, Compute,
    DesiredState, Metadata, MetadataTable, Operator, VmManifest,
};
use crate::messages::agent::AgentStatus;
use crate::types::ObjectMetadata;
use ahash::{AHashMap, AHashSet};
use bytesize::ByteSize;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use ulid::Ulid;

fn test_manifest(vcpus: u32, memory_bytes: u64) -> VmManifest {
    VmManifest {
        api_version: crate::manifest::MANIFEST_VERSION,
        id: Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("valid ulid"),
        desired: DesiredState {
            metadata: Metadata {
                name: "test".to_owned(),
                ..Default::default()
            },
            compute: Compute {
                vcpus,
                memory_bytes,
                ..Default::default()
            },
            boot: Boot::default(),
            ..Default::default()
        },
        observed: None,
    }
}

fn requirement(operator: Operator, values: &[&str]) -> AffinityRequirement {
    AffinityRequirement {
        key: "tier".to_owned(),
        table: MetadataTable::Label,
        operator,
        values: values.iter().map(|value| (*value).to_owned()).collect(),
    }
}

#[test]
fn expires_unresolved_vm_placeholder_but_retains_vm_intent() {
    let vmid = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("valid ulid");
    let agent_id = super::ActorId::new(1);
    let mut placements: AHashMap<Ulid, Vec<VmPlacement>> = AHashMap::new();
    placements.insert(
        vmid,
        vec![VmPlacement {
            agent_id,
            lifecycle: VmLifecycle::Pending,
            created_at: Instant::now()
                .checked_sub(Duration::from_secs(121))
                .expect("test timestamp should be representable"),
            last_confirmed_at: None,
        }],
    );
    let manifests = AHashMap::from([(vmid, test_manifest(1, 1))]);
    let mut data_cache: AHashMap<Ulid, Vec<CachedVMActor>> = AHashMap::new();

    SchedulerActor::cleanup_unresolved_vm_cache(&mut placements, &mut data_cache);

    assert!(manifests.contains_key(&vmid));
    assert!(placements[&vmid].is_empty());
    assert!(!data_cache.contains_key(&vmid));
}

#[test]
fn reserves_pending_vm_resources_until_agent_status_confirms_them() {
    let vmid = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("valid ulid");
    let agent_id = super::ActorId::new(1);
    let config = test_manifest(4, ByteSize::gib(8).as_u64());
    let mut placements: AHashMap<Ulid, Vec<VmPlacement>> = AHashMap::new();
    placements.insert(
        vmid,
        vec![VmPlacement {
            agent_id,
            lifecycle: VmLifecycle::Pending,
            created_at: Instant::now(),
            last_confirmed_at: None,
        }],
    );

    let manifests = AHashMap::from([(vmid, config)]);
    assert_eq!(
        pending_resources_for_agent(&manifests, &placements, agent_id),
        (4, ByteSize::gib(8).as_u64())
    );
}

#[test]
fn reconciling_source_agent_preserves_destination_migration_placement() {
    let vmid = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("valid ulid");
    let source_agent = super::ActorId::new(1);
    let destination_agent = super::ActorId::new(2);
    let running_placement = |agent_id| VmPlacement {
        agent_id,
        lifecycle: VmLifecycle::Running,
        created_at: Instant::now(),
        last_confirmed_at: Some(Instant::now()),
    };
    let manifests = AHashMap::from([(vmid, test_manifest(1, 1))]);
    let mut placements = AHashMap::from([(vmid, vec![running_placement(source_agent)])]);
    let destination_status = AgentStatus {
        hostname: "destination".to_owned(),
        vcpus: 1,
        ram: ByteSize::b(1),
        used_vcpus: 0,
        used_ram: ByteSize::b(0),
        vms: vec![vmid],
        retained_vms: Vec::new(),
        metadata: ObjectMetadata::default(),
    };
    SchedulerActor::reconcile_agent_placements(
        destination_agent,
        &destination_status,
        &manifests,
        &mut placements,
    );
    assert_eq!(manifests.len(), 1);

    let source_status = AgentStatus {
        hostname: "source".to_owned(),
        vcpus: 1,
        ram: ByteSize::b(1),
        used_vcpus: 0,
        used_ram: ByteSize::b(0),
        vms: Vec::new(),
        retained_vms: Vec::new(),
        metadata: ObjectMetadata::default(),
    };

    SchedulerActor::reconcile_agent_placements(
        source_agent,
        &source_status,
        &manifests,
        &mut placements,
    );

    let remaining = placements
        .get(&vmid)
        .expect("destination placement remains");
    assert_eq!(remaining.len(), 2);
    assert!(remaining.iter().any(|entry| entry.agent_id == source_agent));
    assert!(
        remaining
            .iter()
            .any(|entry| entry.agent_id == destination_agent)
    );
}

#[test]
fn agent_delta_removal_unplaces_running_placement() {
    let vmid = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("valid ulid");
    let agent_id = super::ActorId::new(1);
    let manifests = AHashMap::from([(vmid, test_manifest(1, 1))]);
    let mut placements = AHashMap::from([(
        vmid,
        vec![VmPlacement {
            agent_id,
            lifecycle: VmLifecycle::Running,
            created_at: Instant::now(),
            last_confirmed_at: Some(Instant::now()),
        }],
    )]);

    SchedulerActor::reconcile_agent_delta(agent_id, &[], &[vmid], &manifests, &mut placements);

    assert!(placements[&vmid].is_empty());
}

#[test]
fn agent_delta_removal_retains_pending_placement() {
    let vmid = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("valid ulid");
    let agent_id = super::ActorId::new(1);
    let manifests = AHashMap::from([(vmid, test_manifest(1, 1))]);
    let mut placements = AHashMap::from([(
        vmid,
        vec![VmPlacement {
            agent_id,
            lifecycle: VmLifecycle::Pending,
            created_at: Instant::now(),
            last_confirmed_at: None,
        }],
    )]);

    SchedulerActor::reconcile_agent_delta(agent_id, &[], &[vmid], &manifests, &mut placements);

    assert_eq!(placements[&vmid].len(), 1);
    assert_eq!(placements[&vmid][0].lifecycle, VmLifecycle::Pending);
}

#[test]
fn departed_agent_leaves_empty_placement_for_reconciliation() {
    let vmid = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("valid ulid");
    let agent_id = super::ActorId::new(1);
    let mut placements = AHashMap::from([(
        vmid,
        vec![VmPlacement {
            agent_id,
            lifecycle: VmLifecycle::Running,
            created_at: Instant::now(),
            last_confirmed_at: Some(Instant::now()),
        }],
    )]);

    SchedulerActor::remove_agent_placements(agent_id, &AHashSet::new(), &mut placements);

    assert!(placements.contains_key(&vmid));
    assert!(placements[&vmid].is_empty());
}

#[test]
fn departed_agent_preserves_placement_while_vm_actor_is_reachable() {
    let vmid = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("valid ulid");
    let agent_id = super::ActorId::new(1);
    let mut placements = AHashMap::from([(
        vmid,
        vec![VmPlacement {
            agent_id,
            lifecycle: VmLifecycle::Running,
            created_at: Instant::now(),
            last_confirmed_at: Some(Instant::now()),
        }],
    )]);

    SchedulerActor::remove_agent_placements(agent_id, &AHashSet::from([vmid]), &mut placements);

    assert_eq!(placements[&vmid].len(), 1);
}

#[test]
fn discovered_vm_reconciles_agent_snapshot_received_first() {
    let vmid = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("valid ulid");
    let agent_id = super::ActorId::new(1);
    let manifests = AHashMap::from([(vmid, test_manifest(1, 1))]);
    let agent_vm_index = AHashMap::from([(agent_id, AHashSet::from([vmid]))]);
    let mut placements = AHashMap::new();

    SchedulerActor::reconcile_discovered_vm(vmid, &agent_vm_index, &manifests, &mut placements);

    let placement = &placements[&vmid][0];
    assert_eq!(placement.agent_id, agent_id);
    assert_eq!(placement.lifecycle, VmLifecycle::Running);
}

#[test]
fn pending_placement_survives_full_snapshot_until_pending_timeout() {
    let vmid = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("valid ulid");
    let agent_id = super::ActorId::new(1);
    let mut placements = AHashMap::from([(
        vmid,
        vec![VmPlacement {
            agent_id,
            lifecycle: VmLifecycle::Pending,
            created_at: Instant::now(),
            last_confirmed_at: None,
        }],
    )]);
    let manifests = AHashMap::from([(vmid, test_manifest(1, 1))]);
    let empty_status = AgentStatus {
        hostname: "agent".to_owned(),
        vcpus: 1,
        ram: ByteSize::b(1),
        used_vcpus: 0,
        used_ram: ByteSize::b(0),
        vms: Vec::new(),
        retained_vms: Vec::new(),
        metadata: ObjectMetadata::default(),
    };

    SchedulerActor::reconcile_agent_placements(
        agent_id,
        &empty_status,
        &manifests,
        &mut placements,
    );

    assert_eq!(placements[&vmid][0].lifecycle, VmLifecycle::Pending);
}

#[test]
fn stale_running_placement_is_expired_by_full_snapshot() {
    let vmid = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("valid ulid");
    let agent_id = super::ActorId::new(1);
    let mut placements = AHashMap::from([(
        vmid,
        vec![VmPlacement {
            agent_id,
            lifecycle: VmLifecycle::Running,
            created_at: Instant::now(),
            last_confirmed_at: Some(
                Instant::now()
                    .checked_sub(Duration::from_secs(31))
                    .expect("test timestamp should be representable"),
            ),
        }],
    )]);
    let manifests = AHashMap::from([(vmid, test_manifest(1, 1))]);
    let empty_status = AgentStatus {
        hostname: "agent".to_owned(),
        vcpus: 1,
        ram: ByteSize::b(1),
        used_vcpus: 0,
        used_ram: ByteSize::b(0),
        vms: Vec::new(),
        retained_vms: Vec::new(),
        metadata: ObjectMetadata::default(),
    };

    SchedulerActor::reconcile_agent_placements(
        agent_id,
        &empty_status,
        &manifests,
        &mut placements,
    );

    assert!(!placements.contains_key(&vmid));
}

#[test]
fn vm_cleanup_unplaces_vm_without_another_discovered_actor() {
    let agent_id = super::ActorId::new(1);
    let vm_actor_id = super::ActorId::new(2);
    let vmid = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("valid ulid");
    let mut scheduler = SchedulerActor {
        agent_data_cache: AHashMap::new(),
        agent_keepalive_tasks: AHashMap::new(),
        vm_actorid_ulid_map: AHashMap::from([(vm_actor_id, vmid)]),
        vm_manifests: AHashMap::from([(vmid, test_manifest(1, 1))]),
        vm_effective_manifests: AHashMap::new(),
        vm_tombstones: AHashMap::new(),
        vm_auto_deletes: AHashMap::new(),
        known_agent_refs: AHashMap::new(),
        vm_delete_targets: AHashMap::new(),
        vm_placements: AHashMap::from([(
            vmid,
            vec![VmPlacement {
                agent_id,
                lifecycle: VmLifecycle::Running,
                created_at: Instant::now(),
                last_confirmed_at: Some(Instant::now()),
            }],
        )]),
        vm_data_cache: AHashMap::from([(vmid, vec![CachedVMActor { actor_ref: None }])]),
        vm_keepalive_tasks: AHashMap::new(),
        pending_resources_cache: None,
        agent_vm_index: AHashMap::new(),
        actor_kinds: AHashMap::new(),
        cache_actor_finder: None,
    };

    scheduler.cleanup_vm_actor(vm_actor_id);

    assert!(scheduler.vm_manifests.contains_key(&vmid));
    assert!(scheduler.vm_placements[&vmid].is_empty());
}

#[test]
fn failed_create_rolls_back_state_without_an_actor() {
    let vmid = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("valid ulid");
    let mut manifests = AHashMap::from([(vmid, test_manifest(1, 1))]);
    let mut effective_manifests = AHashMap::new();
    let mut actor_map = AHashMap::new();
    let mut placements = AHashMap::from([(
        vmid,
        vec![VmPlacement {
            agent_id: super::ActorId::new(1),
            lifecycle: VmLifecycle::Pending,
            created_at: Instant::now(),
            last_confirmed_at: None,
        }],
    )]);
    let mut data_cache = AHashMap::from([(vmid, vec![CachedVMActor { actor_ref: None }])]);

    SchedulerActor::rollback_failed_create(
        vmid,
        false,
        None,
        &mut actor_map,
        &mut manifests,
        &mut effective_manifests,
        &mut placements,
        &mut data_cache,
    );

    assert!(!manifests.contains_key(&vmid));
    assert!(!effective_manifests.contains_key(&vmid));
    assert!(!placements.contains_key(&vmid));
    assert!(!data_cache.contains_key(&vmid));
}

#[test]
fn settled_failed_create_cleanup_allows_corrected_recreate() {
    let vmid = Ulid::generate();
    let owner = super::VmDeleteOwner::agent(super::ActorId::new(1));
    let mut scheduler = empty_scheduler();
    scheduler.remember_vm_delete_owner(vmid, owner);

    scheduler.settle_failed_create_cleanup(vmid, owner, true, true);

    assert!(!scheduler.vm_delete_targets.contains_key(&vmid));
    assert_ne!(
        scheduler.vm_tombstone_state(vmid),
        Some(VmTombstoneState::DeletePending)
    );
    assert!(!scheduler.create_is_confirmed(vmid));
}

#[test]
fn uncertain_failed_create_cleanup_retains_owner_and_blocks_recreate() {
    let vmid = Ulid::generate();
    let owner = super::VmDeleteOwner::agent(super::ActorId::new(1));
    let mut scheduler = empty_scheduler();
    scheduler.remember_vm_delete_owner(vmid, owner);

    scheduler.settle_failed_create_cleanup(vmid, owner, true, false);

    assert!(scheduler.vm_delete_targets[&vmid].contains_key(&owner));
    assert_eq!(
        scheduler.vm_tombstone_state(vmid),
        Some(VmTombstoneState::DeletePending)
    );
}

#[test]
fn unknown_create_retry_waits_for_acceptance_or_discovery() {
    let vmid = Ulid::generate();
    let mut scheduler = empty_scheduler();
    scheduler.vm_manifests.insert(vmid, test_manifest(1, 1));
    scheduler.vm_placements.insert(
        vmid,
        vec![VmPlacement {
            agent_id: super::ActorId::new(1),
            lifecycle: VmLifecycle::Pending,
            created_at: Instant::now(),
            last_confirmed_at: None,
        }],
    );
    scheduler
        .vm_data_cache
        .insert(vmid, vec![CachedVMActor { actor_ref: None }]);

    assert!(!scheduler.create_is_confirmed(vmid));

    // a successful agent reply saves the effective config even before status
    // discovery sees the new VM
    scheduler
        .vm_effective_manifests
        .insert(vmid, test_manifest(1, 1));
    assert!(scheduler.create_is_confirmed(vmid));
}

#[test]
fn accepted_create_actor_id_confirms_idempotent_retry() {
    let vmid = Ulid::generate();
    let actor_id = super::ActorId::new(2);
    let mut scheduler = empty_scheduler();
    scheduler.vm_manifests.insert(vmid, test_manifest(1, 1));
    scheduler.vm_actorid_ulid_map.insert(actor_id, vmid);

    assert!(scheduler.create_is_confirmed(vmid));
}

#[test]
fn failed_create_keeps_state_if_actor_exists() {
    let vmid = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("valid ulid");
    let mut manifests = AHashMap::from([(vmid, test_manifest(1, 1))]);
    let mut effective_manifests = AHashMap::new();
    let mut actor_map = AHashMap::new();
    let mut placements = AHashMap::new();
    let mut data_cache = AHashMap::new();

    SchedulerActor::rollback_failed_create(
        vmid,
        true,
        None,
        &mut actor_map,
        &mut manifests,
        &mut effective_manifests,
        &mut placements,
        &mut data_cache,
    );

    assert!(manifests.contains_key(&vmid));
}

#[test]
fn explicit_stop_removes_vm_intent_and_actor_mapping() {
    let vmid = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("valid ulid");
    let vm_actor_id = super::ActorId::new(2);
    let agent_id = super::ActorId::new(1);
    let mut scheduler = SchedulerActor {
        agent_data_cache: AHashMap::new(),
        agent_keepalive_tasks: AHashMap::new(),
        vm_actorid_ulid_map: AHashMap::from([(vm_actor_id, vmid)]),
        vm_manifests: AHashMap::from([(vmid, test_manifest(1, 1))]),
        vm_effective_manifests: AHashMap::new(),
        vm_tombstones: AHashMap::new(),
        vm_auto_deletes: AHashMap::new(),
        known_agent_refs: AHashMap::new(),
        vm_delete_targets: AHashMap::new(),
        vm_placements: AHashMap::from([(vmid, Vec::new())]),
        vm_data_cache: AHashMap::from([(vmid, vec![CachedVMActor { actor_ref: None }])]),
        vm_keepalive_tasks: AHashMap::new(),
        pending_resources_cache: None,
        agent_vm_index: AHashMap::from([(agent_id, AHashSet::from([vmid]))]),
        actor_kinds: AHashMap::new(),
        cache_actor_finder: None,
    };

    scheduler.remove_vm_intent(vmid);

    assert!(scheduler.vm_tombstones.contains_key(&vmid));
    assert!(!scheduler.vm_manifests.contains_key(&vmid));
    assert!(!scheduler.vm_placements.contains_key(&vmid));
    assert!(!scheduler.vm_data_cache.contains_key(&vmid));
    assert!(!scheduler.vm_actorid_ulid_map.contains_key(&vm_actor_id));
    assert!(!scheduler.agent_vm_index[&agent_id].contains(&vmid));
}

#[test]
fn agent_cleanup_does_not_remove_unrelated_vm_state() {
    let agent_id = super::ActorId::new(1);
    let vm_actor_id = super::ActorId::new(2);
    let placement_agent_id = super::ActorId::new(3);
    let vmid = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("valid ulid");
    let mut scheduler = SchedulerActor {
        agent_data_cache: AHashMap::new(),
        agent_keepalive_tasks: AHashMap::new(),
        vm_actorid_ulid_map: AHashMap::from([(vm_actor_id, vmid)]),
        vm_manifests: AHashMap::from([(vmid, test_manifest(1, 1))]),
        vm_effective_manifests: AHashMap::new(),
        vm_tombstones: AHashMap::new(),
        vm_auto_deletes: AHashMap::new(),
        known_agent_refs: AHashMap::new(),
        vm_delete_targets: AHashMap::new(),
        vm_placements: AHashMap::from([(
            vmid,
            vec![VmPlacement {
                agent_id: placement_agent_id,
                lifecycle: VmLifecycle::Running,
                created_at: Instant::now(),
                last_confirmed_at: Some(Instant::now()),
            }],
        )]),
        vm_data_cache: AHashMap::from([(vmid, vec![CachedVMActor { actor_ref: None }])]),
        vm_keepalive_tasks: AHashMap::new(),
        pending_resources_cache: None,
        agent_vm_index: AHashMap::new(),
        actor_kinds: AHashMap::from([(agent_id, CachedActorKind::Agent)]),
        cache_actor_finder: None,
    };

    scheduler.cleanup_agent_actor(agent_id);

    assert!(scheduler.vm_placements.contains_key(&vmid));
    assert!(scheduler.vm_manifests.contains_key(&vmid));
    assert!(scheduler.vm_actorid_ulid_map.contains_key(&vm_actor_id));
    assert!(scheduler.vm_data_cache.contains_key(&vmid));
}

#[test]
fn vm_cleanup_does_not_remove_unrelated_agent_state() {
    let agent_id = super::ActorId::new(1);
    let vm_actor_id = super::ActorId::new(2);
    let vmid = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("valid ulid");
    let mut scheduler = SchedulerActor {
        agent_data_cache: AHashMap::new(),
        agent_keepalive_tasks: AHashMap::new(),
        vm_actorid_ulid_map: AHashMap::from([(vm_actor_id, vmid)]),
        vm_manifests: AHashMap::from([(vmid, test_manifest(1, 1))]),
        vm_effective_manifests: AHashMap::new(),
        vm_tombstones: AHashMap::new(),
        vm_auto_deletes: AHashMap::new(),
        known_agent_refs: AHashMap::new(),
        vm_delete_targets: AHashMap::new(),
        vm_placements: AHashMap::new(),
        vm_data_cache: AHashMap::from([(vmid, vec![CachedVMActor { actor_ref: None }])]),
        vm_keepalive_tasks: AHashMap::new(),
        pending_resources_cache: None,
        agent_vm_index: AHashMap::new(),
        actor_kinds: AHashMap::from([(agent_id, CachedActorKind::Agent)]),
        cache_actor_finder: None,
    };

    scheduler.cleanup_vm_actor(vm_actor_id);

    assert!(scheduler.actor_kinds.contains_key(&agent_id));
    assert!(scheduler.vm_manifests.contains_key(&vmid));
    assert!(scheduler.vm_data_cache.contains_key(&vmid));
}

fn empty_scheduler() -> SchedulerActor {
    SchedulerActor {
        agent_data_cache: AHashMap::new(),
        agent_keepalive_tasks: AHashMap::new(),
        vm_actorid_ulid_map: AHashMap::new(),
        vm_manifests: AHashMap::new(),
        vm_effective_manifests: AHashMap::new(),
        vm_tombstones: AHashMap::new(),
        vm_auto_deletes: AHashMap::new(),
        known_agent_refs: AHashMap::new(),
        vm_delete_targets: AHashMap::new(),
        vm_placements: AHashMap::new(),
        vm_data_cache: AHashMap::new(),
        vm_keepalive_tasks: AHashMap::new(),
        pending_resources_cache: None,
        agent_vm_index: AHashMap::new(),
        actor_kinds: AHashMap::new(),
        cache_actor_finder: None,
    }
}

fn running_placement(agent_id: super::ActorId) -> VmPlacement {
    VmPlacement {
        agent_id,
        lifecycle: VmLifecycle::Running,
        created_at: Instant::now(),
        last_confirmed_at: Some(Instant::now()),
    }
}

#[test]
fn delete_captures_evicted_and_undiscovered_migration_owners_before_intent_removal() {
    let vmid = Ulid::generate();
    let source = super::ActorId::new_with_peer_id(1, libp2p::PeerId::random());
    let destination = super::ActorId::new_with_peer_id(2, libp2p::PeerId::random());
    let undiscovered_vm = super::ActorId::new_with_peer_id(3, libp2p::PeerId::random());
    let mut scheduler = empty_scheduler();
    scheduler.vm_placements.insert(
        vmid,
        vec![running_placement(source), running_placement(destination)],
    );
    scheduler.vm_actorid_ulid_map.insert(undiscovered_vm, vmid);

    scheduler.cleanup_agent_actor(source);
    assert_eq!(scheduler.vm_placements[&vmid].len(), 1);
    scheduler.remove_vm_intent(vmid);
    assert!(!scheduler.vm_placements.contains_key(&vmid));
    assert!(!scheduler.vm_actorid_ulid_map.contains_key(&undiscovered_vm));
    let targets = &scheduler.vm_delete_targets[&vmid];
    assert_eq!(targets.len(), 3);
    for owner in [
        super::VmDeleteOwner::agent(source),
        super::VmDeleteOwner::agent(destination),
        super::VmDeleteOwner::vm(undiscovered_vm),
    ] {
        assert!(targets.contains_key(&owner));
        assert!(targets[&owner].actor_ref.is_none());
    }
    assert!(!scheduler.finish_vm_delete(vmid));
}

#[test]
fn delete_retains_stopped_lease_owners_after_status_and_placement_removal() {
    let vmid = Ulid::generate();
    let agent = super::ActorId::new_with_peer_id(1, libp2p::PeerId::random());
    let owner = super::VmDeleteOwner::agent(agent);
    let mut scheduler = empty_scheduler();
    scheduler.remember_vm_delete_owner(vmid, owner);
    scheduler.vm_manifests.insert(vmid, test_manifest(1, 1));
    scheduler
        .vm_placements
        .insert(vmid, vec![running_placement(agent)]);
    SchedulerActor::reconcile_agent_delta(
        agent,
        &[],
        &[vmid],
        &scheduler.vm_manifests,
        &mut scheduler.vm_placements,
    );
    assert!(scheduler.vm_placements[&vmid].is_empty());

    scheduler.remove_vm_intent(vmid);
    assert!(scheduler.vm_delete_targets[&vmid].contains_key(&owner));
    assert!(!scheduler.finish_vm_delete(vmid));
    scheduler.acknowledge_vm_delete_owner(vmid, owner);
    assert!(scheduler.finish_vm_delete(vmid));
}

#[test]
fn unrelated_success_cannot_complete_delete_and_failures_survive_retries() {
    let vmid = Ulid::generate();
    let source = super::VmDeleteOwner::agent(super::ActorId::new(1));
    let destination = super::VmDeleteOwner::agent(super::ActorId::new(2));
    let unrelated = super::VmDeleteOwner::agent(super::ActorId::new(3));
    let mut scheduler = empty_scheduler();
    scheduler.remember_vm_delete_owner(vmid, source);
    scheduler.remember_vm_delete_owner(vmid, destination);
    scheduler.remove_vm_intent(vmid);
    scheduler.set_vm_tombstone(vmid, VmTombstoneState::DeletePending);
    scheduler.acknowledge_vm_delete_owner(vmid, unrelated);
    assert!(!scheduler.finish_vm_delete(vmid));
    assert_eq!(scheduler.vm_delete_targets[&vmid].len(), 2);

    scheduler.acknowledge_vm_delete_owner(vmid, source);
    // stale discovery and retries shouldn't undo progress we already made
    scheduler.remember_vm_delete_owner(vmid, source);
    scheduler.remove_vm_intent(vmid);
    assert!(scheduler.vm_delete_targets[&vmid][&source].confirmed);
    assert!(!scheduler.vm_delete_targets[&vmid][&destination].confirmed);
    assert!(!scheduler.finish_vm_delete(vmid));
    assert_eq!(
        scheduler.vm_tombstone_state(vmid),
        Some(VmTombstoneState::DeletePending)
    );
    scheduler.acknowledge_vm_delete_owner(vmid, destination);
    assert!(scheduler.finish_vm_delete(vmid));
    assert!(!scheduler.vm_delete_targets.contains_key(&vmid));
    assert_eq!(
        scheduler.vm_tombstone_state(vmid),
        Some(VmTombstoneState::Deleted)
    );
}

#[test]
fn delete_owner_routes_to_replacement_actor_on_same_peer_only() {
    let peer = libp2p::PeerId::random();
    let old_agent = super::ActorId::new_with_peer_id(1, peer);
    let replacement = super::ActorId::new_with_peer_id(2, peer);
    let vm = super::ActorId::new_with_peer_id(3, peer);
    let unrelated = super::ActorId::new_with_peer_id(2, libp2p::PeerId::random());
    let owner = super::VmDeleteOwner::agent(old_agent);
    assert_eq!(owner, super::VmDeleteOwner::vm(vm));
    assert!(owner.matches_agent(replacement));
    assert!(!owner.matches_agent(unrelated));
    // a VM with no peer info shouldn't get matched to some unrelated local agent
    assert!(
        !super::VmDeleteOwner::vm(super::ActorId::new(3)).matches_agent(super::ActorId::new(1))
    );
}

#[test]
fn deleting_one_vm_does_not_include_other_vms_historical_dead_owners() {
    let vmid = Ulid::generate();
    let unrelated_vmid = Ulid::generate();
    let owner = super::VmDeleteOwner::agent(super::ActorId::new(1));
    let unrelated = super::VmDeleteOwner::agent(super::ActorId::new(2));
    let mut scheduler = empty_scheduler();
    scheduler.remember_vm_delete_owner(vmid, owner);
    scheduler.remember_vm_delete_owner(unrelated_vmid, unrelated);
    scheduler.remove_vm_intent(vmid);
    assert_eq!(scheduler.vm_delete_targets[&vmid].len(), 1);
    assert!(!scheduler.vm_delete_targets[&vmid].contains_key(&unrelated));
    scheduler.acknowledge_vm_delete_owner(vmid, owner);
    assert!(scheduler.finish_vm_delete(vmid));
    assert!(scheduler.vm_delete_targets[&unrelated_vmid].contains_key(&unrelated));
}

#[test]
fn late_discovery_adds_new_owner_without_losing_confirmed_progress() {
    let vmid = Ulid::generate();
    let source = super::VmDeleteOwner::agent(super::ActorId::new(1));
    let late_owner = super::VmDeleteOwner::agent(super::ActorId::new(2));
    let mut scheduler = empty_scheduler();
    scheduler.remember_vm_delete_owner(vmid, source);
    scheduler.acknowledge_vm_delete_owner(vmid, source);
    scheduler.remember_vm_delete_owner(vmid, late_owner);
    assert!(scheduler.vm_delete_targets[&vmid][&source].confirmed);
    assert!(!scheduler.finish_vm_delete(vmid));
    scheduler.acknowledge_vm_delete_owner(vmid, late_owner);
    assert!(scheduler.finish_vm_delete(vmid));
}

#[test]
fn agent_replacement_and_vm_actor_on_same_peer_are_one_cleanup_owner() {
    let vmid = Ulid::generate();
    let peer = libp2p::PeerId::random();
    let owner = super::VmDeleteOwner::agent(super::ActorId::new_with_peer_id(1, peer));
    let replacement = super::VmDeleteOwner::agent(super::ActorId::new_with_peer_id(2, peer));
    let vm_owner = super::VmDeleteOwner::vm(super::ActorId::new_with_peer_id(3, peer));
    let mut scheduler = empty_scheduler();
    for candidate in [owner, replacement, vm_owner] {
        scheduler.remember_vm_delete_owner(vmid, candidate);
    }
    assert_eq!(scheduler.vm_delete_targets[&vmid].len(), 1);
    scheduler.acknowledge_vm_delete_owner(vmid, replacement);
    assert!(scheduler.finish_vm_delete(vmid));
}

#[test]
fn delete_without_any_ownership_or_acknowledgement_cannot_finish() {
    let vmid = Ulid::generate();
    let mut scheduler = empty_scheduler();
    assert!(!scheduler.finish_vm_delete(vmid));
    scheduler.vm_delete_targets.insert(vmid, AHashMap::new());
    assert!(!scheduler.finish_vm_delete(vmid));
}

#[test]
fn evaluates_membership_and_missing_keys() {
    let metadata = BTreeMap::from([("tier".to_owned(), "frontend".to_owned())]);
    assert!(evaluate_table_value(
        metadata.get("tier"),
        &requirement(Operator::In, &["frontend", "api"])
    ));
    assert!(!evaluate_table_value(
        metadata.get("tier"),
        &requirement(Operator::In, &["backend"])
    ));
    assert!(!evaluate_table_value(
        None,
        &requirement(Operator::In, &["frontend"])
    ));
    assert!(evaluate_table_value(
        None,
        &requirement(Operator::NotIn, &["frontend"])
    ));
}

#[test]
fn evaluates_not_in_and_numeric_comparisons() {
    let metadata = BTreeMap::from([("tier".to_owned(), "4".to_owned())]);
    assert!(evaluate_table_value(
        metadata.get("tier"),
        &requirement(Operator::NotIn, &["5"])
    ));
    assert!(evaluate_table_value(
        metadata.get("tier"),
        &requirement(Operator::Lt, &["5"])
    ));
    assert!(evaluate_table_value(
        metadata.get("tier"),
        &requirement(Operator::Gt, &["3"])
    ));
    assert!(!evaluate_table_value(
        metadata.get("tier"),
        &requirement(Operator::Lt, &["4", "5"])
    ));
    assert!(!evaluate_table_value(
        metadata.get("tier"),
        &requirement(Operator::Gt, &["not-a-number"])
    ));
}

#[test]
fn evaluates_inverse_and_empty_requirements() {
    let metadata = ObjectMetadata {
        labels: BTreeMap::from([("tier".to_owned(), "frontend".to_owned())]),
        annotations: BTreeMap::new(),
    };
    let rule = AffinityRule {
        strictness: AffinityStrictness::Required,
        affinity_type: AffinityType::Agent,
        inverse: true,
        requirements: vec![requirement(Operator::In, &["frontend"])],
    };
    assert!(!evaluate_affinity_rule(
        &[(&metadata.labels, &metadata.annotations)],
        &rule,
    ));

    let non_matching_rule = AffinityRule {
        strictness: AffinityStrictness::Required,
        affinity_type: AffinityType::Agent,
        inverse: true,
        requirements: vec![requirement(Operator::In, &["backend"])],
    };
    assert!(evaluate_affinity_rule(
        &[(&metadata.labels, &metadata.annotations)],
        &non_matching_rule,
    ));

    let empty_rule = AffinityRule {
        strictness: AffinityStrictness::Required,
        affinity_type: AffinityType::Agent,
        inverse: false,
        requirements: Vec::new(),
    };
    assert!(!evaluate_affinity_rule(&[], &empty_rule));
}

#[test]
fn evaluates_required_preferred_and_capacity_rules() {
    assert_eq!(affinity_delta(&AffinityStrictness::Required, true), Some(0));
    assert_eq!(affinity_delta(&AffinityStrictness::Required, false), None);
    assert_eq!(
        affinity_delta(&AffinityStrictness::Preferred { weight: 7 }, true),
        Some(7)
    );
    assert_eq!(
        affinity_delta(&AffinityStrictness::Preferred { weight: 7 }, false),
        Some(0)
    );
    assert!(has_capacity(8, 2, 2, 16, 4, 4));
    assert!(has_capacity(8, 6, 2, 16, 4, 4));
    assert!(has_capacity(8, 2, 2, 16, 12, 4));
    assert!(!has_capacity(8, 7, 2, 16, 4, 4));
    assert!(!has_capacity(8, 2, 2, 16, 13, 4));
}

fn ago(duration: Duration) -> Instant {
    Instant::now()
        .checked_sub(duration)
        .expect("test timestamp should be representable")
}

fn status_with(vms: Vec<Ulid>, retained_vms: Vec<Ulid>) -> AgentStatus {
    AgentStatus {
        hostname: "agent".to_owned(),
        vcpus: 1,
        ram: ByteSize::b(1),
        used_vcpus: 0,
        used_ram: ByteSize::b(0),
        vms,
        retained_vms,
        metadata: ObjectMetadata::default(),
    }
}

#[test]
fn any_ack_wins_and_explicit_agent_errors_beat_unreachable_routes() {
    use super::deletion::{DeleteAttempt, DeleteOwnerOutcome, delete_owner_outcome};

    assert_eq!(
        delete_owner_outcome(vec![
            DeleteAttempt::Unreachable("stale route".to_owned()),
            DeleteAttempt::Acked,
        ]),
        DeleteOwnerOutcome::Acked
    );
    // a live replacement saying no matters more than a dead old route
    assert_eq!(
        delete_owner_outcome(vec![
            DeleteAttempt::Unreachable("stale route".to_owned()),
            DeleteAttempt::AgentError("teardown failed".to_owned()),
        ]),
        DeleteOwnerOutcome::Rejected(vec!["teardown failed".to_owned()])
    );
    assert_eq!(
        delete_owner_outcome(vec![DeleteAttempt::Unreachable("timeout".to_owned())]),
        DeleteOwnerOutcome::Unreachable(vec!["timeout".to_owned()])
    );
    assert_eq!(
        delete_owner_outcome(Vec::new()),
        DeleteOwnerOutcome::Unreachable(Vec::new())
    );
}

#[test]
fn force_drops_unreachable_owners_but_not_explicit_agent_errors() {
    use super::deletion::{DeleteOwnerOutcome, plan_delete_settlement};

    let acked = super::VmDeleteOwner::agent(super::ActorId::new(1));
    let no_route = super::VmDeleteOwner::agent(super::ActorId::new(2));
    let transport = super::VmDeleteOwner::agent(super::ActorId::new(3));
    let rejected = super::VmDeleteOwner::agent(super::ActorId::new(4));
    let outcomes = || {
        vec![
            (acked, DeleteOwnerOutcome::Acked),
            (no_route, DeleteOwnerOutcome::Unreachable(Vec::new())),
            (
                transport,
                DeleteOwnerOutcome::Unreachable(vec!["timeout".to_owned()]),
            ),
            (
                rejected,
                DeleteOwnerOutcome::Rejected(vec!["teardown failed".to_owned()]),
            ),
        ]
    };

    let forced = plan_delete_settlement(outcomes(), true);
    assert_eq!(forced.acked, vec![acked]);
    assert_eq!(
        forced
            .dropped
            .iter()
            .map(|(owner, _)| *owner)
            .collect::<Vec<_>>(),
        vec![no_route, transport]
    );
    assert_eq!(forced.failures.len(), 1);
    assert!(forced.failures[0].contains("teardown failed"));

    let unforced = plan_delete_settlement(outcomes(), false);
    assert_eq!(unforced.acked, vec![acked]);
    assert_eq!(unforced.dropped, Vec::new());
    assert_eq!(unforced.failures.len(), 3);
    assert!(
        unforced
            .failures
            .iter()
            .any(|failure| failure.contains("missing agent for known VM owner"))
    );
}

#[test]
fn forced_delete_finishes_once_unreachable_owners_are_dropped() {
    use super::deletion::{DeleteOwnerOutcome, plan_delete_settlement};

    let vmid = Ulid::generate();
    let alive = super::VmDeleteOwner::agent(super::ActorId::new(1));
    let dead = super::VmDeleteOwner::agent(super::ActorId::new(2));
    let mut scheduler = empty_scheduler();
    scheduler.remember_vm_delete_owner(vmid, alive);
    scheduler.remember_vm_delete_owner(vmid, dead);
    scheduler.remove_vm_intent(vmid);
    scheduler.set_vm_tombstone(vmid, VmTombstoneState::DeletePending);
    let outcomes = || {
        vec![
            (alive, DeleteOwnerOutcome::Acked),
            (dead, DeleteOwnerOutcome::Unreachable(Vec::new())),
        ]
    };

    // without force the dead node keeps the delete pending, but the ack sticks
    let settlement = plan_delete_settlement(outcomes(), false);
    assert!(
        scheduler
            .apply_delete_settlement(vmid, settlement, false)
            .is_err()
    );
    assert!(scheduler.vm_delete_targets[&vmid][&alive].confirmed);
    assert!(scheduler.vm_delete_targets[&vmid].contains_key(&dead));
    assert_eq!(
        scheduler.vm_tombstone_state(vmid),
        Some(VmTombstoneState::DeletePending)
    );

    let settlement = plan_delete_settlement(outcomes(), true);
    assert_eq!(
        scheduler.apply_delete_settlement(vmid, settlement, true),
        Ok(())
    );
    assert!(!scheduler.vm_delete_targets.contains_key(&vmid));
    assert_eq!(
        scheduler.vm_tombstone_state(vmid),
        Some(VmTombstoneState::Deleted)
    );
}

#[test]
fn forced_delete_still_fails_when_a_live_agent_rejects_it() {
    use super::deletion::{DeleteOwnerOutcome, plan_delete_settlement};

    let vmid = Ulid::generate();
    let dead = super::VmDeleteOwner::agent(super::ActorId::new(1));
    let rejecting = super::VmDeleteOwner::agent(super::ActorId::new(2));
    let mut scheduler = empty_scheduler();
    scheduler.remember_vm_delete_owner(vmid, dead);
    scheduler.remember_vm_delete_owner(vmid, rejecting);
    scheduler.remove_vm_intent(vmid);
    scheduler.set_vm_tombstone(vmid, VmTombstoneState::DeletePending);

    let settlement = plan_delete_settlement(
        vec![
            (dead, DeleteOwnerOutcome::Unreachable(Vec::new())),
            (
                rejecting,
                DeleteOwnerOutcome::Rejected(vec!["teardown failed".to_owned()]),
            ),
        ],
        true,
    );
    let error = scheduler
        .apply_delete_settlement(vmid, settlement, true)
        .expect_err("a live rejection fails even a forced delete");
    assert!(error.contains("teardown failed"));
    // the dead owner is gone, but the rejecting one still blocks recreate
    assert!(!scheduler.vm_delete_targets[&vmid].contains_key(&dead));
    assert!(!scheduler.vm_delete_targets[&vmid][&rejecting].confirmed);
    assert_eq!(
        scheduler.vm_tombstone_state(vmid),
        Some(VmTombstoneState::DeletePending)
    );
}

#[test]
fn forced_delete_without_any_owners_finishes_but_unforced_does_not() {
    use super::deletion::DeleteSettlement;

    let vmid = Ulid::generate();
    let mut scheduler = empty_scheduler();
    scheduler.remove_vm_intent(vmid);
    scheduler.set_vm_tombstone(vmid, VmTombstoneState::DeletePending);

    assert!(
        scheduler
            .apply_delete_settlement(vmid, DeleteSettlement::default(), false)
            .is_err()
    );
    assert_eq!(
        scheduler.vm_tombstone_state(vmid),
        Some(VmTombstoneState::DeletePending)
    );
    assert_eq!(
        scheduler.apply_delete_settlement(vmid, DeleteSettlement::default(), true),
        Ok(())
    );
    assert_eq!(
        scheduler.vm_tombstone_state(vmid),
        Some(VmTombstoneState::Deleted)
    );
}

#[test]
fn shutdown_tombstones_as_stopped_and_delete_goes_pending_then_deleted() {
    let vmid = Ulid::generate();
    let owner = super::VmDeleteOwner::agent(super::ActorId::new(1));
    let mut scheduler = empty_scheduler();
    scheduler.vm_manifests.insert(vmid, test_manifest(1, 1));
    scheduler.remember_vm_delete_owner(vmid, owner);

    // shutdown only removes intent
    scheduler.remove_vm_intent(vmid);
    assert_eq!(
        scheduler.vm_tombstone_state(vmid),
        Some(VmTombstoneState::Stopped)
    );
    assert!(scheduler.vm_is_stopped_or_deleted(vmid));

    // delete does the same, then marks cleanup pending until owners ack
    scheduler.remove_vm_intent(vmid);
    scheduler.set_vm_tombstone(vmid, VmTombstoneState::DeletePending);
    assert!(!scheduler.vm_is_stopped_or_deleted(vmid));
    scheduler.acknowledge_vm_delete_owner(vmid, owner);
    assert!(scheduler.finish_vm_delete(vmid));
    assert_eq!(
        scheduler.vm_tombstone_state(vmid),
        Some(VmTombstoneState::Deleted)
    );

    // shutting down again doesn't downgrade a finished delete
    scheduler.remove_vm_intent(vmid);
    assert_eq!(
        scheduler.vm_tombstone_state(vmid),
        Some(VmTombstoneState::Deleted)
    );
}

#[test]
fn auto_delete_only_fires_for_deleted_vms() {
    let agent = super::ActorId::new(1);
    let stopped = Ulid::generate();
    let pending = Ulid::generate();
    let deleted = Ulid::generate();
    let live = Ulid::generate();
    let mut scheduler = empty_scheduler();
    scheduler.set_vm_tombstone(stopped, VmTombstoneState::Stopped);
    scheduler.set_vm_tombstone(pending, VmTombstoneState::DeletePending);
    scheduler.set_vm_tombstone(deleted, VmTombstoneState::Deleted);

    let picked =
        scheduler.take_auto_delete_vmids(agent, [stopped, pending, deleted, live], Instant::now());
    assert_eq!(picked, vec![deleted]);
    assert!(scheduler.auto_delete_in_flight(deleted));
    assert!(!scheduler.auto_delete_in_flight(stopped));
}

#[test]
fn auto_delete_is_rate_limited_per_vm_and_agent() {
    use super::deletion::AUTO_DELETE_RETRY_INTERVAL;

    let agent = super::ActorId::new(1);
    let other_agent = super::ActorId::new(2);
    let vmid = Ulid::generate();
    let mut scheduler = empty_scheduler();
    scheduler.set_vm_tombstone(vmid, VmTombstoneState::Deleted);
    let now = Instant::now();

    assert_eq!(
        scheduler.take_auto_delete_vmids(agent, [vmid], now),
        vec![vmid]
    );
    // still in flight, so the next status poll doesn't send another
    assert_eq!(
        scheduler.take_auto_delete_vmids(agent, [vmid], now),
        Vec::new()
    );
    // a different agent holding the same VM gets its own
    assert_eq!(
        scheduler.take_auto_delete_vmids(other_agent, [vmid], now),
        vec![vmid]
    );

    scheduler.settle_auto_delete(vmid, agent, Some("busy".to_owned()), now);
    assert_eq!(
        scheduler.take_auto_delete_vmids(agent, [vmid], now),
        Vec::new()
    );
    let later = now
        .checked_add(AUTO_DELETE_RETRY_INTERVAL)
        .expect("test timestamp should be representable");
    assert_eq!(
        scheduler.take_auto_delete_vmids(agent, [vmid], later),
        vec![vmid]
    );

    scheduler.settle_auto_delete(vmid, agent, None, later);
    scheduler.settle_auto_delete(vmid, other_agent, None, later);
    assert!(!scheduler.auto_delete_in_flight(vmid));
    assert_eq!(scheduler.vm_auto_deletes.len(), 0);
}

#[test]
fn retained_vms_rebuild_delete_owners_after_scheduler_restart() {
    use super::deletion::status_update_held_vmids;
    use crate::messages::agent::AgentStatusUpdate;

    let agent = super::ActorId::new_with_peer_id(1, libp2p::PeerId::random());
    let running = Ulid::generate();
    let stopped_lease = Ulid::generate();
    let full = AgentStatusUpdate::Full {
        revision: 1,
        status: status_with(vec![running], vec![stopped_lease]),
    };
    let held = status_update_held_vmids(&full, None).expect("first full snapshot is accepted");

    // a fresh scheduler has no tombstones or owners for either VM
    let mut scheduler = empty_scheduler();
    scheduler.record_status_delete_owners(agent, held);
    let owner = super::VmDeleteOwner::agent(agent);
    assert!(scheduler.vm_delete_targets[&running].contains_key(&owner));
    assert!(scheduler.vm_delete_targets[&stopped_lease].contains_key(&owner));

    // so a later delete has to wait for this agent instead of acking without it
    scheduler.remove_vm_intent(stopped_lease);
    scheduler.set_vm_tombstone(stopped_lease, VmTombstoneState::DeletePending);
    assert!(!scheduler.finish_vm_delete(stopped_lease));
}

#[test]
fn status_deltas_carry_retained_vms_even_without_a_revision_bump() {
    use super::deletion::status_update_held_vmids;
    use crate::messages::agent::AgentStatusUpdate;

    let added = Ulid::generate();
    let retained = Ulid::generate();
    let delta = |revision, added: Vec<Ulid>| AgentStatusUpdate::Delta {
        revision,
        added,
        removed: Vec::new(),
        retained_vms: vec![retained],
        used_vcpus: 0,
        used_ram: ByteSize::b(0),
    };

    assert_eq!(
        status_update_held_vmids(&delta(3, vec![added]), Some(2)),
        Some(vec![added, retained])
    );
    // same revision, nothing added, still refreshes retained state
    assert_eq!(
        status_update_held_vmids(&delta(2, Vec::new()), Some(2)),
        Some(vec![retained])
    );
    // stale ones and deltas before any full snapshot are ignored
    assert_eq!(
        status_update_held_vmids(&delta(1, Vec::new()), Some(2)),
        None
    );
    assert_eq!(status_update_held_vmids(&delta(3, Vec::new()), None), None);
    let stale_full = AgentStatusUpdate::Full {
        revision: 2,
        status: status_with(vec![added], vec![retained]),
    };
    assert_eq!(status_update_held_vmids(&stale_full, Some(2)), None);
}

#[test]
fn status_does_not_re_add_owners_for_finished_deletes_or_untracked_stops() {
    let agent = super::ActorId::new(1);
    let owner = super::VmDeleteOwner::agent(agent);
    let deleted = Ulid::generate();
    let stopped_tracked = Ulid::generate();
    let pending = Ulid::generate();
    let mut scheduler = empty_scheduler();
    scheduler.set_vm_tombstone(deleted, VmTombstoneState::Deleted);
    scheduler.set_vm_tombstone(pending, VmTombstoneState::DeletePending);
    scheduler.remember_vm_delete_owner(
        stopped_tracked,
        super::VmDeleteOwner::agent(super::ActorId::new(9)),
    );
    scheduler.set_vm_tombstone(stopped_tracked, VmTombstoneState::Stopped);

    scheduler.record_status_delete_owners(agent, [deleted, stopped_tracked, pending]);

    // finished deletes get cleaned up in the background instead
    assert!(!scheduler.vm_delete_targets.contains_key(&deleted));
    assert!(scheduler.vm_delete_targets[&stopped_tracked].contains_key(&owner));
    assert!(scheduler.vm_delete_targets[&pending].contains_key(&owner));
}

#[test]
fn prunes_old_stopped_and_deleted_tombstones_but_never_pending_or_tracked_ones() {
    use super::deletion::{
        AUTO_DELETE_RETRY_INTERVAL, DELETED_TOMBSTONE_TTL, STOPPED_TOMBSTONE_TTL,
    };
    use super::{AutoDeleteState, VmTombstone};

    let tombstone = |state, since| VmTombstone { state, since };
    let old_stopped = Ulid::generate();
    let fresh_stopped = Ulid::generate();
    let old_deleted = Ulid::generate();
    let fresh_deleted = Ulid::generate();
    let ancient_pending = Ulid::generate();
    let old_stopped_with_owner = Ulid::generate();
    let mut scheduler = empty_scheduler();
    let past_stopped_ttl = ago(STOPPED_TOMBSTONE_TTL.saturating_add(Duration::from_secs(1)));
    let past_deleted_ttl = ago(DELETED_TOMBSTONE_TTL.saturating_add(Duration::from_secs(1)));
    scheduler.vm_tombstones.extend([
        (
            old_stopped,
            tombstone(VmTombstoneState::Stopped, past_stopped_ttl),
        ),
        (
            fresh_stopped,
            tombstone(VmTombstoneState::Stopped, Instant::now()),
        ),
        (
            old_deleted,
            tombstone(VmTombstoneState::Deleted, past_deleted_ttl),
        ),
        // older than the stopped TTL, but deleted ones live longer
        (
            fresh_deleted,
            tombstone(VmTombstoneState::Deleted, past_stopped_ttl),
        ),
        (
            ancient_pending,
            tombstone(VmTombstoneState::DeletePending, past_deleted_ttl),
        ),
        (
            old_stopped_with_owner,
            tombstone(VmTombstoneState::Stopped, past_deleted_ttl),
        ),
    ]);
    scheduler.remember_vm_delete_owner(
        old_stopped_with_owner,
        super::VmDeleteOwner::agent(super::ActorId::new(1)),
    );
    let agent = super::ActorId::new(2);
    scheduler
        .vm_auto_deletes
        .insert((old_deleted, agent), AutoDeleteState::InFlight);
    scheduler.vm_auto_deletes.insert(
        (fresh_deleted, agent),
        AutoDeleteState::Failed(ago(AUTO_DELETE_RETRY_INTERVAL)),
    );

    scheduler.prune_vm_tombstones(Instant::now());

    assert!(!scheduler.vm_tombstones.contains_key(&old_stopped));
    assert!(scheduler.vm_tombstones.contains_key(&fresh_stopped));
    assert!(!scheduler.vm_tombstones.contains_key(&old_deleted));
    assert!(scheduler.vm_tombstones.contains_key(&fresh_deleted));
    assert!(scheduler.vm_tombstones.contains_key(&ancient_pending));
    assert!(
        scheduler
            .vm_tombstones
            .contains_key(&old_stopped_with_owner)
    );
    // in-flight deletes stay until they report back, expired failures go
    assert!(
        scheduler
            .vm_auto_deletes
            .contains_key(&(old_deleted, agent))
    );
    assert!(
        !scheduler
            .vm_auto_deletes
            .contains_key(&(fresh_deleted, agent))
    );
}

#[test]
fn delete_of_an_unknown_vm_ends_up_prunable() {
    use super::deletion::DELETED_TOMBSTONE_TTL;

    let vmid = Ulid::generate();
    let agent = super::VmDeleteOwner::agent(super::ActorId::new(1));
    let mut scheduler = empty_scheduler();
    // the handler falls back to asking every reachable agent, which all ack
    scheduler.remove_vm_intent(vmid);
    scheduler.set_vm_tombstone(vmid, VmTombstoneState::DeletePending);
    scheduler.remember_vm_delete_owner(vmid, agent);
    scheduler.acknowledge_vm_delete_owner(vmid, agent);
    assert!(scheduler.finish_vm_delete(vmid));

    scheduler.prune_vm_tombstones(Instant::now());
    assert!(scheduler.vm_tombstones.contains_key(&vmid));
    scheduler.prune_vm_tombstones(
        Instant::now()
            .checked_add(DELETED_TOMBSTONE_TTL)
            .expect("test timestamp should be representable"),
    );
    assert!(!scheduler.vm_tombstones.contains_key(&vmid));
    assert!(!scheduler.vm_delete_targets.contains_key(&vmid));
}
