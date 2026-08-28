/*
    Ported from the original GTK4 discovery.rs (Copyright (C) 2025, 2026
    John Melton G0ORX/N6LYT) to be UI-agnostic and thread-safe so it can
    be driven from an egui/eframe background thread instead of GTK's
    single-threaded main loop.

    This program is free software: you can redistribute it and/or modify
    it under the terms of the GNU General Public License as published by
    the Free Software Foundation, either version 3 of the License, or
    (at your option) any later version.
*/

use network_interface::{Addr, NetworkInterface, NetworkInterfaceConfig};
use serde::{Deserialize, Serialize};
use socket2::{Domain, Socket, Type};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const DISCOVERY_PORT: u16 = 1024;
const SOCKET_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Boards {
    Metis,
    Hermes,
    Hermes2,
    Angelia,
    Orion,
    Orion2,
    Saturn,
    HermesLite,
    HermesLite2,
    Unknown,
}

impl Default for Boards {
    fn default() -> Self {
        Boards::Unknown
    }
}

#[derive(Copy, Clone, Debug)]
pub struct Device {
    pub address: SocketAddr,
    pub my_address: SocketAddr,
    // Parsed straight off the wire but not consumed by any feature yet
    // -- real protocol data, not scaffolding, so kept (and allowed)
    // rather than thrown away just to quiet the compiler.
    #[allow(dead_code)]
    pub device: u8, // protocol-relative board id
    pub board: Boards, // protocol-independent board identity
    pub protocol: u8, // 1 or 2
    pub version: u8,
    pub status: u8, // 2 = idle/available, 3 = running/in use
    pub mac: [u8; 6],
    pub supported_receivers: u8,
    #[allow(dead_code)]
    pub supported_transmitters: u8,
    pub adcs: u8,
    /// Now consumed by main.rs's band-button rows (and PA Calibration's
    /// per-band list) -- see BANDS::iter() call sites there: a real
    /// report that HermesLite/HermesLite2 don't reach 6m, confirmed
    /// against piHPSDR's own identical frequency_max clamp (its
    /// band_menu.c skips any band button whose range falls outside
    /// radio->frequency_min/frequency_max), which this project already
    /// computed correctly per board_info_p1/p2 above but never actually
    /// used anywhere until now.
    pub frequency_min: u64,
    pub frequency_max: u64,
}

impl Device {
    /// Parse a Protocol 1 (Metis) discovery reply.
    /// Layout: <0xEF><0xFE><status><MAC 6 bytes><fw version><board id>...
    fn from_p1_reply(buf: &[u8], src: SocketAddr, my_address: SocketAddr) -> Option<Device> {
        if buf.len() < 20 {
            return None;
        }
        let status = buf[2];
        let mac = [buf[3], buf[4], buf[5], buf[6], buf[7], buf[8]];
        let version = buf[9];
        let board_id = buf[10];
        let (board, adcs, supported_receivers, supported_transmitters, frequency_min, frequency_max) =
            board_info_p1(board_id, version, buf[19]);

        Some(Device {
            address: src,
            my_address,
            device: board_id,
            board,
            protocol: 1,
            version,
            status,
            mac,
            supported_receivers,
            supported_transmitters,
            adcs,
            frequency_min,
            frequency_max,
        })
    }

    /// Parse a Protocol 2 discovery reply.
    /// Layout: <seq 4 bytes><status><MAC 6 bytes><board id><...><fw version>
    fn from_p2_reply(buf: &[u8], src: SocketAddr, my_address: SocketAddr) -> Option<Device> {
        if buf.len() < 14 {
            return None;
        }
        let status = buf[4];
        let mac = [buf[5], buf[6], buf[7], buf[8], buf[9], buf[10]];
        let board_id = buf[11];
        let version = buf[13];
        let (board, adcs, supported_receivers, supported_transmitters, frequency_min, frequency_max) =
            board_info_p2(board_id);

        Some(Device {
            address: src,
            my_address,
            device: board_id,
            board,
            protocol: 2,
            version,
            status,
            mac,
            supported_receivers,
            supported_transmitters,
            adcs,
            frequency_min,
            frequency_max,
        })
    }
}

