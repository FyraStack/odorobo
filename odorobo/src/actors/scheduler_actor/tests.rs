use super::{
    CachedActorKind, CachedVMActor, SchedulerActor, VmLifecycle, VmPlacement,
    scheduling::{
        affinity_delta, evaluate_affinity_rule, evaluate_table_value, has_capacity,
        pending_resources_for_agent, pending_resources_with_status_for_agent,
    },
};

use crate::manifest::{
    AffinityRequirement, AffinityRule, AffinityStrictness, AffinityType, Boot, Compute,
    DesiredState, Metadata, MetadataTable, Operator, VmManifest,
};
use crate::messages::agent::{AgentStatus, AgentStatusUpdate, apply_status_update};
use crate::types::ObjectMetadata;
use ahash::{AHashMap, AHashSet};
use bytesize::ByteSize;
use odorobo::cluster_state::{MemoryStateStore, StateStore};
use std::time::{Duration, Instant};
use std::{collections::BTreeMap, sync::Arc};
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
fn durable_create_intent_is_reserved_once_until_agent_status_charges_it() {
    let vmid = Ulid::generate();
    let agent_id = super::ActorId::new(7);
    let mut manifest = test_manifest(4, ByteSize::gib(8).as_u64());
    manifest.id = vmid;
    let manifests = AHashMap::from([(vmid, manifest)]);
    let placement = odorobo::cluster_state::PlacementRecord::active(vmid, "owner-a".to_owned());
    let durable = AHashMap::from([(vmid, placement.clone())]);
    let pending = AHashMap::from([(
        vmid,
        vec![VmPlacement {
            agent_id,
            lifecycle: VmLifecycle::Pending,
            created_at: Instant::now(),
            last_confirmed_at: None,
        }],
    )]);
    let uncharged_status = AgentStatus {
        hostname: "owner-a".to_owned(),
        vcpus: 8,
        ram: ByteSize::gib(32),
        used_vcpus: 0,
        used_ram: ByteSize::b(0),
        vms: Vec::new(),
        reserved_vms: Vec::new(),
        resource_charges: Vec::new(),
        metadata: ObjectMetadata::default(),
    };
    let agents = AHashMap::from([(agent_id, uncharged_status.clone())]);

    // An adopted durable create reserves its original owner even before local
    // placement reconciliation has created a pending placeholder.
    assert_eq!(
        pending_resources_with_status_for_agent(
            &manifests,
            &AHashMap::new(),
            &AHashMap::new(),
            &durable,
            &agents,
            agent_id,
        ),
        (4, ByteSize::gib(8).as_u64())
    );

    // Repeated reconciliation and local pending state still represent one
    // reservation, not two.
    assert_eq!(
        pending_resources_with_status_for_agent(
            &manifests,
            &AHashMap::new(),
            &pending,
            &durable,
            &agents,
            agent_id,
        ),
        (4, ByteSize::gib(8).as_u64())
    );

    // Once the agent reports the reservation, scheduler-side pending accounting
    // must not add the same allocation again.
    let mut charged_status = uncharged_status;
    charged_status.reserved_vms.push(vmid);
    charged_status
        .resource_charges
        .push(crate::messages::agent::VMResourceCharge {
            vmid,
            generation: placement.generation,
            vcpus: 4,
            memory_bytes: ByteSize::gib(8).as_u64(),
        });
    let charged_agents = AHashMap::from([(agent_id, charged_status)]);
    assert_eq!(
        pending_resources_with_status_for_agent(
            &manifests,
            &AHashMap::new(),
            &pending,
            &durable,
            &charged_agents,
            agent_id,
        ),
        (0, 0)
    );
}

