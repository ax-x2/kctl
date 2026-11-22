use crate::system::*;
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::{BufRead, BufReader},
    path::Path,
    process::Stdio,
};

type Device = (&'static str, u16, u16);
type Devices = BTreeMap<Device, Option<String>>;
type Symbols = HashMap<String, u16>;

fn hex(s: &str) -> Option<u16> {
    u16::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok()
}
fn devices(sys: &Path) -> Result<Devices> {
    let mut found = Devices::new();
    for (bus, v, d) in [
        ("pci", "vendor", "device"),
        ("usb", "idVendor", "idProduct"),
    ] {
        let path = sys.join(bus).join("devices");
        if !path.is_dir() {
            continue;
        }
        for item in fs::read_dir(path)? {
            let p = item?.path();
            let (Ok(v), Ok(d)) = (fs::read_to_string(p.join(v)), fs::read_to_string(p.join(d)))
            else {
                continue;
            };
            if let (Some(v), Some(d)) = (hex(&v), hex(&d)) {
                found.insert((bus, v, d), None);
            }
        }
    }
    Ok(found)
}
fn symbols(header: &str) -> Symbols {
    let mut map = Symbols::new();
    for line in header.lines() {
        let mut cols = line.split_whitespace();
        if cols.next() != Some("#define") {
            continue;
        }
        if let (Some(key), Some(value)) = (cols.next(), cols.next())
            && let Some(n) = hex(value)
        {
            map.insert(key.to_owned(), n);
            if let Some(short) = key.strip_prefix("PCI_VENDOR_ID_") {
                map.insert(short.to_owned(), n);
            }
        }
    }
    map
}
fn match_line(line: &str, map: &Symbols, devices: &mut Devices) {
    let Some((path, code)) = line.split_once(':') else {
        return;
    };
    for (name, bus) in [
        ("PCI_DEVICE(", "pci"),
        ("PCI_VDEVICE(", "pci"),
        ("USB_DEVICE(", "usb"),
    ] {
        let Some((_, args)) = code.split_once(name) else {
            continue;
        };
        let mut args = args.split([',', ')']).map(str::trim);
        let resolve = |s: &str| {
            map.get(s)
                .copied()
                .or_else(|| s.strip_prefix("0x").and_then(hex))
        };
        if let (Some(v), Some(d)) = (args.next().and_then(resolve), args.next().and_then(resolve))
            && let Some(hit) = devices.get_mut(&(bus, v, d))
            && hit.is_none()
        {
            *hit = Some(path.to_owned())
        }
    }
}
pub fn scan(src: &Path) -> Result {
    let map = symbols(&fs::read_to_string(src.join("include/linux/pci_ids.h"))?);
    let mut devices = devices(Path::new("/sys/bus"))?;
    let mut child = cmd(
        "grep",
        &[
            "-rIHE",
            "--include=*.c",
            "--include=*.h",
            "(PCI_V?DEVICE|USB_DEVICE)\\(",
            "drivers",
            "sound",
        ],
    )
    .current_dir(src)
    .stdout(Stdio::piped())
    .spawn()?;
    let mut input = BufReader::new(child.stdout.take().expect("requested stdout pipe"));
    let mut line = Vec::new();
    while input.read_until(b'\n', &mut line)? > 0 {
        if let Ok(text) = std::str::from_utf8(&line) {
            match_line(text, &map, &mut devices)
        }
        line.clear()
    }
    if !matches!(child.wait()?.code(), Some(0 | 1)) {
        return Err("source scan failed".into());
    }
    println!(
        "Hardware source hints (an ID match does not verify the kernel configuration or firmware)"
    );
    for ((bus, v, d), hit) in devices {
        println!(
            "{bus:3}  {v:04x}:{d:04x}  {}",
            hit.as_deref()
                .unwrap_or("unknown (no matching table entry)")
        )
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn require_vendor_and_device() {
        let mut ids = Devices::from([
            (("pci", 0x8086, 0x10d3), None),
            (("pci", 0x1234, 0x10d3), None),
        ]);
        let map = symbols("#define PCI_VENDOR_ID_INTEL 0x8086\n#define PCI_DEVICE_ID_CARD 0x10d3");
        match_line(
            "drivers/net/card.c: { PCI_VDEVICE(INTEL, PCI_DEVICE_ID_CARD) },",
            &map,
            &mut ids,
        );
        assert_eq!(
            ids[&("pci", 0x8086, 0x10d3)].as_deref(),
            Some("drivers/net/card.c")
        );
        assert!(ids[&("pci", 0x1234, 0x10d3)].is_none());
    }
}
