//! The permission catalogue: what an app can ask for, how much the user must be asked, and
//! what the sandbox does with it. Anything not listed here is refused at install time, so an
//! app built for a newer firmware cannot silently run with less isolation than it expects.

/// How a permission is decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// Granted with the install; shown in the install dialog for information.
    Normal,
    /// The user must allow it explicitly (install dialog); `appctl revoke` withdraws it.
    Sensitive,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Normal => "normal",
            Level::Sensitive => "sensitive",
        }
    }
}

pub struct Def {
    pub name: &'static str,
    /// What the sandbox does (README / `appctl info`).
    pub effect: &'static str,
}

/// Every known permission.
pub const CATALOGUE: &[Def] = &[
    Def {
        name: "display",
        effect: "while in the foreground: the DRM master fd (KMS) is brokered to the app, the GPU render node and a read-only /sys are mounted",
    },
    Def { name: "input", effect: "while in the foreground: remote-control/keyboard keys arrive on the broker socket" },
    Def {
        name: "media",
        effect: "the hardware video decoder (/dev/video*, /dev/media*) and the HDMI sound card's playback stream (/dev/snd/pcmC*D*p) appear in the app's /dev, owned by its uid",
    },
    Def {
        name: "storage",
        effect: "private /data (tmpfs of quota_kb, noexec), loaded from and saved to the flash data partition",
    },
    Def {
        name: "internet",
        effect: "HTTP(S) through the system network service (meow-net) to the hosts the manifest lists (\"*\" alone: any public host), never to the home network; without \"network\" or \"lan\" the app itself keeps an empty network namespace",
    },
    Def {
        name: "network",
        effect: "the device's network namespace (raw sockets: internet and LAN); without it the app gets an empty namespace with loopback only",
    },
    Def {
        name: "lan",
        effect: "the device's network namespace, for talking to devices on the home network: listening sockets, multicast discovery (SSDP, mDNS), UDP streams. Landlock lets the app bind only the fixed TCP and UDP ports its manifest lists (and ports the kernel picks); where it connects and sends is not confined (the kernel has no per-app packet filter)",
    },
    Def {
        name: "manage-receivers",
        effect: "the app may read and set the \"Run in background\" switch of the apps its signed manifest lists under \"manages\", through the broker; nothing else about them",
    },
    Def {
        name: "wifi-direct",
        effect: "may ask the system to be a Miracast (Wi-Fi Display) sink: the network daemon makes the device visible over Wi-Fi Direct under the name the app gives, lets a nearby source connect (push button or a PIN the app shows) and tells the app the source's address; the app gets no access to the radio itself",
    },
    Def {
        name: "manage-apps",
        effect: "the app is the device's app store: through the broker it sees every app of every repository, installs (recording the user's answer to the permission question it shows), uninstalls and opens apps, and adds, removes and switches the repositories apps come from. Only for apps of the built-in repository; such an app is installed with the system and cannot be removed",
    },
    Def {
        name: "background",
        effect: "a small second instance (the manifest's \"background\" section) runs while the app is closed, without screen, keys, decoder or sound; it can ask to open the app",
    },
];

/// Hosts an "internet" request may list.
pub const INTERNET_MAX_HOSTS: usize = 16;
/// The one entry that stands for every public host (the network service still refuses
/// addresses of the home network and of the device).
pub const ANY_HOST: &str = "*";

/// `example.com` or `*.example.com` (the host and everything below it), lower case.
pub fn valid_host_pattern(p: &str) -> bool {
    let name = p.strip_prefix("*.").unwrap_or(p);
    name.len() <= 253
        && name.contains('.')
        && name.split('.').all(|label| {
            !label.is_empty() && label.len() <= 63 && label.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') && !label.starts_with('-') && !label.ends_with('-')
        })
        && !name.split('.').all(|label| label.bytes().all(|b| b.is_ascii_digit()))
}

/// The hosts of an "internet" request for a person: "youtube.com, googlevideo.com and 1 more".
pub fn hosts_label(hosts: &[String]) -> String {
    if hosts.len() == 1 && hosts[0] == ANY_HOST {
        return "any site on the internet".to_string();
    }
    let mut names: Vec<&str> = Vec::new();
    for h in hosts {
        let name = h.strip_prefix("*.").unwrap_or(h);
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let shown = names.iter().take(3).copied().collect::<Vec<_>>().join(", ");
    if names.len() > 3 { format!("{shown} and {} more", names.len() - 3) } else { shown }
}

/// Storage up to this quota is a normal permission; more needs consent (flash is small).
pub const STORAGE_NORMAL_KB: u32 = 256;
/// Hard per-app storage limit.
pub const STORAGE_MAX_KB: u32 = 1024;

pub fn known(name: &str) -> bool {
    CATALOGUE.iter().any(|d| d.name == name)
}

pub fn def(name: &str) -> Option<&'static Def> {
    CATALOGUE.iter().find(|d| d.name == name)
}

pub fn level(name: &str, quota_kb: u32) -> Level {
    match name {
        "network" | "lan" | "internet" | "background" | "manage-receivers" | "wifi-direct" | "manage-apps" => Level::Sensitive,
        "storage" if quota_kb > STORAGE_NORMAL_KB => Level::Sensitive,
        _ => Level::Normal,
    }
}

/// One-line description for the consent dialog.
pub fn label(name: &str, quota_kb: u32) -> String {
    match name {
        "display" => "Show full-screen pictures and use the graphics chip while open".into(),
        "input" => "Receive remote-control keys while open".into(),
        "media" => "Play video and sound with the device's decoder and speakers".into(),
        "storage" => format!("Keep up to {quota_kb} KiB of its own data on this device"),
        "internet" => "Connect to its sites on the internet".into(),
        "network" => "Access the internet and devices on your network".into(),
        "lan" => "Find and talk to devices on your home network".into(),
        "background" => "Keep running in the background and open by itself".into(),
        "wifi-direct" => "Let nearby devices connect directly over Wi-Fi to show their screen".into(),
        "manage-apps" => "Install and remove apps, and choose where apps come from".into(),
        "manage-receivers" => "Switch background listening of your other cast apps on and off".into(),
        other => other.to_string(),
    }
}