#[test]
fn unresolved_stop_reservation_survives_restart_and_deduplicates_matching_status() {
    let vmid = Ulid::generate();
    let agent_id = super::ActorId::new(7);
    let mut manifest = test_manifest(4, ByteSize::gib(8).as_u64());
    manifest.id = vmid;
    let manifests = AHashMap::from([(vmid, manifest.clone())]);
    let placement = odorobo::cluster_state::PlacementRecord::active(vmid, "owner-a".to_owned());
    let mut durable = AHashMap::from([(vmid, placement.clone())]);
    let uncharged = AgentStatus {
        hostname: "owner-a".to_owned(),
        vcpus: 8,
        ram: ByteSize::gib(32),
        used_vcpus: 0,
        used_ram: ByteSize::b(0),
        vms: Vec::new(),
        reserved_vms: Vec::new(),
        resource_charges: Vec::new(),
        metadata: ObjectMetadata::default(),
    };
    let agents = AHashMap::from([(agent_id, uncharged.clone())]);
    assert_eq!(
        pending_resources_with_status_for_agent(
            &manifests,
            &AHashMap::new(),
            &AHashMap::new(),
            &durable,
            &agents,
            agent_id,
        ),
        (4, ByteSize::gib(8).as_u64())
    );

    // A timed-out stop drops active reconciliation intent, but keeps its paired
    // manifest and owner as a capacity reservation across manager reconstruction.
    let mut stopping = placement;
    stopping.lifecycle = odorobo::cluster_state::PlacementLifecycle::Stopping;
    durable.insert(vmid, stopping.clone());
    let active_manifests = AHashMap::new();
    let stop_manifests = AHashMap::from([(vmid, manifest)]);
    for _manager_restart in 0..2 {
        assert_eq!(
            pending_resources_with_status_for_agent(
                &active_manifests,
                &stop_manifests,
                &AHashMap::new(),
                &durable,
                &agents,
                agent_id,
            ),
            (4, ByteSize::gib(8).as_u64())
        );
    }

    let mut charged = uncharged;
    charged.reserved_vms.push(vmid);
    charged
        .resource_charges
        .push(crate::messages::agent::VMResourceCharge {
            vmid,
            generation: stopping.generation,
            vcpus: 4,
            memory_bytes: ByteSize::gib(8).as_u64(),
        });
    let charged_agents = AHashMap::from([(agent_id, charged)]);
    assert_eq!(
        pending_resources_with_status_for_agent(
            &active_manifests,
            &stop_manifests,
            &AHashMap::new(),
            &durable,
            &charged_agents,
            agent_id,
        ),
        (0, 0)
    );
}

