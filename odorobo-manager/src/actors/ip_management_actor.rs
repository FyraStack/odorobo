use kameo::prelude::*;
use stable_eyre::{Report, Result};
use tracing::{error, info, warn};

// idk if we ever agreed upon an OUI for fyra, but im reserving `FYR` for this
// -cappy
pub const FYRA_OUI: [u8; 3] = [0x46, 0x59, 0x52];

/// Calculates a MAC address for a given IP address using the FYRA OUI prefix.
///
/// Takes the last 3 bytes of the IP address and combines them with the FYRA OUI prefix.
pub fn calculate_mac_address(ip: [u8; 4]) -> [u8; 6] {
    let mut mac = [0u8; 6];
    mac[0..3].copy_from_slice(&FYRA_OUI);
    mac[3..].copy_from_slice(&ip[1..]);
    mac
}

#[test]
fn test_calculate_mac_address() {
    let ip = [192, 168, 1, 1];
    let mac = calculate_mac_address(ip);
    assert_eq!(mac, [0x46, 0x59, 0x52, 168, 0x01, 0x01]);
}

/// HTTP REST API service
#[derive(RemoteActor)]
pub struct IPManagementActor;

impl Actor for IPManagementActor {
    type Args = ();
    type Error = Report;

    async fn on_start(_state: Self::Args, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        // if we need to like prep the router stuff

        Ok(Self)
    }
}
