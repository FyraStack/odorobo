use std::hint::black_box;
use std::time::Instant;

use bytesize::ByteSize;
use odorobo::messages::agent::{AgentStatus, AgentStatusUpdate, apply_status_update};
use odorobo::types::ObjectMetadata;
use ulid::Ulid;

fn status(vm_count: usize) -> AgentStatus {
    AgentStatus {
        hostname: "benchmark-agent".to_owned(),
        vcpus: 64,
        ram: ByteSize::gb(256),
        used_vcpus: u32::try_from(vm_count).expect("benchmark VM count fits in u32"),
        used_ram: ByteSize::gb(vm_count as u64),
        vms: (0..vm_count).map(|_| Ulid::generate()).collect(),
        metadata: ObjectMetadata::default(),
    }
}

fn main() {
    let iterations = std::env::var("ITERATIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(10_000usize);

    println!("AgentStatus benchmark; iterations={iterations}");
    println!("Set ITERATIONS to change the sample count.");

    for vm_count in [10, 100, 1_000, 10_000] {
        let base = status(vm_count);
        let added = vec![Ulid::generate()];
        let removed = vec![base.vms[vm_count / 2]];

        let full_update = AgentStatusUpdate::Full {
            revision: 1,
            status: base.clone(),
        };
        let delta_update = AgentStatusUpdate::Delta {
            revision: 1,
            added,
            removed,
            used_vcpus: base.used_vcpus,
            used_ram: base.used_ram,
        };

        let full_payload = serde_json::to_vec(&full_update).expect("full update serializes");
        let delta_payload = serde_json::to_vec(&delta_update).expect("delta update serializes");

        let start = Instant::now();
        for _ in 0..iterations {
            black_box(full_update.clone());
        }
        let full_elapsed = start.elapsed();

        let start = Instant::now();
        for _ in 0..iterations {
            let mut applied = base.clone();
            black_box(apply_status_update(&mut applied, delta_update.clone()));
        }
        let delta_elapsed = start.elapsed();

        println!(
            "vms={vm_count:>5} full={full_elapsed:?} delta={delta_elapsed:?} full_payload={:>8}B delta_payload={:>5}B",
            full_payload.len(),
            delta_payload.len(),
        );
    }
}
