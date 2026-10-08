use crate::util::hash::hash_fnv1a;
use gethostname::gethostname;
use hex::ToHex;
use regex::Regex;
use ring::digest::SHA1_FOR_LEGACY_USE_ONLY;
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::CpuidResult;
use thiserror::Error;

/// Stand-in for architectures without a cpuid instruction (aarch64). Same
/// shape as `core::arch::x86_64::CpuidResult` so the hash composition code
/// is shared; the registers are always zero there (see `cpu::detect_arm`).
#[cfg(not(target_arch = "x86_64"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuidResult {
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
}

#[derive(Debug)]
pub struct CpuDetails {
    pub flags: CpuidResult,
    pub manufacturer: String,
    pub brand_name: String,
}

impl Default for CpuDetails {
    fn default() -> Self {
        Self {
            flags: CpuidResult {
                eax: 0,
                ebx: 0,
                ecx: 0,
                edx: 0,
            },
            manufacturer: String::default(),
            brand_name: String::default(),
        }
    }
}

#[derive(Debug, Default)]
pub struct HardwareInfo {
    pub version: u32,
    pub board_manufacturer: String,
    pub board_sn: String,
    pub bios_manufacturer: String,
    pub bios_sn: String,
    pub os_install_date: String,
    pub os_sn: String,
    pub disk_sn: String,
    pub volume_sn: String,
    pub gpu_pnp_id: Option<String>,
    pub mac: Option<String>,
    pub cpu_details: CpuDetails,
    pub hostname: String,
}

