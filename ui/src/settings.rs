//! User settings: the persisted values (/data/ui/settings, key=value lines on the JFFS2 data
//! partition), the choices offered for them, and the live system facts the settings pages show.
//!
//! Everything here is cheap: a few small file reads, no processes spawned. Saving goes through
//! a background thread, because a JFFS2 write on the SPI-NOR can stall for a block erase.

use std::sync::mpsc;

/// Output modes the launcher can switch to (kms.rs names) and how they are shown.
pub const OUTPUTS: &[(&str, &str, &str)] = &[
    ("1080p60", "1920 × 1080 · 60 Hz", "Full HD. Smoothest motion"),
    ("4k30", "3840 × 2160 · 30 Hz", "4K. Sharper text, 30 Hz motion"),
    ("720p60", "1280 × 720 · 60 Hz", "HD. For older TVs"),
];

/// Idle time before the screen dims (minutes; 0 = never).
pub const DIMS: &[(u32, &str)] = &[
    (0, "Never"),
    (1, "After 1 minute"),
    (5, "After 5 minutes"),
    (15, "After 15 minutes"),
    (30, "After 30 minutes"),
];

/// Time zones as POSIX TZ strings (the firmware has no zoneinfo database): (places, UTC
/// offset label, TZ).
pub const ZONES: &[(&str, &str, &str)] = &[
    ("Honolulu", "UTC-10", "HST10"),
    ("Anchorage", "UTC-9", "AKST9AKDT,M3.2.0,M11.1.0"),
    ("Los Angeles, Vancouver", "UTC-8", "PST8PDT,M3.2.0,M11.1.0"),
    ("Denver, Calgary", "UTC-7", "MST7MDT,M3.2.0,M11.1.0"),
    ("Chicago, Mexico City", "UTC-6", "CST6CDT,M3.2.0,M11.1.0"),
    ("New York, Toronto", "UTC-5", "EST5EDT,M3.2.0,M11.1.0"),
    ("São Paulo, Buenos Aires", "UTC-3", "<-03>3"),
    ("UTC", "UTC", "UTC0"),
    ("London, Dublin, Lisbon", "UTC+0", "GMT0BST,M3.5.0/1,M10.5.0"),
    ("Berlin, Paris, Warsaw", "UTC+1", "CET-1CEST,M3.5.0,M10.5.0/3"),
    ("Athens, Helsinki, Kyiv", "UTC+2", "EET-2EEST,M3.5.0/3,M10.5.0/4"),
    ("Istanbul, Moscow", "UTC+3", "<+03>-3"),
    ("Dubai", "UTC+4", "<+04>-4"),
    ("Delhi, Mumbai", "UTC+5:30", "IST-5:30"),
    ("Bangkok, Jakarta", "UTC+7", "<+07>-7"),
    ("Beijing, Singapore", "UTC+8", "CST-8"),
    ("Tokyo, Seoul", "UTC+9", "JST-9"),
    ("Sydney, Melbourne", "UTC+10", "AEST-10AEDT,M10.1.0,M4.1.0/3"),
    ("Auckland", "UTC+12", "NZST-12NZDT,M9.5.0,M4.1.0/3"),
];

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// Output mode name (OUTPUTS).
    pub output: String,
    /// POSIX TZ string.
    pub tz: String,
    pub clock24: bool,
    /// Wallpaper depth: the background blurs and darkens below the top row.
    pub depth: bool,
    /// Minutes of no input before the screen dims (0 = never).
    pub dim_min: u32,
    /// Performance overlay (fps, late frames, GPU clock, temperature).
    pub hud: bool,
    /// The home wallpaper: an asset of the meow store (gallery.rs; Settings > Wallpaper).
    pub wallpaper: String,
    /// Set after a resolution change: the previous output, until the user keeps the new one.
    pub confirm: Option<String>,
}

pub fn path() -> String {
    std::env::var("UI_SETTINGS").unwrap_or_else(|_| "/data/ui/settings".into())
}

impl Settings {
    /// Defaults: the output this process was started with, TZ from the environment.
    pub fn defaults(output: &str) -> Settings {
        Settings {
            output: output.to_string(),
            tz: std::env::var("TZ").ok().filter(|t| !t.is_empty()).unwrap_or_else(|| "UTC0".into()),
            clock24: false,
            depth: true,
            dim_min: 0,
            hud: false,
            wallpaper: crate::gallery::DEFAULT.into(),
            confirm: None,
        }
    }