/// Board characteristics for Protocol 1 board IDs.
/// `buf19` is only meaningful for HermesLite2, which reports its receiver
/// count in that byte (mirrors the original code's `buf[19]` lookup).
fn board_info_p1(board_id: u8, version: u8, buf19: u8) -> (Boards, u8, u8, u8, u64, u64) {
    match board_id {
        0 => (Boards::Metis, 1, 5, 1, 0, 61_440_000),
        1 => (Boards::Hermes, 1, 5, 1, 0, 61_440_000),
        4 => (Boards::Angelia, 2, 7, 1, 0, 61_440_000),
        5 => (Boards::Orion, 2, 7, 1, 0, 61_440_000),
        6 => {
            if version < 42 {
                (Boards::HermesLite, 1, 2, 1, 0, 30_720_000)
            } else {
                (Boards::HermesLite2, 1, buf19, 1, 0, 30_720_000)
            }
        }
        10 => (Boards::Orion2, 2, 7, 1, 0, 61_440_000),
        _ => (Boards::Unknown, 1, 1, 1, 0, 61_440_000),
    }
}

/// Board characteristics for Protocol 2 board IDs.
/// Note: these IDs do NOT share numbering with Protocol 1 -- e.g. board id
/// 6 means HermesLite here but Orion2 there. Keep the tables separate.
fn board_info_p2(board_id: u8) -> (Boards, u8, u8, u8, u64, u64) {
    match board_id {
        0 => (Boards::Metis, 1, 5, 1, 0, 61_440_000), // ATLAS
        1 => (Boards::Hermes, 1, 5, 1, 0, 61_440_000),
        2 => (Boards::Hermes2, 1, 5, 1, 0, 61_440_000),
        3 => (Boards::Angelia, 2, 7, 1, 0, 61_440_000),
        4 => (Boards::Orion, 2, 7, 1, 0, 61_440_000),
        5 => (Boards::Orion2, 2, 7, 1, 0, 61_440_000),
        // Real HermesLite2 hardware supports up to 4 receivers (confirmed
        // by the user), not 5 -- this entry is very likely unreachable in
        // practice anyway, since HermesLite2 only actually speaks Protocol
        // 1 (see board_info_p1's buf19-based dynamic lookup, the path a
        // real HL2 unit's discovery reply takes), but corrected for
        // accuracy rather than left silently wrong.
        6 => (Boards::HermesLite2, 1, 4, 1, 0, 30_720_000),
        10 => (Boards::Saturn, 2, 7, 1, 0, 61_440_000),
        _ => (Boards::Unknown, 1, 1, 1, 0, 61_440_000),
    }
}

/// Shared socket setup for both discovery phases.
fn open_socket(bind_addr: SocketAddr, broadcast: bool) -> std::io::Result<UdpSocket> {
    let setup_socket = Socket::new(Domain::for_address(bind_addr), Type::DGRAM, Some(socket2::Protocol::UDP))?;
    setup_socket.set_broadcast(broadcast)?;
    setup_socket.set_read_timeout(Some(SOCKET_TIMEOUT))?;
    setup_socket.set_write_timeout(Some(SOCKET_TIMEOUT))?;
    setup_socket.set_reuse_address(true)?;
    #[cfg(unix)]
    setup_socket.set_reuse_port(true)?;
    setup_socket.bind(&bind_addr.into())?;
    Ok(setup_socket.into())
}

/// Broadcast a Protocol 1 discovery packet and collect replies until the
/// read timeout fires with nothing pending.
pub fn protocol1_discovery(devices: Arc<Mutex<Vec<Device>>>, socket_addr: SocketAddr) {
    let socket = match open_socket(socket_addr, true) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("protocol1_discovery: failed to open socket: {e}");
            return;
        }
    };

    let mut request = [0u8; 63];
    request[0] = 0xEF;
    request[1] = 0xFE;
    request[2] = 0x02;
    if let Err(e) = socket.send_to(&request, ("255.255.255.255", DISCOVERY_PORT)) {
        eprintln!("protocol1_discovery: send failed: {e}");
        return;
    }

    let local_addr = socket.local_addr().unwrap_or(socket_addr);
    let mut buf = [0u8; 1024];
    loop {
        match socket.recv_from(&mut buf) {
            Ok((amt, src)) if amt == 60 && src.port() == DISCOVERY_PORT => {
                if let Some(device) = Device::from_p1_reply(&buf[..amt], src, local_addr) {
                    devices.lock().unwrap().push(device);
                }
            }
            Ok(_) => continue,
            Err(_) => break, // timeout or real error -- either way, stop listening
        }
    }
}

