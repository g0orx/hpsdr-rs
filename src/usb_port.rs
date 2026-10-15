/*
    Identifying USB receivers that have no usable serial number by the
    physical USB port they're plugged into -- shared by rtlsdr.rs and
    mirisdr.rs.

    REAL REPORT (2026-10-15): with two MSI.SDR sticks, and then with two
    RTL-SDR dongles, plugged in only one of each was found -- every
    stick of a type shared one fixed sentinel MAC, and connecting opened
    whichever USB listed first. The MSI.SDR clones have no serial at
    all, and cheap RTL-SDRs commonly all report "00000001" (one of the
    reporter's two did), so the port is the only identity that's both
    stable and unique. It survives the re-enumeration a USB reset causes
    (same port, new device address) and is expected to be stable across
    reboots (see nusb's DeviceInfo::port_chain).
*/

/// Sentinel "MAC" for the device in `info`'s port:
/// [0, bus, port, port, port, tag], `tag` being the board type's own
/// last byte (discovery.rs's per-board sentinels). The MAC keys the
/// stick's config file and is how the driver finds it again at connect
/// time. The bus byte is Linux's bus number, or a hash of the bus id
/// elsewhere -- never 0, so no port MAC equals the old single-device
/// [0, 0, 0, 0, 0, tag]. Port chains deeper than three hubs fold the
/// rest into the last byte (high bit set): still unique in practice,
/// just not printable.
pub fn port_mac(info: &nusb::DeviceInfo, tag: u8) -> [u8; 6] {
    let bus_id = info.bus_id();
    let bus = match bus_id.parse::<u8>() {
        Ok(n) if n != 0 => n,
        _ => (bus_id.bytes().fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32)) % 255 + 1) as u8,
    };
    let chain = info.port_chain();
    let mut ports = [0u8; 3];
    for (slot, &p) in ports.iter_mut().zip(chain) {
        *slot = p;
    }
    if chain.len() > 3 {
        let folded = chain[2..].iter().fold(0u8, |h, &p| h.wrapping_mul(31).wrapping_add(p));
        ports[2] = folded | 0x80;
    }
    [0, bus, ports[0], ports[1], ports[2], tag]
}

/// The single fixed MAC every device of a type used before port_mac --
/// only read now, to carry an existing config over.
pub fn legacy_mac(tag: u8) -> [u8; 6] {
    [0, 0, 0, 0, 0, tag]
}

/// "bus-port.port.port" for a port_mac -- what the Discover window and
/// About tab show in place of an IP address, matching Linux's own sysfs
/// naming (e.g. "1-3.3").
pub fn port_label(mac: [u8; 6]) -> String {
    let ports: Vec<String> = mac[2..5].iter().filter(|&&p| p != 0).map(|p| p.to_string()).collect();
    format!("{}-{}", mac[1], ports.join("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_label_matches_sysfs_naming() {
        // Real ports from the multi-stick reports.
        assert_eq!(port_label([0, 1, 3, 3, 0, 3]), "1-3.3");
        assert_eq!(port_label([0, 6, 1, 0, 0, 2]), "6-1");
    }
}