    /// Settings from the file, over `defaults`. Unknown keys are ignored, so older and newer
    /// launchers can share the file.
    pub fn load(defaults: Settings) -> (Settings, bool) {
        let Ok(text) = std::fs::read_to_string(path()) else { return (defaults, false) };
        let mut s = defaults;
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let v = v.trim();
            let on = v == "1" || v == "true";
            match k.trim() {
                "output" if OUTPUTS.iter().any(|o| o.0 == v) => s.output = v.into(),
                "tz" if !v.is_empty() => s.tz = v.into(),
                "clock24" => s.clock24 = on,
                "depth" => s.depth = on,
                "dim_min" => s.dim_min = v.parse().unwrap_or(0),
                "hud" => s.hud = on,
                "wallpaper" if crate::gallery::valid_asset(v) => s.wallpaper = v.into(),
                "confirm" if OUTPUTS.iter().any(|o| o.0 == v) => s.confirm = Some(v.into()),
                _ => {}
            }
        }
        (s, true)
    }

    fn serialise(&self) -> String {
        let b = |v: bool| if v { "1" } else { "0" };
        let mut out = format!(
            "# miracast-ui settings (written by the launcher)\noutput={}\ntz={}\nclock24={}\ndepth={}\ndim_min={}\nhud={}\nwallpaper={}\n",
            self.output, self.tz, b(self.clock24), b(self.depth), self.dim_min, b(self.hud), self.wallpaper
        );
        if let Some(c) = &self.confirm {
            out.push_str(&format!("confirm={c}\n"));
        }
        out
    }

    /// Write now, on this thread (used right before a restart).
    pub fn save_now(&self) -> Result<(), String> {
        write_atomic(&path(), &self.serialise())
    }

    /// Make TZ effective for localtime_r in this process.
    pub fn apply_tz(&self) {
        // SAFETY: only the render thread reads the environment after start-up (worker
        // threads do not call getenv/localtime).
        unsafe {
            std::env::set_var("TZ", &self.tz);
            tzset();
        }
    }

    pub fn zone_index(&self) -> Option<usize> {
        ZONES.iter().position(|z| z.2 == self.tz)
    }

    pub fn zone_label(&self) -> String {
        match self.zone_index() {
            Some(i) if ZONES[i].0 == ZONES[i].1 => ZONES[i].0.to_string(),
            Some(i) => format!("{} ({})", ZONES[i].0, ZONES[i].1),
            None => format!("Custom ({})", self.tz),
        }
    }

    pub fn output_label(&self) -> &'static str {
        output_label(&self.output)
    }

    pub fn dim_label(&self) -> String {
        DIMS.iter().find(|d| d.0 == self.dim_min).map_or_else(|| format!("After {} minutes", self.dim_min), |d| d.1.to_string())
    }
}

pub fn output_label(name: &str) -> &'static str {
    OUTPUTS.iter().find(|o| o.0 == name).map_or("Unknown", |o| o.1)
}

unsafe extern "C" {
    fn tzset();
}

fn write_atomic(path: &str, text: &str) -> Result<(), String> {
    let p = std::path::Path::new(path);
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let tmp = format!("{path}.new");
    std::fs::write(&tmp, text).map_err(|e| format!("{tmp}: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{path}: {e}"))
}

/// Saves settings off the render thread; only the newest pending value is written.
pub struct Saver {
    tx: mpsc::Sender<Settings>,
}

impl Saver {
    pub fn new() -> Saver {
        let (tx, rx) = mpsc::channel::<Settings>();
        std::thread::spawn(move || {
            unsafe { libc::setpriority(libc::PRIO_PROCESS, libc::gettid() as libc::id_t, 10) };
            while let Ok(mut s) = rx.recv() {
                while let Ok(newer) = rx.try_recv() {
                    s = newer;
                }
                match s.save_now() {
                    Ok(()) => eprintln!("UI_SETTINGS saved {}", path()),
                    Err(e) => eprintln!("UI_WARN settings: {e}"),
                }
            }
        });
        Saver { tx }
    }
    pub fn save(&self, s: &Settings) {
        let _ = self.tx.send(s.clone());
    }
}

/// Whether /data is a mount point yet (init mounts it after the launcher starts).
pub fn data_mounted() -> bool {
    std::fs::read_to_string("/proc/self/mounts").is_ok_and(|m| m.lines().any(|l| l.split_whitespace().nth(1) == Some("/data")))
}