#[test]
fn stale_generation_status_cannot_certify_larger_durable_recreation() {
    let vmid = Ulid::generate();
    let agent_id = super::ActorId::new(7);
    let generation_g1 = Ulid::generate();
    let generation_g2 = Ulid::generate();
    let mut manifest_g2 = test_manifest(4, ByteSize::gib(24).as_u64());
    manifest_g2.id = vmid;
    let manifests = AHashMap::from([(vmid, manifest_g2)]);
    let placement_g2 = odorobo::cluster_state::PlacementRecord {
        vmid,
        node: "owner-a".to_owned(),
        generation: generation_g2,
        lifecycle: odorobo::cluster_state::PlacementLifecycle::Active,
    };
    let durable = AHashMap::from([(vmid, placement_g2)]);
    let old_status = AgentStatus {
        hostname: "owner-a".to_owned(),
        vcpus: 16,
        ram: ByteSize::gib(32),
        used_vcpus: 0,
        used_ram: ByteSize::b(0),
        vms: vec![vmid],
        reserved_vms: Vec::new(),
        resource_charges: vec![crate::messages::agent::VMResourceCharge {
            vmid,
            generation: generation_g1,
            vcpus: 4,
            memory_bytes: ByteSize::gib(4).as_u64(),
        }],
        metadata: ObjectMetadata::default(),
    };
    let agents = AHashMap::from([(agent_id, old_status.clone())]);

    let pending = pending_resources_with_status_for_agent(
        &manifests,
        &AHashMap::new(),
        &AHashMap::new(),
        &durable,
        &agents,
        agent_id,
    );
    assert_eq!(pending, (4, ByteSize::gib(24).as_u64()));
    assert!(!has_capacity(
        32,
        old_status.used_vcpus,
        1,
        ByteSize::gib(32).as_u64(),
        old_status.used_ram.as_u64().saturating_add(pending.1),
        ByteSize::gib(12).as_u64(),
    ));

    // Only a confirmation matching G2 and its requested resources can transfer
    // the durable reservation into status-reported usage.
    let mut legacy_status = old_status.clone();
    legacy_status.resource_charges.clear();
    assert_eq!(
        pending_resources_with_status_for_agent(
            &manifests,
            &AHashMap::new(),
            &AHashMap::new(),
            &durable,
            &AHashMap::from([(agent_id, legacy_status)]),
            agent_id,
        ),
        (4, ByteSize::gib(24).as_u64())
    );

    let mut current_status = old_status;
    current_status.used_vcpus = 4;
    current_status.used_ram = ByteSize::gib(24);
    current_status.resource_charges[0].generation = generation_g2;
    current_status.resource_charges[0].memory_bytes = ByteSize::gib(24).as_u64();
    let matching_agents = AHashMap::from([(agent_id, current_status.clone())]);
    assert_eq!(
        pending_resources_with_status_for_agent(
            &manifests,
            &AHashMap::new(),
            &AHashMap::new(),
            &durable,
            &matching_agents,
            agent_id,
        ),
        (0, 0)
    );
    assert!(has_capacity(
        32,
        current_status.used_vcpus,
        1,
        ByteSize::gib(32).as_u64(),
        current_status.used_ram.as_u64(),
        ByteSize::gib(8).as_u64(),
    ));
}

