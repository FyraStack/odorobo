//! Static interface configuration over rtnetlink.
//!
//! The host cannot hand a guest `/32` through the kernel `ip=` parameter (its
//! netmask shares `INADDR_NONE` with "unset") and the kernel takes no IPv6
//! there at all, so the config document carries the interface setup instead and
//! the init applies it: link up, addresses with prefixes, default route.

use std::ffi::CString;
use std::fs;
use std::io;
use std::net::IpAddr;
use std::os::fd::RawFd;

/// One interface's static configuration, matched to a device by MAC address.
#[derive(Debug, Clone)]
pub struct InterfaceConfig {
    pub mac: String,
    pub addresses: Vec<Address>,
    pub gateway: Option<IpAddr>,
    pub mtu: Option<u32>,
}

/// An address with its prefix length, `10.40.10.7/24` or `fd42:40:10::7/64`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Address {
    pub ip: IpAddr,
    pub prefix_len: u8,
}

impl std::str::FromStr for Address {
    type Err = io::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (address, prefix) = match value.split_once('/') {
            Some((address, prefix)) => (address, Some(prefix)),
            None => (value, None),
        };
        let ip: IpAddr = address.trim().parse().map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid address {address:?}: {err}"),
            )
        })?;
        let limit = if ip.is_ipv4() { 32 } else { 128 };
        let prefix_len = match prefix {
            Some(prefix) => prefix.trim().parse::<u8>().map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid prefix {prefix:?}: {err}"),
                )
            })?,
            None => limit,
        };
        if prefix_len > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("prefix /{prefix_len} does not fit {ip}"),
            ));
        }
        Ok(Self { ip, prefix_len })
    }
}

const NETLINK_ROUTE: i32 = 0;
const NLM_F_REQUEST: u16 = 1;
const NLM_F_ACK: u16 = 4;
const NLM_F_REPLACE: u16 = 0x100;
const NLM_F_CREATE: u16 = 0x400;
const NLMSG_ERROR: u16 = 2;
const RTM_NEWLINK: u16 = 16;
const RTM_NEWADDR: u16 = 20;
const RTM_NEWROUTE: u16 = 24;
const RT_TABLE_MAIN: u8 = 254;
const RTPROT_BOOT: u8 = 3;
const RT_SCOPE_UNIVERSE: u8 = 0;
const RT_SCOPE_LINK: u8 = 253;
const RTN_UNICAST: u8 = 1;
const IFF_UP: u32 = 0x1;
const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;
const IFLA_MTU: u16 = 4;
const RTA_DST: u16 = 1;
const RTA_OIF: u16 = 4;
const RTA_GATEWAY: u16 = 5;

/// Applies the host's interface configuration; a missing device is an error.
pub fn configure(interfaces: &[InterfaceConfig]) -> io::Result<()> {
    if interfaces.is_empty() {
        return Ok(());
    }
    let socket = Socket::open()?;
    for interface in interfaces {
        let name = interface_name_for_mac(&interface.mac)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no interface with MAC {}", interface.mac),
            )
        })?;
        let name = CString::new(name.clone()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("interface name {name:?} contains a NUL byte"),
            )
        })?;
        // SAFETY: name is a valid NUL-terminated interface name.
        let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
        if index == 0 {
            return Err(io::Error::last_os_error());
        }
        socket.request(&link_message(index, interface.mtu))?;
        for address in &interface.addresses {
            ignore_existing(socket.request(&address_message(index, address)))?;
        }
        if let Some(gateway) = interface.gateway {
            if !gateway_is_on_link(gateway, &interface.addresses) {
                let host = Address {
                    ip: gateway,
                    prefix_len: if gateway.is_ipv4() { 32 } else { 128 },
                };
                ignore_existing(socket.request(&route_message(index, Some(host), None)))?;
            }
            ignore_existing(socket.request(&route_message(index, None, Some(gateway))))?;
        }
    }
    Ok(())
}