#[derive(Error, Debug)]
pub enum HardwareHashError {
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl HardwareInfo {
    #[cfg(windows)]
    pub fn new(version: u32) -> Self {
        use std::collections::HashMap;

        use log::warn;
        use wmi::{COMLibrary, FilterValue, WMIConnection};

        use crate::util::wmi_utils;

        let wmi_thread = std::thread::spawn(move || {
            let com_con = COMLibrary::new().unwrap();
            let wmi_con = WMIConnection::new(com_con).unwrap();

            let os_data: Vec<wmi_utils::Win32OperatingSystem> = wmi_con.query().unwrap();
            let bios_data: Vec<wmi_utils::Win32BIOS> = wmi_con.query().unwrap();
            let board_data: Vec<wmi_utils::Win32BaseBoard> = wmi_con.query().unwrap();
            let gpu_data: Vec<wmi_utils::Win32VideoController> = wmi_con.query().unwrap();
            let disk_data: Vec<wmi_utils::Win32DiskDrive> = wmi_con
                .filtered_query(&{
                    let mut filters = HashMap::new();
                    filters.insert(String::from("Index"), FilterValue::Number(0));

                    filters
                })
                .unwrap();
            Box::new((os_data, bios_data, board_data, gpu_data, disk_data))
        });

        let wmi_data = wmi_thread.join();
        if wmi_data.is_err() {
            warn!(
                "WMI call failed, using dummy hardware info. Please report this! {:?}",
                wmi_data.err().unwrap()
            );
            return Self::default();
        }

        let (os_data, bios_data, board_data, gpu_data, disk_data) = *wmi_data.unwrap();

        let mut board_manufacturer = "Microsoft Corporation";
        let mut board_sn = "None";
        if let Some(board_info) = board_data.get(0) {
            board_manufacturer = board_info.manufacturer.as_str();
            board_sn = board_info.serial_number.as_str();
        }

        let mut bios_manufacturer = "Microsoft Corporation";
        let mut bios_sn = "None";
        if let Some(bios_info) = bios_data.get(0) {
            bios_manufacturer = bios_info.manufacturer.as_str();
            bios_sn = bios_info.serial_number.as_str();
        }

        let mut os_install_date = "1970-01-0100:00:00.000000000+0000";
        let mut os_sn = "None";
        if let Some(os_info) = os_data.get(0) {
            os_install_date = os_info.install_date.as_str();
            os_sn = os_info.serial_number.as_str();
        }

        let mut disk_sn = "None";
        if let Some(disk_info) = disk_data.get(0) {
            disk_sn = disk_info.serial_number.as_str();
        }

        let volume_sn = format!("{:08x}", get_c_drive_volume_serial());

        let mut gpu_pnp_id: Option<String> = None;
        if let Some(gpu_info) = gpu_data.get(0) {
            let device_id = &gpu_info.pnp_device_id;
            let device_id =
                device_id[..device_id.rfind('\\').unwrap_or(device_id.len())].to_string();
            gpu_pnp_id = Some(device_id);
        }

        let mac = get_ea_mac_address();
        let cpu_details = Self::get_cpu_details();
        let hostname = gethostname().to_string_lossy().to_string();

        Self {
            version,
            bios_manufacturer: bios_manufacturer.to_owned(),
            bios_sn: bios_sn.to_owned(),
            board_manufacturer: board_manufacturer.to_owned(),
            board_sn: board_sn.to_owned(),
            os_install_date: os_install_date.to_owned(),
            os_sn: os_sn.to_owned(),
            disk_sn: disk_sn.to_owned(),
            volume_sn,
            gpu_pnp_id,
            mac,
            cpu_details,
            hostname,
        }
    }

    #[cfg(target_os = "linux")]
    pub fn new(version: u32) -> Self {
        use std::{fs, path::Path, process::Command};

        let board_manufacturer = match fs::read_to_string("/sys/class/dmi/id/board_vendor") {
            Ok(vendor) => vendor.trim().to_owned(),
            Err(_) => String::from("Linux Foundation"),
        };

        let board_sn = match fs::read_to_string("/var/lib/dbus/machine-id") {
            Ok(machine_id) => machine_id.trim().to_ascii_uppercase().to_owned(),
            Err(_) => String::from("None"),
        };
        let bios_manufacturer = match fs::read_to_string("/sys/class/dmi/id/bios_vendor") {
            Ok(vendor) => vendor.trim().to_owned(),
            Err(_) => String::from("Linux Foundation"),
        };

        let bios_sn = String::from("Serial number");
        let os_install_date = get_root_creation_str();
        let os_sn = String::from("00330-50000-00000-AAOEM");

        let mut gpu_pnp_id: Option<String> = None;
        let output = Command::new("lspci").args(["-Dd", "::0300"]).output();
        if let Ok(output) = output {
            if output.status.success() {
                let output = String::from_utf8_lossy(&output.stdout);
                let lines: Vec<&str> = output
                    .lines()
                    .take(1)
                    .map(|line| line.split_whitespace().next().unwrap_or_default())
                    .collect();

                if let Some(address) = lines.first() {
                    let path = format!("/sys/bus/pci/devices/{}", address);
                    let path_str = path.as_str();

                    if Path::new(path_str).exists() {
                        let vendor_id = read_file_hex_contents(format!("{}/{}", path, "vendor"));
                        let device_id = read_file_hex_contents(format!("{}/{}", path, "device"));
                        let rev_id = Some(0u16);

                        gpu_pnp_id =
                            Some(generate_pci_pnp_id(version, vendor_id, device_id, rev_id));
                    }
                }
            }
        }

        // TODO: Maybe, in the future, look for a good way to get the actual disk serial number
        // instead of using the partition UUID
        let mut disk_sn = String::from("None");
        let fstab = fs::read_to_string("/etc/fstab");
        if let Ok(fstab) = fstab {
            for line in fstab.lines() {
                // Skip comments and empty lines
                if !line.starts_with('#') && !line.is_empty() {
                    // Split the line into fields
                    let fields: Vec<&str> = line.split_whitespace().collect();

                    // Check if the line corresponds to the root filesystem ("/")
                    if fields.len() >= 2 && fields[1] == "/" {
                        // Extract the UUID
                        if let Some(uuid_field) =
                            fields.iter().find(|&&field| field.starts_with("UUID="))
                        {
                            disk_sn = uuid_field
                                .trim_start_matches("UUID=")
                                .trim_matches('"')
                                .to_owned();
                        }
                    }
                }
            }
        }

        let mac = get_ea_mac_address();
        let cpu_details = Self::get_cpu_details();
        let hostname = gethostname().to_string_lossy().to_string();

        Self {
            version,
            bios_manufacturer,
            bios_sn,
            board_manufacturer,
            board_sn,
            os_install_date,
            os_sn,
            disk_sn,
            volume_sn: String::from("43000000"),
            gpu_pnp_id,
            mac,
            cpu_details,
            hostname,
        }
    }

    #[cfg(target_os = "macos")]
    pub fn new(version: u32) -> Self {
        use std::process::Command;

        use smbioslib::{
            table_load_from_device, SMBiosBaseboardInformation, SMBiosSystemInformation,
        };

        use crate::util::system_profiler_utils::SPDisplaysDataType;

        // Apple Silicon Macs have no SMBIOS — fall back to the defaults
        // below; MAC address, disk UUID and hostname still make the hash
        // unique and stable per machine.
        let smbios_data = table_load_from_device().ok();
        let bios_data = smbios_data
            .as_ref()
            .and_then(|d| d.first::<SMBiosSystemInformation>());
        let board_data = smbios_data
            .as_ref()
            .and_then(|d| d.first::<SMBiosBaseboardInformation>());

        let mut board_manufacturer = String::from("Apple Inc.");
        let mut board_sn = String::from("None");
        if let Some(board) = board_data {
            board_manufacturer = board.manufacturer().to_string();
            board_sn = board.serial_number().to_string();
        }

        let mut bios_manufacturer = String::from("Apple Inc.");
        let mut bios_sn = String::from("None");
        if let Some(bios) = bios_data.as_ref() {
            bios_manufacturer = bios.manufacturer().to_string();
            bios_sn = bios.serial_number().to_string();
        }

        let os_install_date = get_root_creation_str();
        let mut os_sn = String::from("None");
        if let Some(uuid) = bios_data.and_then(|bios| bios.uuid()) {
            os_sn = uuid.to_string();
        }

        let mut gpu_pnp_id: Option<String> = None;
        let output = Command::new("system_profiler")
            .args(["SPDisplaysDataType", "-json"])
            .output()
            .unwrap();
        if output.status.success() {
            let json = String::from_utf8_lossy(&output.stdout);
            // Apple GPUs expose no PCI device/revision ids in this output —
            // parse failure just means no gpu_pnp_id, which the hash allows.
            let result: Option<SPDisplaysDataType> = serde_json::from_str(&json).ok();

            if let Some(gpu) = result.as_ref().and_then(|r| r.items.first()) {
                gpu_pnp_id = Some(generate_pci_pnp_id(
                    version,
                    None,
                    Some(gpu.device_id),
                    Some(gpu.revision_id),
                ));
            }
        }

        let mut disk_sn = String::from("None");
        let output = Command::new("diskutil")
            .args(["info", "/"])
            .output()
            .unwrap();
        // Check if the command was successful
        if output.status.success() {
            // Convert the output bytes to a UTF-8 string
            let output_str = String::from_utf8_lossy(&output.stdout);

            // Search for the line containing the serial number
            if let Some(uuid) = extract_diskutil_volume_uuid(&output_str) {
                disk_sn = uuid.to_owned();
            }
        }

        let mac = get_ea_mac_address();
        let cpu_details = Self::get_cpu_details();
        let hostname = gethostname().to_string_lossy().to_string();

        Self {
            version,
            bios_manufacturer,
            bios_sn,
            board_manufacturer,
            board_sn,
            os_install_date,
            os_sn,
            gpu_pnp_id,
            disk_sn,
            volume_sn: String::from("43000000"),
            mac,
            cpu_details,
            hostname,
        }
    }

    pub fn get_gpu_id(&self) -> u32 {
        let re = Regex::new(r"DEV_(\w+)").unwrap();

        match &self.gpu_pnp_id {
            Some(gpu_id) => match re.captures(gpu_id) {
                Some(captures) => captures
                    .get(1)
                    .map_or(0, |m| u32::from_str_radix(m.as_str(), 16).unwrap()),
                None => 0,
            },
            None => 0,
        }
    }

    pub fn get_cpu_details() -> CpuDetails {
        cpu::detect()
    }

    pub fn generate_mid(&self) -> Result<String, HardwareHashError> {
        let mut buffer = String::new();
        buffer += &self.board_manufacturer;
        buffer += &self.board_sn;
        buffer += &self.bios_manufacturer;
        buffer += &self.bios_sn;
        buffer += &self.os_install_date;
        buffer += &self.os_sn;

        if let Some(mac) = get_ea_mac_address() {
            buffer += mac.as_str();
        }

        Ok(hash_fnv1a(buffer.as_bytes()).to_string())
    }

    pub fn generate_hardware_hash(&self) -> String {
        let mut buffer: Vec<&str> = Vec::new();
        let gpu = self.gpu_pnp_id.clone().unwrap_or("None".to_string());
        let cpu_edx = format!("{:08x}", self.cpu_details.flags.edx);
        let cpu_edx_eax = format!(
            "{:08X}{:08X}",
            self.cpu_details.flags.edx, self.cpu_details.flags.eax
        );
        let cpu_ecx = format!("{:08x}", self.cpu_details.flags.ecx);

        buffer.push(&self.board_manufacturer);
        buffer.push(&self.board_sn);
        match self.version {
            0 | 1 => {
                buffer.push(&self.hostname);
                buffer.push(&self.bios_manufacturer);
                buffer.push(&self.bios_sn);
                buffer.push(&self.os_install_date);
                buffer.push(&self.os_sn)
            }
            2 => {
                buffer.push(&self.bios_manufacturer);
                buffer.push(&self.bios_sn);
                buffer.push(&self.os_install_date);
                buffer.push(&self.os_sn);
                buffer.push(&self.volume_sn);
                buffer.push(&gpu);
                buffer.push(&self.cpu_details.manufacturer);
                buffer.push(&cpu_edx);
                buffer.push(&cpu_ecx);
            }
            3 => {
                buffer.push(&self.bios_manufacturer);
                buffer.push(&self.bios_sn);
                buffer.push(&self.volume_sn);
                buffer.push(&gpu);
                buffer.push(&self.cpu_details.manufacturer);
                buffer.push(&cpu_edx);
                buffer.push(&cpu_ecx);
            }
            4_u32..=u32::MAX => {
                buffer.push(&self.bios_manufacturer);
                buffer.push(&self.bios_sn);
                buffer.push(&self.volume_sn);
                buffer.push(&gpu); // THIS IS EXTENDED IN generate_pci_pnp_id
                buffer.push(&self.cpu_details.manufacturer);
                buffer.push(&cpu_edx_eax);
            }
        }

        let mut final_data = buffer.join(";").to_string();
        final_data.push(';');
        if self.version >= 2 {
            final_data.push_str(&self.cpu_details.brand_name);
            final_data.push(';');
        }
        log::debug!("Hardware hash string \"{}\"", final_data);
        let digest = ring::digest::digest(&SHA1_FOR_LEGACY_USE_ONLY, final_data.as_bytes());
        if self.version < 4 {
            // they fucked up the format and used :x instead of :02x
            digest
                .as_ref()
                .iter()
                .map(|byte| format!("{:x}", byte))
                .collect::<Vec<String>>()
                .join("")
        } else {
            digest.encode_hex()
        }
    }
}

#[cfg(unix)]
fn get_root_creation_str() -> String {
    use crate::unix::wine::wine_prefix_dir;
    use chrono::{TimeZone, Utc};
    use std::{fs, os::unix::fs::MetadataExt};

    let date_str = String::from("1970010100:00:00.000000000+0000");
    let wine_prefix = wine_prefix_dir();
    if wine_prefix.is_err() {
        return date_str;
    }
    let wine_prefix = wine_prefix.unwrap();
    let date_str = match fs::metadata(wine_prefix.join("drive_c")) {
        Ok(metadata) => {
            let nsec = (metadata.mtime_nsec() / 1_000_000) * 1_000_000;
            // Convert Unix timestamp to a DateTime
            let datetime = Utc.timestamp_nanos((metadata.mtime() * 1_000_000_000) + nsec);
            // Format the DateTime
            return datetime.format("%Y%m%d%H%M%S%.6f+000").to_string();
        }
        Err(_) => date_str,
    };

    date_str
}

#[cfg(unix)]
fn generate_pci_pnp_id(
    version: u32,
    vendor: Option<u16>,
    device: Option<u16>,
    revision: Option<u16>,
) -> String {
    let mut sections = vec![];

    sections.push(format!("VEN_{:04X}", vendor.unwrap_or(0)));
    sections.push(format!("DEV_{:04X}", device.unwrap_or(0)));
    sections.push(format!("SUBSYS_{:08X}", 0));
    if version < 4 {
        sections.push(format!("REV_{:02X}", revision.unwrap_or(0)));
    } else {
        // TODO: check how this looks on windows
        sections.push(format!("REV_{:02X}\\0", revision.unwrap_or(0)));
        sections.push(format!("{:08X}", 0xDEADBEEFu32));
        sections.push(format!("{:X}", 0));
        sections.push(format!("{:04X}", 0xDEAD));
    }

    format!("PCI\\{}", sections.join("&"))
}

#[cfg(target_os = "linux")]
fn read_file_hex_contents(path: String) -> Option<u16> {
    use std::fs;

    match fs::read_to_string(path) {
        Ok(hex_str) => Some(u16::from_str_radix(&hex_str.trim()[2..], 16).unwrap()),
        Err(_) => None,
    }
}

#[cfg(target_os = "macos")]
fn extract_diskutil_volume_uuid(output: &str) -> Option<&str> {
    for line in output.lines() {
        if line.trim().starts_with("Volume UUID:") {
            // Extract the serial number from the line
            let parts: Vec<&str> = line.split_whitespace().collect();
            if let Some(uuid) = parts.get(2) {
                return Some(uuid);
            }
        }
    }
    None
}

#[cfg(target_os = "windows")]
fn get_c_drive_volume_serial() -> u32 {
    use std::ptr;
    use winapi::shared::minwindef::DWORD;
    use winapi::um::fileapi::GetVolumeInformationW;

    let mut serial: DWORD = 0;
    let res = unsafe {
        GetVolumeInformationW(
            "C:\\"
                .encode_utf16()
                .chain(Some(0))
                .collect::<Vec<u16>>()
                .as_ptr(),
            ptr::null_mut(),
            0,
            &mut serial,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            0,
        )
    };

    if res == 0 {
        return 0;
    }

    serial
}

fn get_ea_mac_address() -> Option<String> {
    let mac = match mac_address::get_mac_address() {
        Ok(addr) => addr,
        Err(_) => return None,
    };

    match mac {
        Some(address) => {
            let mac = hex::encode(&address.bytes());
            Some("$".to_owned() + &mac)
        }
        None => None,
    }
}

/// CPU identification, one implementation per architecture family.
///
/// * x86 / x86_64 read the real `cpuid` leaves, exactly what EA's
///   `Activation.dll` probes.
/// * aarch64 has no `cpuid`. The OS-provided CPU description is used
///   instead (macOS `sysctl`, Linux `/proc/cpuinfo` then `lscpu`, Windows
///   registry) and the register block is zero. The hash only needs to be
///   stable per machine, it cannot be made to equal an x86 hash.
///
/// Caveat (upstream issue 49): on Windows-on-ARM, and on Linux with
/// box64/FEX, the *game* runs x86_64-emulated, so its `Activation.dll` sees
/// the emulated `cpuid` and computes a different hardware hash than a
/// native-arm64 Maxima does. In those setups run the x86_64 Maxima build
/// under the same emulation so both sides probe the same CPU. macOS is
/// different: license validation was validated end to end with the native
/// arm64 hash.
mod cpu {
    use super::{CpuDetails, CpuidResult};

