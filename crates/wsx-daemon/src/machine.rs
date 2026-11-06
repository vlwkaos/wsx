//! wsxd's built-in machine panel `this-mac`: this machine's memory, swap, load and volumes, read
//! with system calls into a panel document. No project and no command: the clients draw it like a
//! `format: json` panel, first on the Machine tab and in the TUI's `M`. A reading the system does
//! not give is left out, never an error.
//! ref: docs/panels.md "Machine panels"

use std::ffi::{c_int, CStr};
use wsx_core::panel::{
    Item, Level, Num, PanelDoc, PanelInfo, PanelRun, PanelScope, PanelStatus, Section,
};

pub(crate) const NAME: &str = "this-mac";
pub(crate) const REFRESH_SECS: u64 = 60;
#[cfg(target_os = "macos")]
const TITLE: &str = "This Mac";
#[cfg(not(target_os = "macos"))]
const TITLE: &str = "This machine";

// ^ Every level threshold, as docs/panels.md "Machine panels" lists them: a share at or above
// `warn` is `warn`, at or above `bad` is `bad`.
/// Memory used (app, wired and compressed, as Activity Monitor counts it) of the RAM.
#[cfg(target_os = "macos")]
const MEMORY: (f64, f64) = (0.85, 0.95);
/// Swap used, as a share of the RAM (the swap total grows with use, so its own share says little).
#[cfg(target_os = "macos")]
const SWAP: (f64, f64) = (0.25, 0.5);
/// The 1-minute load average per logical core.
const LOAD: (f64, f64) = (1.0, 2.0);
/// A volume's used share.
const VOLUME: (f64, f64) = (0.9, 0.95);
/// A smaller volume is not shown (a recovery or a firmware partition).
const VOLUME_MIN_BYTES: u64 = 1_000_000_000;
/// ^ The panel document's item limit per section (docs/panels.md "JSON panels").
const MAX_VOLUMES: usize = 50;

/// Memory and swap in GB of 1024³ (as Activity Monitor), disks in GB of 10⁹ (as Finder).
#[cfg(target_os = "macos")]
const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
const GB: f64 = 1e9;

extern "C" {
    // ^ POSIX on macOS and Linux; libc declares it for Linux only.
    fn getloadavg(loadavg: *mut f64, nelem: c_int) -> c_int;
}

pub(crate) fn info() -> PanelInfo {
    PanelInfo {
        name: NAME.into(),
        refresh: REFRESH_SECS,
        scope: PanelScope::Machine,
    }
}

fn level(share: f64, (warn, bad): (f64, f64)) -> Level {
    if share >= bad {
        Level::Bad
    } else if share >= warn {
        Level::Warn
    } else {
        Level::Ok
    }
}

/// One decimal: 13.6 GB.
fn tenths(value: f64) -> Num {
    Num((value * 10.0).round() / 10.0)
}

fn gauge(label: String, used: f64, total: f64, unit: f64, level: Level) -> Item {
    Item::Gauge {
        label,
        used: tenths(used / unit),
        total: tenths(total / unit),
        unit: Some("GB".into()),
        level: Some(level),
    }
}

/// Read the machine now: a few system calls, no disk scan.
pub(crate) fn read() -> PanelRun {
    let mut sections = Vec::new();
    #[cfg(target_os = "macos")]
    if let Some(items) = mac::memory() {
        sections.push(Section {
            title: Some("Memory".into()),
            items,
        });
    }
    if let Some(item) = load() {
        sections.push(Section {
            title: Some("Load".into()),
            items: vec![item],
        });
    }
    let volumes: Vec<Item> = volumes()
        .into_iter()
        .take(MAX_VOLUMES)
        .map(|(label, used, total)| gauge(label, used, total, GB, level(used / total, VOLUME)))
        .collect();
    if !volumes.is_empty() {
        sections.push(Section {
            title: Some("Disks".into()),
            items: volumes,
        });
    }
    let doc = PanelDoc {
        title: Some(TITLE.into()),
        sections,
    };
    PanelRun {
        status: PanelStatus::Ok,
        // ^ The document as text, for a client that draws only `output`.
        output: serde_json::to_string(&doc).unwrap_or_default(),
        truncated: false,
        exit: None,
        finished_unix_ms: super::unix_time_millis(),
        doc: Some(doc),
        notice: None,
    }
}