/// EEXIST means the kernel already has that address or route: desired state.
fn ignore_existing(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(err) if err.raw_os_error() == Some(libc::EEXIST) => Ok(()),
        other => other,
    }
}

/// Whether a route to `gateway` already follows from a configured prefix.
fn gateway_is_on_link(gateway: IpAddr, addresses: &[Address]) -> bool {
    match gateway {
        IpAddr::V4(gateway) => addresses.iter().any(|address| match address.ip {
            IpAddr::V4(ip) => {
                let mask = netmask(address.prefix_len);
                u32::from(ip) & mask == u32::from(gateway) & mask
            }
            IpAddr::V6(_) => false,
        }),
        // Link-local gateways are always on-link by definition.
        IpAddr::V6(gateway) => gateway.is_unicast_link_local(),
    }
}

fn netmask(prefix_len: u8) -> u32 {
    match prefix_len {
        0 => 0,
        len => u32::MAX << u32::from(32_u8.saturating_sub(len)),
    }
}

fn interface_name_for_mac(mac: &str) -> io::Result<Option<String>> {
    for entry in fs::read_dir("/sys/class/net")? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(address) = fs::read_to_string(entry.path().join("address")) else {
            continue;
        };
        if address.trim().eq_ignore_ascii_case(mac.trim()) {
            return Ok(Some(name));
        }
    }
    Ok(None)
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NlMsgHeader {
    len: u32,
    kind: u16,
    flags: u16,
    seq: u32,
    pid: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct IfInfoMsg {
    family: u8,
    pad: u8,
    kind: u16,
    index: i32,
    flags: u32,
    change: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct IfAddrMsg {
    family: u8,
    prefix_len: u8,
    flags: u8,
    scope: u8,
    index: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RtMsg {
    family: u8,
    dst_len: u8,
    src_len: u8,
    tos: u8,
    table: u8,
    protocol: u8,
    scope: u8,
    kind: u8,
    flags: u32,
}

/// The address-family byte netlink messages carry.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "address family constants are small"
)]
const fn family(family: libc::c_int) -> u8 {
    family as u8
}

const fn family_of(ip: IpAddr) -> u8 {
    match ip {
        IpAddr::V4(_) => family(libc::AF_INET),
        IpAddr::V6(_) => family(libc::AF_INET6),
    }
}

fn octets_of(ip: IpAddr) -> Vec<u8> {
    match ip {
        IpAddr::V4(ip) => ip.octets().to_vec(),
        IpAddr::V6(ip) => ip.octets().to_vec(),
    }
}

fn push_attribute(buffer: &mut Vec<u8>, kind: u16, payload: &[u8]) {
    let len = u16::try_from(payload.len().saturating_add(4)).expect("attribute length fits u16");
    buffer.extend_from_slice(&len.to_ne_bytes());
    buffer.extend_from_slice(&kind.to_ne_bytes());
    buffer.extend_from_slice(payload);
    while !buffer.len().is_multiple_of(4) {
        buffer.push(0);
    }
}

fn message(kind: u16, flags: u16, body: &[u8]) -> Vec<u8> {
    let len = u32::try_from(16_usize.saturating_add(body.len())).expect("message length fits u32");
    let header = NlMsgHeader {
        len,
        kind,
        flags: flags | NLM_F_REQUEST | NLM_F_ACK,
        seq: 1,
        pid: 0,
    };
    let mut out = Vec::with_capacity(usize::try_from(len).expect("length fits usize"));
    // SAFETY: the header is plain data with no padding.
    out.extend_from_slice(unsafe { bytes_of(&header) });
    out.extend_from_slice(body);
    out
}

fn link_message(index: u32, mtu: Option<u32>) -> Vec<u8> {
    let header = IfInfoMsg {
        family: family(libc::AF_UNSPEC),
        pad: 0,
        kind: 0,
        index: index.cast_signed(),
        flags: IFF_UP,
        change: IFF_UP,
    };
    // SAFETY: the struct is plain data with no padding.
    let mut body = unsafe { bytes_of(&header) }.to_vec();
    if let Some(mtu) = mtu {
        push_attribute(&mut body, IFLA_MTU, &mtu.to_ne_bytes());
    }
    message(RTM_NEWLINK, 0, &body)
}

fn address_message(index: u32, address: &Address) -> Vec<u8> {
    let header = IfAddrMsg {
        family: family_of(address.ip),
        prefix_len: address.prefix_len,
        flags: 0,
        scope: 0,
        index,
    };
    // SAFETY: the struct is plain data with no padding.
    let mut body = unsafe { bytes_of(&header) }.to_vec();
    let octets = octets_of(address.ip);
    if address.ip.is_ipv4() {
        // IPv4 carries the address in both attributes; IPv6 only in ADDRESS.
        push_attribute(&mut body, IFA_LOCAL, &octets);
    }
    push_attribute(&mut body, IFA_ADDRESS, &octets);
    message(RTM_NEWADDR, NLM_F_CREATE | NLM_F_REPLACE, &body)
}

fn route_message(index: u32, destination: Option<Address>, gateway: Option<IpAddr>) -> Vec<u8> {
    let family = match (destination, gateway) {
        (Some(destination), _) => family_of(destination.ip),
        (None, Some(gateway)) => family_of(gateway),
        // The kernel only needs the family for the default route, and that
        // route always has a gateway in this code.
        (None, None) => family(libc::AF_INET),
    };
    let header = RtMsg {
        family,
        dst_len: destination.map_or(0, |destination| destination.prefix_len),
        src_len: 0,
        tos: 0,
        table: RT_TABLE_MAIN,
        protocol: RTPROT_BOOT,
        scope: if gateway.is_some() {
            RT_SCOPE_UNIVERSE
        } else {
            RT_SCOPE_LINK
        },
        kind: RTN_UNICAST,
        flags: 0,
    };
    // SAFETY: the struct is plain data with no padding.
    let mut body = unsafe { bytes_of(&header) }.to_vec();
    if let Some(destination) = destination {
        push_attribute(&mut body, RTA_DST, &octets_of(destination.ip));
    }
    if let Some(gateway) = gateway {
        push_attribute(&mut body, RTA_GATEWAY, &octets_of(gateway));
    }
    push_attribute(&mut body, RTA_OIF, &index.to_ne_bytes());
    message(RTM_NEWROUTE, NLM_F_CREATE | NLM_F_REPLACE, &body)
}

/// Reads a `#[repr(C)]` plain-data struct as bytes.
///
/// SAFETY: `T` must contain no padding and no pointers.
const unsafe fn bytes_of<T>(value: &T) -> &[u8] {
    // SAFETY: the caller guarantees the type is plain data.
    unsafe {
        std::slice::from_raw_parts(
            std::ptr::from_ref(value).cast::<u8>(),
            std::mem::size_of::<T>(),
        )
    }
}

struct Socket(RawFd);

impl Socket {
    fn open() -> io::Result<Self> {
        // SAFETY: socket(2) creates a new file descriptor or fails.
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                NETLINK_ROUTE,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `nl_family` is the only field that must be set; the padding
        // and pid/groups fields are valid as zero for a request socket.
        let mut address: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        address.nl_family = u16::from(family(libc::AF_NETLINK));
        let length =
            libc::socklen_t::try_from(std::mem::size_of::<libc::sockaddr_nl>()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "sockaddr_nl does not fit socklen_t",
                )
            })?;
        // SAFETY: address is initialized and the length passed matches it.
        let bound = unsafe { libc::bind(fd, std::ptr::from_ref(&address).cast(), length) };
        if bound < 0 {
            let err = io::Error::last_os_error();
            // SAFETY: fd was created above and is not used afterwards.
            unsafe { libc::close(fd) };
            return Err(err);
        }
        Ok(Self(fd))
    }

    fn request(&self, message: &[u8]) -> io::Result<()> {
        // SAFETY: message is a valid netlink message and send(2) copies it.
        let written = unsafe { libc::send(self.0, message.as_ptr().cast(), message.len(), 0) };
        if written < 0 {
            return Err(io::Error::last_os_error());
        }
        let written = usize::try_from(written).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "negative netlink write length")
        })?;
        if written != message.len() {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "short netlink write",
            ));
        }
        let mut buffer = [0_u8; 4096];
        loop {
            // SAFETY: buffer is writable memory of the given length.
            let received =
                unsafe { libc::recv(self.0, buffer.as_mut_ptr().cast(), buffer.len(), 0) };
            if received < 0 {
                return Err(io::Error::last_os_error());
            }
            let received = usize::try_from(received).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "negative netlink read length")
            })?;
            if received < 16 {
                continue;
            }
            if field_u16(&buffer[..received], 4)? != NLMSG_ERROR {
                // Only ACKs are expected; anything else is not ours to parse.
                continue;
            }
            let code = field_i32(&buffer[..received], 16)?;
            return if code == 0 {
                Ok(())
            } else {
                Err(io::Error::from_raw_os_error(code.saturating_neg()))
            };
        }
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        // SAFETY: the descriptor is owned by this value and closed once.
        unsafe { libc::close(self.0) };
    }
}