    /// Pure decode of the x86 `cpuid` leaves; `detect` feeds it live values.
    #[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
    pub(super) fn decode_x86(
        vendor: CpuidResult,
        features: CpuidResult,
        brand_leaves: [CpuidResult; 3],
    ) -> CpuDetails {
        let mut man = Vec::new();
        for val in [vendor.ebx, vendor.edx, vendor.ecx] {
            for b in val.to_ne_bytes() {
                man.push(b);
            }
        }

        let mut brand_name = Vec::with_capacity(47);
        'outer: for part in brand_leaves {
            for val in [part.eax, part.ebx, part.ecx, part.edx] {
                for b in val.to_ne_bytes() {
                    if b == 0 {
                        break 'outer;
                    }
                    brand_name.push(b);
                }
            }
        }
        brand_name.resize(47, 0);
        let brand_name = std::str::from_utf8(&brand_name)
            .unwrap_or("Unknown")
            .to_string();
        let manufacturer = std::str::from_utf8(&man).unwrap_or("Unknown").to_string();

        CpuDetails {
            flags: features,
            manufacturer,
            brand_name,
        }
    }

    #[cfg(target_arch = "x86_64")]
    pub(super) fn detect() -> CpuDetails {
        use core::arch::x86_64::__cpuid;

        // SAFETY: cpuid is available on every x86_64 CPU. (Safe fn on newer
        // toolchains, hence the allow.)
        #[allow(unused_unsafe)]
        let details = unsafe {
            decode_x86(
                __cpuid(0),
                __cpuid(1),
                [
                    __cpuid(0x80000002),
                    __cpuid(0x80000003),
                    __cpuid(0x80000004),
                ],
            )
        };
        details
    }