fn load() -> Option<Item> {
    let mut load = [0f64; 3];
    // SAFETY: room for the 3 values asked for.
    if unsafe { getloadavg(load.as_mut_ptr(), 3) } != 3 {
        return None;
    }
    let cores = std::thread::available_parallelism().map_or(1, usize::from);
    Some(Item::Stat {
        label: "Load average".into(),
        value: format!("{:.2}, {:.2}, {:.2}", load[0], load[1], load[2]),
        note: Some(format!("1, 5, 15 min, {cores} cores")),
        level: Some(level(load[0] / cores as f64, LOAD)),
    })
}

/// (label, used bytes, total bytes) of each volume to show, the boot volume first.
#[cfg(not(target_os = "macos"))]
fn volumes() -> Vec<(String, f64, f64)> {
    // ^ Portable: the root file system only.
    // SAFETY: a zeroed out-parameter the call fills.
    let mut fs: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c"/".as_ptr(), &mut fs) } != 0 {
        return Vec::new();
    }
    let unit = fs.f_frsize as f64;
    let total = fs.f_blocks as f64 * unit;
    let used = total - fs.f_bavail as f64 * unit;
    if total < VOLUME_MIN_BYTES as f64 {
        return Vec::new();
    }
    vec![("/".into(), used, total)]
}

#[cfg(target_os = "macos")]
fn volumes() -> Vec<(String, f64, f64)> {
    mac::volumes()
}

#[cfg(target_os = "macos")]
mod mac {
    use super::*;
    use std::ffi::c_void;
    use std::sync::OnceLock;

    /// A sysctl value of a fixed size.
    fn sysctl<T: Copy>(name: &CStr) -> Option<T> {
        // SAFETY: a zeroed plain-data out-parameter whose size the call checks.
        let mut value: T = unsafe { std::mem::zeroed() };
        let mut size = std::mem::size_of::<T>();
        let status = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                (&mut value as *mut T).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        (status == 0 && size == std::mem::size_of::<T>()).then_some(value)
    }

    fn host() -> libc::mach_port_t {
        // ^ Each mach_host_self() adds a reference to the port: ask once in the daemon's life.
        static HOST: OnceLock<libc::mach_port_t> = OnceLock::new();
        #[allow(deprecated)]
        // SAFETY: no arguments; the port stays valid for the process.
        *HOST.get_or_init(|| unsafe { libc::mach_host_self() })
    }

    pub(super) fn memory() -> Option<Vec<Item>> {
        let total = sysctl::<u64>(c"hw.memsize")? as f64;
        // SAFETY: the out-parameter is HOST_VM_INFO64's struct and the count its size.
        let mut stats: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
        let mut count = libc::HOST_VM_INFO64_COUNT;
        let status = unsafe {
            libc::host_statistics64(
                host(),
                libc::HOST_VM_INFO64,
                (&mut stats as *mut libc::vm_statistics64).cast(),
                &mut count,
            )
        };
        if status != libc::KERN_SUCCESS || total <= 0.0 {
            return None;
        }
        // SAFETY: no arguments.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(0) as f64;
        let pages = |count: u64| count as f64 * page;
        let app = pages(u64::from(
            stats
                .internal_page_count
                .saturating_sub(stats.purgeable_count),
        ));
        let wired = pages(u64::from(stats.wire_count));
        let compressed = pages(u64::from(stats.compressor_page_count));
        let used = (app + wired + compressed).min(total);
        let mut items = vec![gauge(
            "Memory".into(),
            used,
            total,
            GIB,
            level(used / total, MEMORY),
        )];
        // ^ The kernel's own level, the one `memory_pressure` prints.
        if let Some((word, level)) = match sysctl::<i32>(c"kern.memorystatus_vm_pressure_level") {
            Some(1) => Some(("normal", Level::Ok)),
            Some(2) => Some(("warning", Level::Warn)),
            Some(4) => Some(("critical", Level::Bad)),
            _ => None,
        } {
            items.push(Item::Stat {
                label: "Pressure".into(),
                value: word.into(),
                note: None,
                level: Some(level),
            });
        }
        let stored = pages(stats.total_uncompressed_pages_in_compressor);
        items.push(Item::Stat {
            label: "Compressed".into(),
            value: format!("{:.1} GB", compressed / GIB),
            note: Some(format!("holds {:.1} GB", stored / GIB)),
            level: None,
        });
        if let Some(swap) = sysctl::<libc::xsw_usage>(c"vm.swapusage") {
            let used = swap.xsu_used as f64;
            let level = level(used / total, SWAP);
            items.push(if swap.xsu_total > 0 {
                gauge("Swap".into(), used, swap.xsu_total as f64, GIB, level)
            } else {
                Item::Stat {
                    label: "Swap".into(),
                    value: "none".into(),
                    note: None,
                    level: Some(level),
                }
            });
        }
        Some(items)
    }