fn field_u16(buffer: &[u8], offset: usize) -> io::Result<u16> {
    let bytes = buffer
        .get(offset..offset.saturating_add(2))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "short netlink header"))?;
    let bytes: [u8; 2] = bytes
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "short netlink field"))?;
    Ok(u16::from_ne_bytes(bytes))
}

fn field_i32(buffer: &[u8], offset: usize) -> io::Result<i32> {
    let bytes = buffer
        .get(offset..offset.saturating_add(4))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "short netlink header"))?;
    let bytes: [u8; 4] = bytes
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "short netlink field"))?;
    Ok(i32::from_ne_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADDRESS_HEADER_LEN: usize = 16 + 8;
    const ROUTE_HEADER_LEN: usize = 16 + 12;

    fn read_u16(buffer: &[u8], offset: usize) -> u16 {
        field_u16(buffer, offset).unwrap()
    }

    #[expect(
        clippy::arithmetic_side_effects,
        reason = "test-only walker over a fixed message layout"
    )]
    fn attributes(buffer: &[u8], start: usize) -> Vec<(u16, Vec<u8>)> {
        let mut out = Vec::new();
        let mut offset = start;
        while offset + 4 <= buffer.len() {
            let len = usize::from(read_u16(buffer, offset));
            if len < 4 || offset + len > buffer.len() {
                break;
            }
            let kind = read_u16(buffer, offset + 2);
            out.push((kind, buffer[offset + 4..offset + len].to_vec()));
            offset += len.div_ceil(4) * 4;
        }
        out
    }

    #[test]
    fn parses_addresses_with_prefixes() {
        let v4: Address = "10.40.10.7/24".parse().unwrap();
        assert_eq!(v4.prefix_len, 24);
        assert!(v4.ip.is_ipv4());
        let v6: Address = "fd42:40:10::7/64".parse().unwrap();
        assert_eq!(v6.prefix_len, 64);
        assert_eq!("10.40.10.7".parse::<Address>().unwrap().prefix_len, 32);
        "10.40.10.7/33".parse::<Address>().unwrap_err();
        "not-an-address/24".parse::<Address>().unwrap_err();
    }

    #[test]
    fn link_message_sets_the_link_up_with_mtu() {
        let message = link_message(7, Some(1450));
        assert_eq!(read_u16(&message, 4), RTM_NEWLINK);
        assert_eq!(message.len(), 16 + 16 + 8);
        assert_eq!(field_i32(&message, 16 + 4).unwrap(), 7);
        assert_eq!(field_i32(&message, 16 + 8).unwrap(), IFF_UP.cast_signed());
        let attributes = attributes(&message, 16 + 16);
        assert_eq!(attributes.len(), 1);
        assert_eq!(attributes[0].0, IFLA_MTU);
        assert_eq!(attributes[0].1, 1450_u32.to_ne_bytes());
    }

    #[test]
    fn address_message_carries_both_ipv4_attributes() {
        let address: Address = "10.40.10.7/24".parse().unwrap();
        let message = address_message(3, &address);
        assert_eq!(read_u16(&message, 4), RTM_NEWADDR);
        assert_eq!(message.len(), ADDRESS_HEADER_LEN + 8 + 8);
        assert_eq!(message[16], family(libc::AF_INET));
        assert_eq!(message[17], 24);
        let attributes = attributes(&message, ADDRESS_HEADER_LEN);
        assert_eq!(attributes[0].0, IFA_LOCAL);
        assert_eq!(attributes[0].1, vec![10, 40, 10, 7]);
        assert_eq!(attributes[1].0, IFA_ADDRESS);
        assert_eq!(attributes[1].1, vec![10, 40, 10, 7]);
    }

    #[test]
    fn address_message_omits_local_for_ipv6() {
        let address: Address = "fd42:40:10::7/64".parse().unwrap();
        let message = address_message(3, &address);
        assert_eq!(message[16], family(libc::AF_INET6));
        assert_eq!(message[17], 64);
        let attributes = attributes(&message, ADDRESS_HEADER_LEN);
        assert_eq!(attributes.len(), 1);
        assert_eq!(attributes[0].0, IFA_ADDRESS);
        assert_eq!(
            attributes[0].1,
            address
                .ip
                .to_string()
                .parse::<std::net::Ipv6Addr>()
                .unwrap()
                .octets()
        );
    }

    #[test]
    fn route_message_encodes_the_default_route() {
        let gateway: IpAddr = "10.40.10.1".parse().unwrap();
        let message = route_message(3, None, Some(gateway));
        assert_eq!(read_u16(&message, 4), RTM_NEWROUTE);
        assert_eq!(message.len(), ROUTE_HEADER_LEN + 8 + 8);
        assert_eq!(message[16], family(libc::AF_INET));
        assert_eq!(message[17], 0, "a default route has no destination prefix");
        assert_eq!(message[20], RT_TABLE_MAIN);
        assert_eq!(message[21], RTPROT_BOOT);
        assert_eq!(message[22], RT_SCOPE_UNIVERSE);
        assert_eq!(message[23], RTN_UNICAST);
        let attributes = attributes(&message, ROUTE_HEADER_LEN);
        assert_eq!(attributes[0].0, RTA_GATEWAY);
        assert_eq!(attributes[0].1, vec![10, 40, 10, 1]);
        assert_eq!(attributes[1].0, RTA_OIF);
        assert_eq!(attributes[1].1, 3_u32.to_ne_bytes());
    }

    #[test]
    fn route_message_encodes_an_on_link_host_route() {
        let destination: Address = "10.40.10.1/32".parse().unwrap();
        let message = route_message(3, Some(destination), None);
        assert_eq!(message[17], 32);
        assert_eq!(message[22], RT_SCOPE_LINK);
        let attributes = attributes(&message, ROUTE_HEADER_LEN);
        assert_eq!(attributes[0].0, RTA_DST);
        assert_eq!(attributes[1].0, RTA_OIF);
    }

    #[test]
    fn detects_gateways_inside_a_configured_prefix() {
        let addresses: Vec<Address> = ["10.40.10.7/24".parse().unwrap()].into();
        assert!(gateway_is_on_link(
            "10.40.10.1".parse().unwrap(),
            &addresses
        ));
        assert!(!gateway_is_on_link(
            "10.41.10.1".parse().unwrap(),
            &addresses
        ));
        let host: Vec<Address> = ["10.40.10.7/32".parse().unwrap()].into();
        assert!(!gateway_is_on_link("10.40.10.1".parse().unwrap(), &host));
        assert!(gateway_is_on_link("fe80::1".parse().unwrap(), &host));
    }
}