    #[cfg(not(target_arch = "x86_64"))]
    pub(super) fn detect() -> CpuDetails {
        detect_arm()
    }

    /// What the OS tells us about an ARM CPU.
    #[derive(Debug, Default, PartialEq, Eq)]
    #[cfg_attr(target_arch = "x86_64", allow(dead_code))]
    pub(super) struct ArmCpu {
        pub manufacturer: Option<String>,
        pub brand_name: Option<String>,
    }

    #[cfg_attr(target_arch = "x86_64", allow(dead_code))]
    fn arm_implementer_name(implementer: u32) -> Option<&'static str> {
        Some(match implementer {
            0x41 => "ARM",
            0x42 => "Broadcom",
            0x43 => "Cavium",
            0x46 => "Fujitsu",
            0x48 => "HiSilicon",
            0x4e => "NVIDIA",
            0x50 => "Applied Micro",
            0x51 => "Qualcomm",
            0x61 => "Apple",
            0x70 => "Phytium",
            0xc0 => "Ampere",
            _ => return None,
        })
    }

    #[cfg_attr(target_arch = "x86_64", allow(dead_code))]
    fn parse_hex(value: &str) -> Option<u32> {
        u32::from_str_radix(value.trim().trim_start_matches("0x"), 16).ok()
    }

    /// Parses the first processor block of an arm64 `/proc/cpuinfo`.
    /// Brand preference: `model name`, then `Hardware`, then a string built
    /// from `CPU implementer` / `CPU part` (arm64 kernels usually only have
    /// those).
    #[cfg_attr(target_arch = "x86_64", allow(dead_code))]
    pub(super) fn parse_proc_cpuinfo(text: &str) -> ArmCpu {
        let mut model_name = None;
        let mut hardware = None;
        let mut implementer = None;
        let mut part = None;
        let mut seen_processor = false;

        for line in text.lines() {
            let Some((key, value)) = line.split_once(':') else {
                if line.trim().is_empty() && seen_processor {
                    break;
                }
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            match key.to_ascii_lowercase().as_str() {
                "processor" => seen_processor = true,
                "model name" if model_name.is_none() && !value.is_empty() => {
                    model_name = Some(value.to_string())
                }
                "hardware" if hardware.is_none() && !value.is_empty() => {
                    hardware = Some(value.to_string())
                }
                "cpu implementer" if implementer.is_none() => implementer = parse_hex(value),
                "cpu part" if part.is_none() => part = parse_hex(value),
                _ => {}
            }
        }

        let manufacturer = implementer.and_then(arm_implementer_name).map(String::from);
        let brand_name = model_name.or(hardware).or_else(|| {
            implementer.map(|imp| match part {
                Some(part) => format!("ARM implementer 0x{imp:02x} part 0x{part:03x}"),
                None => format!("ARM implementer 0x{imp:02x}"),
            })
        });

        ArmCpu {
            manufacturer,
            brand_name,
        }
    }

    /// Parses `lscpu` output (`Vendor ID` / `Model name`).
    #[cfg_attr(target_arch = "x86_64", allow(dead_code))]
    pub(super) fn parse_lscpu(text: &str) -> ArmCpu {
        let mut cpu = ArmCpu::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            if value.is_empty() {
                continue;
            }
            match key.trim() {
                "Vendor ID" if cpu.manufacturer.is_none() => {
                    cpu.manufacturer = Some(value.to_string())
                }
                "Model name" if cpu.brand_name.is_none() => {
                    cpu.brand_name = Some(value.to_string())
                }
                _ => {}
            }
        }
        cpu
    }

    /// `HKLM\HARDWARE\DESCRIPTION\System\CentralProcessor\0`.
    #[cfg(windows)]
    #[cfg_attr(target_arch = "x86_64", allow(dead_code))]
    fn read_os_cpu() -> ArmCpu {
        use winreg::{enums::HKEY_LOCAL_MACHINE, RegKey};

        let key = RegKey::predef(HKEY_LOCAL_MACHINE)
            .open_subkey(r"HARDWARE\DESCRIPTION\System\CentralProcessor\0");
        let Ok(key) = key else {
            return ArmCpu::default();
        };
        let non_empty = |v: String| Some(v.trim().to_string()).filter(|v| !v.is_empty());
        ArmCpu {
            manufacturer: key
                .get_value::<String, _>("VendorIdentifier")
                .ok()
                .and_then(non_empty),
            brand_name: key
                .get_value::<String, _>("ProcessorNameString")
                .ok()
                .and_then(non_empty),
        }
    }

    #[cfg(target_os = "macos")]
    #[cfg_attr(target_arch = "x86_64", allow(dead_code))]
    fn read_os_cpu() -> ArmCpu {
        let brand_name = std::process::Command::new("sysctl")
            .args(["-n", "machdep.cpu.brand_string"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty());
        ArmCpu {
            manufacturer: Some("Apple".to_string()),
            brand_name,
        }
    }

    #[cfg(target_os = "linux")]
    #[cfg_attr(target_arch = "x86_64", allow(dead_code))]
    fn read_os_cpu() -> ArmCpu {
        let mut cpu = std::fs::read_to_string("/proc/cpuinfo")
            .map(|t| parse_proc_cpuinfo(&t))
            .unwrap_or_default();

        if cpu.brand_name.is_none() || cpu.manufacturer.is_none() {
            let lscpu = std::process::Command::new("lscpu")
                .env("LC_ALL", "C")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| parse_lscpu(&String::from_utf8_lossy(&o.stdout)))
                .unwrap_or_default();
            cpu.manufacturer = cpu.manufacturer.or(lscpu.manufacturer);
            cpu.brand_name = cpu.brand_name.or(lscpu.brand_name);
        }
        cpu
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    #[cfg_attr(target_arch = "x86_64", allow(dead_code))]
    fn read_os_cpu() -> ArmCpu {
        ArmCpu::default()
    }

    #[cfg(not(any(target_os = "macos", target_arch = "x86_64")))]
    fn warn_native_arm_caveat() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            log::warn!(
                "Running natively on aarch64: the hardware hash is computed from the ARM CPU \
                 description, not from cpuid. If the game runs x86_64-emulated here (Windows on \
                 ARM, box64/FEX), its Activation.dll sees the emulated cpuid and computes a \
                 different hash, so license validation can fail. Use the x86_64 Maxima build \
                 under the same emulation instead."
            );
        });
    }

    #[cfg(not(target_arch = "x86_64"))]
    fn detect_arm() -> CpuDetails {
        #[cfg(not(target_os = "macos"))]
        warn_native_arm_caveat();

        let cpu = read_os_cpu();
        CpuDetails {
            flags: CpuidResult {
                eax: 0,
                ebx: 0,
                ecx: 0,
                edx: 0,
            },
            manufacturer: cpu.manufacturer.unwrap_or_else(|| "ARM".to_string()),
            brand_name: cpu.brand_name.unwrap_or_else(|| {
                if cfg!(target_os = "macos") {
                    "Apple Silicon".to_string()
                } else {
                    "ARM Processor".to_string()
                }
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Guards the non-x86 path (Apple Silicon: no SMBIOS, no cpuid, no PCI GPU
    // ids): HardwareInfo::new must still produce a non-empty hash without
    // panicking.
    #[test]
    fn hardware_info_builds_without_panicking() {
        for version in [1, 2] {
            let info = HardwareInfo::new(version);
            assert!(!info.generate_hardware_hash().is_empty());
            assert!(!info.generate_mid().unwrap().is_empty());
        }
    }

    #[test]
    fn live_cpu_details_are_populated() {
        let cpu = HardwareInfo::get_cpu_details();
        assert!(!cpu.manufacturer.is_empty());
        assert!(!cpu.brand_name.is_empty());
        #[cfg(not(target_arch = "x86_64"))]
        assert_eq!(
            cpu.flags,
            CpuidResult {
                eax: 0,
                ebx: 0,
                ecx: 0,
                edx: 0
            }
        );
    }

    fn fixed_info(version: u32) -> HardwareInfo {
        HardwareInfo {
            version,
            board_manufacturer: "ASUSTeK COMPUTER INC.".into(),
            board_sn: "210123456789".into(),
            bios_manufacturer: "American Megatrends Inc.".into(),
            bios_sn: "Default string".into(),
            os_install_date: "20230415123456.000000+000".into(),
            os_sn: "00330-80000-00000-AA123".into(),
            disk_sn: "S4EVNX0R123456".into(),
            volume_sn: "a1b2c3d4".into(),
            gpu_pnp_id: Some("PCI\\VEN_10DE&DEV_2684&SUBSYS_00000000&REV_A1".into()),
            mac: None,
            cpu_details: CpuDetails {
                flags: CpuidResult {
                    eax: 0x000A0671,
                    ebx: 0,
                    ecx: 0x7FFAFBFF,
                    edx: 0xBFEBFBFF,
                },
                manufacturer: "GenuineIntel".into(),
                brand_name: "Intel(R) Core(TM) i9-11900K @ 3.50GHz".into(),
            },
            hostname: "DESKTOP-TEST".into(),
        }
    }

    // Pins the hash composition (field order, `{:x}` vs `{:02x}` quirk below
    // v4, brand-name suffix from v2) with fixed inputs. Expected values were
    // computed independently of the Rust code and match the pre-refactor
    // implementation, so x86_64 hashes must never change.
    #[test]
    fn hardware_hash_composition_is_pinned() {
        let expected = [
            (0, "1cf677e1aa2c96f952e627af7229e4448fd817"),
            (1, "1cf677e1aa2c96f952e627af7229e4448fd817"),
            (2, "d22d90fe3a1a7f18df429b7c4d5f98b432f35fc"),
            (3, "7b7fd8c1e78931cd3a75e8b4c94def0d991c"),
            (4, "534f21c4aaee7d0b73deb80dd56c68de21b2d327"),
            (5, "534f21c4aaee7d0b73deb80dd56c68de21b2d327"),
        ];
        for (version, hash) in expected {
            assert_eq!(fixed_info(version).generate_hardware_hash(), hash);
        }
    }

    // Pins the cpuid decoding with fixed registers ("GenuineIntel" is
    // EBX,EDX,ECX of leaf 0; the brand string is leaves 0x80000002..4 and is
    // NUL-padded to 47 bytes).
    #[test]
    fn x86_cpuid_decoding_is_pinned() {
        let reg = |s: &[u8; 4]| u32::from_ne_bytes(*s);
        let leaf = |a: &[u8; 4], b: &[u8; 4], c: &[u8; 4], d: &[u8; 4]| CpuidResult {
            eax: reg(a),
            ebx: reg(b),
            ecx: reg(c),
            edx: reg(d),
        };
        let vendor = CpuidResult {
            eax: 0x16,
            ebx: reg(b"Genu"),
            edx: reg(b"ineI"),
            ecx: reg(b"ntel"),
        };
        let features = CpuidResult {
            eax: 0x000A0671,
            ebx: 0x00100800,
            ecx: 0x7FFAFBFF,
            edx: 0xBFEBFBFF,
        };
        let brand = [
            leaf(b"Inte", b"l(R)", b" Cor", b"e(TM"),
            leaf(b") i9", b"-119", b"00K ", b"@ 3."),
            leaf(b"50GH", b"z\0\0\0", b"\0\0\0\0", b"\0\0\0\0"),
        ];

        let cpu = cpu::decode_x86(vendor, features, brand);
        assert_eq!(cpu.manufacturer, "GenuineIntel");
        assert_eq!(cpu.flags, features);
        let mut brand_name = String::from("Intel(R) Core(TM) i9-11900K @ 3.50GHz");
        while brand_name.len() < 47 {
            brand_name.push('\0');
        }
        assert_eq!(cpu.brand_name, brand_name);
    }

    #[test]
    fn arm_proc_cpuinfo_parsing() {
        let apple = "processor\t: 0\nBogoMIPS\t: 48.00\nFeatures\t: fp asimd\n\
                     CPU implementer\t: 0x61\nCPU architecture: 8\nCPU variant\t: 0x0\n\
                     CPU part\t: 0x023\nCPU revision\t: 0\n\nprocessor\t: 1\nCPU implementer\t: 0x41\n";
        let cpu = cpu::parse_proc_cpuinfo(apple);
        assert_eq!(cpu.manufacturer.as_deref(), Some("Apple"));
        assert_eq!(
            cpu.brand_name.as_deref(),
            Some("ARM implementer 0x61 part 0x023")
        );

        let named = "processor : 0\nmodel name : Neoverse-N1\nCPU implementer : 0x41\n\
                     CPU part : 0xd0c\n\nHardware : ignored second block\n";
        let cpu = cpu::parse_proc_cpuinfo(named);
        assert_eq!(cpu.manufacturer.as_deref(), Some("ARM"));
        assert_eq!(cpu.brand_name.as_deref(), Some("Neoverse-N1"));

        let hw = "Processor : AArch64 Processor rev 4 (aarch64)\nprocessor : 0\n\
                  CPU implementer : 0x51\nCPU part : 0x800\nHardware : Qualcomm Technologies, Inc SDM845\n";
        let cpu = cpu::parse_proc_cpuinfo(hw);
        assert_eq!(cpu.manufacturer.as_deref(), Some("Qualcomm"));
        assert_eq!(
            cpu.brand_name.as_deref(),
            Some("Qualcomm Technologies, Inc SDM845")
        );

        assert_eq!(cpu::parse_proc_cpuinfo(""), cpu::ArmCpu::default());
    }

    #[test]
    fn arm_lscpu_parsing() {
        let out = "Architecture:        aarch64\nVendor ID:           ARM\n\
                   Model name:          Cortex-A72\nModel:               3\n";
        let cpu = cpu::parse_lscpu(out);
        assert_eq!(cpu.manufacturer.as_deref(), Some("ARM"));
        assert_eq!(cpu.brand_name.as_deref(), Some("Cortex-A72"));
    }
}
