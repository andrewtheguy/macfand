//! Sysfs access for the Apple SMC (`applesmc`) and the CPU package sensors
//! (`coretemp`).
//!
//! Everything here is addressed by *label* — `TPCD`, `Package id 0` — rather
//! than by the `tempN_input` file it happens to live in. The numbering is
//! assigned in probe order and is not stable across kernels or even reboots, so
//! a config that named `temp18_input` would silently start steering the fan
//! from the wrong sensor after an upgrade.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

/// Where the platform devices live. A field rather than a constant so the tests
/// can point at a fixture tree.
const PLATFORM: &str = "/sys/devices/platform";

/// Which driver a sensor is read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// The SMC: chassis, proximity, battery and memory sensors.
    Applesmc,
    /// The CPU's own on-die sensors.
    Coretemp,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Applesmc => "applesmc",
            Source::Coretemp => "coretemp",
        }
    }
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The `tempN_input` files of one driver, keyed by their trimmed label.
pub struct Temps {
    pub dir: PathBuf,
    by_label: BTreeMap<String, PathBuf>,
}

impl Temps {
    /// Locate a driver's platform device and index its labelled sensors.
    pub fn discover(source: Source) -> Result<Temps> {
        let dir = find_platform_device(source.as_str())
            .with_context(|| format!("locating the {source} platform device"))?;
        let mut by_label = BTreeMap::new();
        // coretemp hangs its sensors off a hwmon child; applesmc puts them in
        // the platform directory itself. Scan both rather than special-casing.
        for d in [dir.clone()].into_iter().chain(hwmon_children(&dir)) {
            index_labels(&d, &mut by_label);
        }
        if by_label.is_empty() {
            bail!("{source} exposed no labelled temperature sensors under {}", dir.display());
        }
        Ok(Temps { dir, by_label })
    }

    pub fn path(&self, label: &str) -> Option<&Path> {
        self.by_label.get(label).map(PathBuf::as_path)
    }

    pub fn labels(&self) -> impl Iterator<Item = (&str, &Path)> {
        self.by_label.iter().map(|(k, v)| (k.as_str(), v.as_path()))
    }
}

/// One fan, with the SMC's own idea of how fast it is allowed to spin.
pub struct Fan {
    pub label: String,
    pub input: PathBuf,
    pub output: PathBuf,
    /// `fanN_manual` in sysfs: 1 while software drives the fan, 0 while the
    /// SMC's own curve does.
    pub control_mode: PathBuf,
    pub hw_min: u32,
    pub hw_max: u32,
}

impl Fan {
    pub fn rpm(&self) -> Result<u32> {
        read_u32(&self.input)
    }

    /// Command a speed. The SMC clamps to its own bounds, but we clamp too so
    /// what we log is what the hardware was actually asked for.
    pub fn set_rpm(&self, rpm: u32) -> Result<()> {
        let rpm = rpm.clamp(self.hw_min, self.hw_max);
        write_str(&self.output, &rpm.to_string())
            .with_context(|| format!("setting {} to {rpm} rpm", self.label))
    }

    /// Hand the fan to macfand, or give it back to the SMC's own curve.
    pub fn set_software_control(&self, software: bool) -> Result<()> {
        write_str(&self.control_mode, if software { "1" } else { "0" })
            .with_context(|| format!("handing {} {}", self.label, if software { "to macfand" } else { "back to the SMC" }))
    }
}

/// Every fan the SMC exposes, in index order.
pub fn discover_fans(applesmc: &Path) -> Result<Vec<Fan>> {
    let mut fans = Vec::new();
    for entry in fs::read_dir(applesmc).with_context(|| format!("reading {}", applesmc.display()))? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        let Some(index) = name.strip_prefix("fan").and_then(|r| r.strip_suffix("_output")) else {
            continue;
        };
        let p = |suffix: &str| applesmc.join(format!("fan{index}{suffix}"));
        // fanN_min and fanN_max are what the SMC will accept; going outside
        // them is either ignored or stalls the fan, so they are the real bounds
        // and the config can only narrow them. Neither may be defaulted: a
        // guessed minimum of 0 is a licence to command 0 rpm and stall the fan,
        // which is the one thing these bounds exist to prevent.
        let hw_min = read_u32(&p("_min"))
            .with_context(|| format!("reading the minimum speed of fan {index}"))?;
        let hw_max = read_u32(&p("_max"))
            .with_context(|| format!("reading the maximum speed of fan {index}"))?;
        if hw_min == 0 {
            bail!("fan {index} reports a minimum speed of 0 rpm");
        }
        if hw_max == 0 {
            bail!("fan {index} reports a maximum speed of 0 rpm");
        }
        let label = fs::read_to_string(p("_label"))
            .map(|s| s.trim().to_string())
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("fan{index}"));
        fans.push(Fan {
            label,
            input: p("_input"),
            output: p("_output"),
            control_mode: p("_manual"),
            hw_min,
            hw_max,
        });
    }
    if fans.is_empty() {
        bail!("no fans found under {} — is this an Apple machine with applesmc loaded?", applesmc.display());
    }
    fans.sort_by(|a, b| a.output.cmp(&b.output));
    Ok(fans)
}

/// Read a `tempN_input`, in degrees celsius.
///
/// The kernel reports millidegrees. applesmc will also happily report a sensor
/// that is not populated on this board as 0, so the caller is expected to sanity
/// check the range rather than trust everything that parses.
pub fn read_temp(path: &Path) -> Result<f64> {
    Ok(read_i64(path)? as f64 / 1000.0)
}

fn find_platform_device(prefix: &str) -> Result<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(PLATFORM)
        .with_context(|| format!("reading {PLATFORM}"))?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name == prefix || name.starts_with(&format!("{prefix}."))
        })
        .map(|e| e.path())
        .collect();
    found.sort();
    found.into_iter().next().ok_or_else(|| {
        anyhow!("no {prefix} device under {PLATFORM}; is the {prefix} module loaded? (modprobe {prefix})")
    })
}

fn hwmon_children(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir.join("hwmon")) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    out.sort();
    out
}

fn index_labels(dir: &Path, out: &mut BTreeMap<String, PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(index) = name.strip_prefix("temp").and_then(|r| r.strip_suffix("_label")) else {
            continue;
        };
        let input = dir.join(format!("temp{index}_input"));
        if !input.exists() {
            continue;
        }
        // applesmc pads its labels out to a fixed width.
        let Ok(label) = fs::read_to_string(entry.path()) else {
            continue;
        };
        let label = label.trim();
        if !label.is_empty() {
            out.insert(label.to_string(), input);
        }
    }
}

fn read_i64(path: &Path) -> Result<i64> {
    let raw = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    raw.trim()
        .parse()
        .with_context(|| format!("parsing {} as a number (got {:?})", path.display(), raw.trim()))
}

fn read_u32(path: &Path) -> Result<u32> {
    let v = read_i64(path)?;
    u32::try_from(v).with_context(|| format!("{} reported a negative value ({v})", path.display()))
}

fn write_str(path: &Path, value: &str) -> Result<()> {
    // applesmc serialises access to the SMC and returns EBUSY when the firmware
    // is mid-transaction, which happens often enough on a machine this old to
    // matter. One retry turns a daemon exit into a skipped write.
    match fs::write(path, value) {
        Ok(()) => Ok(()),
        Err(first) => fs::write(path, value)
            .with_context(|| format!("writing {value:?} to {} (first attempt: {first})", path.display())),
    }
}