    /// The volume's name as Finder shows it ("Macintosh HD").
    fn volume_name(mount: &CStr) -> Option<String> {
        let mut list = libc::attrlist {
            bitmapcount: libc::ATTR_BIT_MAP_COUNT,
            reserved: 0,
            commonattr: 0,
            volattr: libc::ATTR_VOL_INFO | libc::ATTR_VOL_NAME,
            dirattr: 0,
            fileattr: 0,
            forkattr: 0,
        };
        // ^ The answer: its u32 length, an attrreference_t, then the name.
        #[repr(C, align(8))]
        struct Answer([u8; 1024]);
        let mut answer = Answer([0; 1024]);
        // SAFETY: the list and a buffer of the size passed.
        let status = unsafe {
            libc::getattrlist(
                mount.as_ptr(),
                (&mut list as *mut libc::attrlist).cast::<c_void>(),
                answer.0.as_mut_ptr().cast(),
                answer.0.len(),
                0,
            )
        };
        if status != 0 {
            return None;
        }
        let bytes = &answer.0;
        let word = |at: usize| u32::from_ne_bytes(bytes[at..at + 4].try_into().unwrap());
        let length = (word(0) as usize).min(bytes.len());
        // ^ The offset counts from the attrreference_t itself, at byte 4.
        let start = 4usize.checked_add_signed(word(4) as i32 as isize)?;
        let end = start.checked_add(word(8) as usize)?;
        let name = CStr::from_bytes_until_nul(bytes.get(start..end.min(length))?).ok()?;
        let name = name.to_string_lossy().trim().to_string();
        (!name.is_empty()).then_some(name)
    }

    pub(super) fn volumes() -> Vec<(String, f64, f64)> {
        // SAFETY: a null buffer asks only for the count.
        let count = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
        if count <= 0 {
            return Vec::new();
        }
        // ^ Room for a mount that appears between the two calls.
        let mut table: Vec<libc::statfs> = vec![unsafe { std::mem::zeroed() }; count as usize + 4];
        let bytes = (table.len() * std::mem::size_of::<libc::statfs>()) as c_int;
        // SAFETY: the buffer holds `table.len()` entries; MNT_NOWAIT never waits on a disk.
        let count = unsafe { libc::getfsstat(table.as_mut_ptr(), bytes, libc::MNT_NOWAIT) };
        table.truncate(count.max(0) as usize);
        let mut shown: Vec<(bool, String, String, f64, f64)> = table
            .iter()
            .filter_map(|fs| {
                // SAFETY: the kernel ends the mount point with a NUL.
                let mount = unsafe { CStr::from_ptr(fs.f_mntonname.as_ptr()) };
                let path = mount.to_string_lossy().into_owned();
                let flags = fs.f_flags as c_int;
                let boot = path == "/";
                // ^ `/` is a read-only snapshot of the system volume, but its numbers are the
                // APFS container's, the Data volume included: it stands for the boot disk.
                let hidden = flags & libc::MNT_LOCAL == 0
                    || flags & (libc::MNT_RDONLY | libc::MNT_SNAPSHOT | libc::MNT_DONTBROWSE) != 0
                    || path.starts_with("/System/Volumes/");
                if !boot && hidden {
                    return None;
                }
                let unit = f64::from(fs.f_bsize);
                let total = fs.f_blocks as f64 * unit;
                if total < VOLUME_MIN_BYTES as f64 {
                    return None;
                }
                let used = total - fs.f_bavail as f64 * unit;
                let label = volume_name(mount).unwrap_or_else(|| {
                    path.rsplit('/')
                        .find(|part| !part.is_empty())
                        .unwrap_or("/")
                        .to_string()
                });
                Some((!boot, path, label.chars().take(64).collect(), used, total))
            })
            .collect();
        shown.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        shown
            .into_iter()
            .map(|(_, _, label, used, total)| (label, used, total))
            .collect()
    }
}