/// Live facts for the Network, Wi-Fi and About pages (sampled every 2 s while shown).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SysInfo {
    pub iface: String,
    pub link_up: bool,
    pub ip: String,
    pub gateway: String,
    pub dns: String,
    pub mac: String,
    pub hostname: String,
    pub wifi: Vec<String>,
    pub kernel: String,
    pub cpus: usize,
    pub cpu_max_mhz: u32,
    pub gpu_mhz: u32,
    pub mem_total_mib: u32,
    pub mem_avail_mib: u32,
    pub data_free_kib: u64,
    pub data_total_kib: u64,
    pub data_volatile: bool,
    pub temp_c: f32,
    pub uptime_s: u64,
    pub time_synced: bool,
    pub ntp_running: bool,
}

fn read(path: &str) -> String {
    std::fs::read_to_string(path).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// Wireless interfaces (any netdev with a wireless/ or phy80211 entry).
pub fn wifi_adapters() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(dir) = std::fs::read_dir("/sys/class/net") {
        for e in dir.flatten() {
            let p = e.path();
            if p.join("wireless").exists() || p.join("phy80211").exists() {
                out.push(e.file_name().to_string_lossy().into_owned());
            }
        }
    }
    out.sort();
    out
}

fn process_running(name: &str) -> bool {
    let Ok(dir) = std::fs::read_dir("/proc") else { return false };
    dir.flatten().any(|e| {
        e.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()) && read(&format!("{}/comm", e.path().display())) == name
    })
}

pub fn sample() -> SysInfo {
    let mut s = SysInfo::default();
    for line in read("/run/net.env").lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim_matches('"').to_string();
        match k {
            "IFACE" => s.iface = v,
            "IP" => s.ip = v,
            "GATEWAY" => s.gateway = v,
            "DNS" => s.dns = v.split_whitespace().collect::<Vec<_>>().join(", "),
            _ => {}
        }
    }
    if s.iface.is_empty() {
        s.iface = "usb0".into();
    }
    s.link_up = read(&format!("/sys/class/net/{}/operstate", s.iface)) == "up" || read(&format!("/sys/class/net/{}/carrier", s.iface)) == "1";
    s.mac = read(&format!("/sys/class/net/{}/address", s.iface));
    s.hostname = read("/proc/sys/kernel/hostname");
    s.wifi = wifi_adapters();
    s.kernel = read("/proc/sys/kernel/osrelease");
    s.cpus = std::fs::read_dir("/sys/devices/system/cpu")
        .map(|d| d.flatten().filter(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n.starts_with("cpu") && n[3..].bytes().all(|b| b.is_ascii_digit()) && n.len() > 3
        }).count())
        .unwrap_or(1);
    s.cpu_max_mhz = read("/sys/devices/system/cpu/cpufreq/policy0/cpuinfo_max_freq").parse::<u32>().unwrap_or(0) / 1000;
    s.gpu_mhz = (read("/sys/class/devfreq/20000000.gpu/cur_freq").parse::<u64>().unwrap_or(0) / 1_000_000) as u32;
    for line in read("/proc/meminfo").lines() {
        let kib = |l: &str| l.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        if line.starts_with("MemTotal:") {
            s.mem_total_mib = (kib(line) / 1024) as u32;
        } else if line.starts_with("MemAvailable:") {
            s.mem_avail_mib = (kib(line) / 1024) as u32;
        }
    }
    unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c"/data".as_ptr(), &mut st) == 0 {
            s.data_free_kib = st.f_bavail as u64 * st.f_frsize as u64 / 1024;
            s.data_total_kib = st.f_blocks as u64 * st.f_frsize as u64 / 1024;
        }
    }
    s.data_volatile = std::path::Path::new("/run/data-volatile").exists() || !data_mounted();
    s.temp_c = read("/sys/class/thermal/thermal_zone0/temp").parse::<f32>().unwrap_or(0.0) / 1000.0;
    s.uptime_s = read("/proc/uptime").split('.').next().and_then(|v| v.parse().ok()).unwrap_or(0);
    // No RTC: before NTP the clock starts at the epoch (or the build date).
    s.time_synced = unsafe { libc::time(std::ptr::null_mut()) } > 1_767_225_600; // 2026-01-01
    s.ntp_running = process_running("ntpd");
    s
}

/// "3 h 12 min", "5 min", "2 d 4 h".
pub fn uptime_label(secs: u64) -> String {
    let (d, h, m) = (secs / 86_400, secs / 3600 % 24, secs / 60 % 60);
    if d > 0 {
        format!("{d} d {h} h")
    } else if h > 0 {
        format!("{h} h {m} min")
    } else {
        format!("{m} min")
    }
}

pub fn size_label(kib: u64) -> String {
    if kib >= 1024 { format!("{:.1} MiB", kib as f64 / 1024.0) } else { format!("{kib} KiB") }
}
