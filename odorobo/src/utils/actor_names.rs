use ulid::Ulid;

pub static AGENT: &str = "agent";
pub static VM: &str = "vm";
pub static SCHEDULER: &str = "scheduler";
pub static HTTP_API_SERVER: &str = "http_api_server";
pub static NETWORK: &str = "network_actor";

pub fn vm_actor_id(vmid: Ulid) -> String {
    format!("vm:{}", vmid)
}
