//! systemd provisioning module for odorobo agent

use crate::util::{systemd_instance_unit_name, zbus_system_connection};
/// This module provides functions for provisioning CH instances for odorobo agent
/// using systemd.
///
/// All it should do is simply just start and stop systemd services with the correct parameters
use stable_eyre::{Result, eyre::WrapErr, eyre::eyre};
use tokio::time::{Duration, sleep};
use tracing::trace;
use zbus::Connection;
use zbus_systemd::systemd1::{ManagerProxy, ServiceProxy};
/// template for systemd unit name for CH instances, where the instance ID is substituted into the unit name
// pub fn instance_unit_name(vmid: &str) -> String {
//     format!("odorobo-ch@{vmid}.service")
// }

// async fn system_connection() -> Result<Connection> {
//     Connection::system()
//         .await
//         .wrap_err("Failed to connect to system D-Bus")
// }

pub async fn manager_proxy(connection: &Connection) -> Result<ManagerProxy<'_>> {
    ManagerProxy::new(connection)
        .await
        .wrap_err("Failed to create systemd manager proxy")
}

pub async fn service_proxy<'a>(
    connection: &'a Connection,
    unit_name: &str,
) -> Result<ServiceProxy<'a>> {
    let manager = manager_proxy(connection).await?;
    let unit_path = manager
        .load_unit(unit_name.to_string())
        .await
        .wrap_err_with(|| format!("Failed to load systemd unit {unit_name}"))?;

    ServiceProxy::builder(connection)
        .path(unit_path)
        .wrap_err_with(|| format!("Failed to build path for systemd unit {unit_name}"))?
        .build()
        .await
        .wrap_err_with(|| format!("Failed to create service proxy for systemd unit {unit_name}"))
}

#[tracing::instrument]
pub async fn start_instance(vmid: &str) -> Result<i32> {
    let connection = zbus_system_connection().await?;
    let manager = manager_proxy(&connection).await?;
    let unit_name = systemd_instance_unit_name(vmid);
    trace!(?unit_name, "Starting systemd unit");

    manager
        .start_unit(unit_name.clone(), "replace".into())
        .await
        .wrap_err_with(|| format!("Failed to start systemd unit {unit_name}"))?;

    let unit_path = manager
        .load_unit(unit_name.clone())
        .await
        .wrap_err_with(|| format!("Failed to load systemd unit {unit_name}"))?;

    let unit = zbus_systemd::systemd1::UnitProxy::builder(&connection)
        .path(unit_path.clone())
        .wrap_err_with(|| format!("Failed to build path for systemd unit {unit_name}"))?
        .build()
        .await
        .wrap_err_with(|| format!("Failed to create unit proxy for systemd unit {unit_name}"))?;

    let service = zbus_systemd::systemd1::ServiceProxy::builder(&connection)
        .path(&unit_path)
        .wrap_err_with(|| format!("Failed to build path for systemd unit {unit_name}"))?
        .build()
        .await
        .wrap_err_with(|| format!("Failed to create service proxy for systemd unit {unit_name}"))?;

    // start_unit enqueues a job but doesn't wait for it to complete.
    // Poll until the service reaches a terminal state so we can return a real PID.
    const MAX_WAIT: u32 = 30;
    for attempt in 0..MAX_WAIT {
        let state = unit
            .active_state()
            .await
            .wrap_err_with(|| format!("Failed to get ActiveState for {unit_name}"))?;

        trace!(
            ?unit_name,
            ?state,
            attempt,
            "Waiting for systemd unit to start"
        );

        match state.as_str() {
            "active" => {
                let pid = service
                    .main_pid()
                    .await
                    .wrap_err_with(|| format!("Failed to get MainPID for {unit_name}"))?;
                return Ok(pid as i32);
            }
            "failed" => {
                return Err(eyre!("Systemd unit {unit_name} failed to start"));
            }
            _ => {}
        }

        if attempt < MAX_WAIT - 1 {
            sleep(Duration::from_millis(500)).await;
        }
    }

    Err(eyre!(
        "Systemd unit {unit_name} did not become active within {MAX_WAIT} attempts"
    ))
}

#[tracing::instrument]
pub async fn stop_instance(vmid: &str) -> Result<()> {
    trace!(?vmid, "Stopping instance");
    let connection = zbus_system_connection().await?;
    let manager = manager_proxy(&connection).await?;
    let unit_name = systemd_instance_unit_name(vmid);

    manager
        .stop_unit(unit_name.clone(), "replace".into())
        .await
        .wrap_err_with(|| format!("Failed to stop systemd unit {unit_name}"))?;

    Ok(())
}

#[tracing::instrument]
pub async fn get_main_pid(vmid: &str) -> Result<i32> {
    trace!(?vmid, "Getting MainPID for instance");
    let connection = zbus_system_connection().await?;
    let unit_name = systemd_instance_unit_name(vmid);
    let service = service_proxy(&connection, &unit_name).await?;

    let pid = service
        .main_pid()
        .await
        .wrap_err_with(|| format!("Failed to get MainPID for {unit_name}"))?;

    Ok(pid as i32)
}