#[test]
fn newer_agent_status_delta_applies_capacity_without_membership_changes() {
    let vmid = Ulid::generate();
    let mut status = AgentStatus {
        hostname: "agent".to_owned(),
        vcpus: 8,
        ram: ByteSize::gib(16),
        used_vcpus: 4,
        used_ram: ByteSize::gib(8),
        vms: vec![vmid],
        reserved_vms: Vec::new(),
        resource_charges: Vec::new(),
        metadata: ObjectMetadata::default(),
    };
    let reserved_vmid = Ulid::generate();
    let reserved_generation = Ulid::generate();
    let revision = apply_status_update(
        &mut status,
        AgentStatusUpdate::Delta {
            revision: 12,
            added: Vec::new(),
            removed: Vec::new(),
            reserved_vms: vec![reserved_vmid],
            resource_charges: vec![crate::messages::agent::VMResourceCharge {
                vmid: reserved_vmid,
                generation: reserved_generation,
                vcpus: 2,
                memory_bytes: ByteSize::gib(4).as_u64(),
            }],
            used_vcpus: 2,
            used_ram: ByteSize::gib(4),
        },
    );

    assert_eq!(revision, 12);
    assert_eq!(status.vms, vec![vmid]);
    assert_eq!(status.reserved_vms, vec![reserved_vmid]);
    assert_eq!(
        status.resource_charges,
        vec![crate::messages::agent::VMResourceCharge {
            vmid: reserved_vmid,
            generation: reserved_generation,
            vcpus: 2,
            memory_bytes: ByteSize::gib(4).as_u64(),
        }]
    );
    assert_eq!(status.used_vcpus, 2);
    assert_eq!(status.used_ram, ByteSize::gib(4));
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
        reserved_vms: Vec::new(),
        resource_charges: Vec::new(),
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
        reserved_vms: Vec::new(),
        resource_charges: Vec::new(),
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
        reserved_vms: Vec::new(),
        resource_charges: Vec::new(),
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
        reserved_vms: Vec::new(),
        resource_charges: Vec::new(),
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
        stop_manifests: AHashMap::new(),
        durable_placements: AHashMap::new(),
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
        state_store: Arc::new(StateStore::Memory(MemoryStateStore::default())),
    };

    scheduler.cleanup_vm_actor(vm_actor_id);

    assert!(scheduler.vm_manifests.contains_key(&vmid));
    assert!(scheduler.vm_placements[&vmid].is_empty());
    drop(scheduler);
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
        stop_manifests: AHashMap::new(),
        durable_placements: AHashMap::from([(
            vmid,
            odorobo::cluster_state::PlacementRecord::active(vmid, "node-a".to_owned()),
        )]),
        vm_placements: AHashMap::from([(vmid, Vec::new())]),
        vm_data_cache: AHashMap::from([(vmid, vec![CachedVMActor { actor_ref: None }])]),
        vm_keepalive_tasks: AHashMap::new(),
        pending_resources_cache: None,
        agent_vm_index: AHashMap::from([(agent_id, AHashSet::from([vmid]))]),
        actor_kinds: AHashMap::new(),
        cache_actor_finder: None,
        state_store: Arc::new(StateStore::Memory(MemoryStateStore::default())),
    };

    scheduler.remove_vm_intent(vmid);

    assert!(!scheduler.vm_manifests.contains_key(&vmid));
    assert!(!scheduler.vm_placements.contains_key(&vmid));
    assert!(!scheduler.vm_data_cache.contains_key(&vmid));
    assert!(!scheduler.durable_placements.contains_key(&vmid));
    assert!(!scheduler.vm_actorid_ulid_map.contains_key(&vm_actor_id));
    assert!(!scheduler.agent_vm_index[&agent_id].contains(&vmid));
    drop(scheduler);
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
        stop_manifests: AHashMap::new(),
        vm_placements: AHashMap::from([(
            vmid,
            vec![VmPlacement {
                agent_id: placement_agent_id,
                lifecycle: VmLifecycle::Running,
                created_at: Instant::now(),
                last_confirmed_at: Some(Instant::now()),
            }],
        )]),
        durable_placements: AHashMap::new(),
        vm_data_cache: AHashMap::from([(vmid, vec![CachedVMActor { actor_ref: None }])]),
        vm_keepalive_tasks: AHashMap::new(),
        pending_resources_cache: None,
        agent_vm_index: AHashMap::new(),
        actor_kinds: AHashMap::from([(agent_id, CachedActorKind::Agent)]),
        cache_actor_finder: None,
        state_store: Arc::new(StateStore::Memory(MemoryStateStore::default())),
    };

    scheduler.cleanup_agent_actor(agent_id);

    assert!(scheduler.vm_placements.contains_key(&vmid));
    assert!(scheduler.vm_manifests.contains_key(&vmid));
    assert!(scheduler.vm_actorid_ulid_map.contains_key(&vm_actor_id));
    assert!(scheduler.vm_data_cache.contains_key(&vmid));
    drop(scheduler);
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
        stop_manifests: AHashMap::new(),
        vm_placements: AHashMap::new(),
        durable_placements: AHashMap::new(),
        vm_data_cache: AHashMap::from([(vmid, vec![CachedVMActor { actor_ref: None }])]),
        vm_keepalive_tasks: AHashMap::new(),
        pending_resources_cache: None,
        agent_vm_index: AHashMap::new(),
        actor_kinds: AHashMap::from([(agent_id, CachedActorKind::Agent)]),
        cache_actor_finder: None,
        state_store: Arc::new(StateStore::Memory(MemoryStateStore::default())),
    };

    scheduler.cleanup_vm_actor(vm_actor_id);

    assert!(scheduler.actor_kinds.contains_key(&agent_id));
    assert!(scheduler.vm_manifests.contains_key(&vmid));
    assert!(scheduler.vm_data_cache.contains_key(&vmid));
    drop(scheduler);
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