/// Broadcast a Protocol 2 discovery packet and collect replies.
pub fn protocol2_discovery(devices: Arc<Mutex<Vec<Device>>>, socket_addr: SocketAddr) {
    let socket = match open_socket(socket_addr, true) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("protocol2_discovery: failed to open socket: {e}");
            return;
        }
    };

    let mut request = [0u8; 60];
    request[4] = 0x02;
    if let Err(e) = socket.send_to(&request, ("255.255.255.255", DISCOVERY_PORT)) {
        eprintln!("protocol2_discovery: send failed: {e}");
        return;
    }

    let local_addr = socket.local_addr().unwrap_or(socket_addr);
    let mut buf = [0u8; 1024];
    loop {
        match socket.recv_from(&mut buf) {
            Ok((amt, src)) if amt == 60 && src.port() == DISCOVERY_PORT => {
                if let Some(device) = Device::from_p2_reply(&buf[..amt], src, local_addr) {
                    devices.lock().unwrap().push(device);
                }
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }
}

/// Run both discovery phases on every active IPv4 interface.
/// Blocking -- call this from a background thread, not the UI thread.
///
/// `interface_names`: filled in with this machine's own address -> the
/// interface name it belongs to (e.g. "eth0"/"enp3s0") as each interface
/// is probed, so the UI can show a real interface name next to each
/// discovered device's `my_address` (which was previously the only
/// interface-identifying thing shown, despite the discovery window's
/// own "Interface" column heading -- a real report that it was actually
/// just displaying an IP there). Left untouched (not cleared) by
/// `manual_discovery`, which has no interface concept -- a device found
/// that way just won't have an entry here, and the UI falls back to
/// showing its `my_address` alone.
pub fn discover(devices: Arc<Mutex<Vec<Device>>>, interface_names: Arc<Mutex<HashMap<IpAddr, String>>>) {
    devices.lock().unwrap().clear();

    let interfaces = match NetworkInterface::show() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("discover: failed to enumerate network interfaces: {e}");
            return;
        }
    };

    for itf in interfaces {
        for addr in &itf.addr {
            if let Addr::V4(v4_info) = addr {
                let ip = v4_info.ip;
                // Probe that the interface is actually up/bindable before using it.
                if std::net::UdpSocket::bind((ip, 5000)).is_ok() {
                    interface_names.lock().unwrap().insert(IpAddr::V4(ip), itf.name.clone());
                    let socket_address = SocketAddr::new(IpAddr::V4(ip), 50000);
                    protocol1_discovery(Arc::clone(&devices), socket_address);
                    protocol2_discovery(Arc::clone(&devices), socket_address);
                } else {
                    eprintln!("discover: interface {} not bindable, skipping", itf.name);
                }
            }
        }
    }
}

/// Unicast discovery against a specific IP, trying Protocol 1 then
/// Protocol 2. Returns true and appends to `devices` if either replies.
/// Blocking -- call from a background thread.
pub fn manual_discovery(devices: Arc<Mutex<Vec<Device>>>, target_ip: IpAddr) -> bool {
    let bind_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
    let socket = match open_socket(bind_addr, false) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("manual_discovery: failed to open socket: {e}");
            return false;
        }
    };
    let local_addr = socket.local_addr().unwrap_or(bind_addr);
    let target_addr = SocketAddr::new(target_ip, DISCOVERY_PORT);

    // Try Protocol 1 first.
    let mut p1_request = [0u8; 63];
    p1_request[0] = 0xEF;
    p1_request[1] = 0xFE;
    p1_request[2] = 0x02;
    if socket.send_to(&p1_request, target_addr).is_ok() {
        let mut buf = [0u8; 1024];
        if let Ok((amt, src)) = socket.recv_from(&mut buf) {
            if amt == 60 && src.ip() == target_ip {
                if let Some(device) = Device::from_p1_reply(&buf[..amt], src, local_addr) {
                    devices.lock().unwrap().push(device);
                    return true;
                }
            }
        }
    }

    // Fall back to Protocol 2.
    let mut p2_request = [0u8; 60];
    p2_request[4] = 0x02;
    if socket.send_to(&p2_request, target_addr).is_ok() {
        let mut buf = [0u8; 1024];
        if let Ok((amt, src)) = socket.recv_from(&mut buf) {
            if amt == 60 && src.ip() == target_ip {
                if let Some(device) = Device::from_p2_reply(&buf[..amt], src, local_addr) {
                    devices.lock().unwrap().push(device);
                    return true;
                }
            }
        }
    }

    false
}
