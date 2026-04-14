use stable_eyre::{Result, eyre::Context};
use zbus::Connection;
/// template for systemd unit name for CH instances, where the instance ID is substituted into the unit name
pub fn systemd_instance_unit_name(vmid: &str) -> String {
    format!("odorobo-ch@{vmid}.service")
}

pub async fn zbus_system_connection() -> Result<Connection> {
    Connection::system()
        .await
        .wrap_err("Failed to connect to system D-Bus")
}
