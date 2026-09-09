//! The on-disk configuration, and the defaults it falls back to.
//!
//! The defaults are measured, not guessed — see `README.md` for the numbers
//! this machine actually produces. Every field is optional, so an empty file is
//! a valid config that behaves exactly like no file at all.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::sysfs::Source;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// How often to sample and re-command. The SMC updates its temperatures
    /// about this fast, and a shorter interval only buys jitter.
    pub poll_interval_ms: u64,
    /// Ceiling on how fast the commanded speed may rise, in rpm per second.
    /// Generous by default: on this hardware a load step can put the package up
    /// 20 degrees inside five seconds, and the fan is already the slow part.
    pub ramp_up_rpm_per_s: f64,
    /// Ceiling on how fast it may fall. Much slower than the rise, because a
    /// fan that chases every dip in load is audible in a way a steady one is not.
    pub ramp_down_rpm_per_s: f64,
    /// Don't rewrite the fan for a change smaller than this.
    pub deadband_rpm: u32,
    /// Narrow the speed range the SMC reports. `None` uses the hardware bounds.
    pub min_rpm: Option<u32>,
    pub max_rpm: Option<u32>,
    /// A reading outside this range is treated as the sensor misreporting
    /// rather than as a real temperature. applesmc reports unpopulated sensors
    /// on this board as 0 or 1 degrees, which would otherwise read as "very cold"
    /// and hold the fan down.
    pub plausible_range_c: [f64; 2],
    /// How many consecutive bad samples to ride out on the last good reading
    /// before treating a sensor as failed.
    pub sensor_grace_polls: u32,
    #[serde(rename = "sensor")]
    pub sensors: Vec<SensorConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SensorConfig {
    pub source: Source,
    /// The sensor's label, e.g. `TPCD` or `Package id 0`.
    pub key: String,
    /// The temperature this sensor is steered towards. The fan sits at its
    /// minimum while the sensor is below target.
    pub target: f64,
    /// Above this, the fan goes straight to maximum, bypassing the controller.
    pub critical: f64,
    /// Proportional gain, in rpm per degree over target.
    #[serde(default = "default_kp")]
    pub kp: f64,
    /// Integral gain, in rpm per degree-second. This is what mbpfan lacks: it
    /// is the term that keeps pushing when a sensor parks a few degrees above
    /// target and a purely proportional response has already settled.
    #[serde(default = "default_ki")]
    pub ki: f64,
    /// Ceiling on how much the integral term alone may add, in rpm.
    ///
    /// This exists for sensors that airflow barely moves. On a MacBookPro9,2
    /// the PCH die sits at 82-88C whether the fan is at 3900 or 6200 rpm, so an
    /// uncapped integral on it would wind all the way up, pin the fan at
    /// maximum indefinitely, and buy about 3 degrees. Capping the integral lets
    /// such a sensor raise the fan proportionately without ever taking it over.
    /// `None` allows the full speed range. The `critical` threshold is
    /// unaffected either way, so a real emergency still goes straight to maximum.
    #[serde(default)]
    pub integral_limit_rpm: Option<u32>,
    /// A sensor that need not exist. Missing optional sensors are skipped with
    /// a note; a missing required sensor is a startup error, because silently
    /// steering on fewer inputs than the operator configured is how a fan
    /// daemon cooks a laptop.
    #[serde(default)]
    pub optional: bool,
}

fn default_kp() -> f64 {
    140.0
}

fn default_ki() -> f64 {
    30.0
}

impl Default for Config {
    fn default() -> Self {
        Config {
            poll_interval_ms: 1000,
            ramp_up_rpm_per_s: 3000.0,
            ramp_down_rpm_per_s: 200.0,
            deadband_rpm: 40,
            min_rpm: None,
            max_rpm: None,
            plausible_range_c: [5.0, 125.0],
            sensor_grace_polls: 5,
            sensors: default_sensors(),
        }
    }
}

/// The default sensor set for a MacBookPro9,2.
///
/// `critical` values are deliberately below the 105 degree `temp1_crit` the CPU
/// reports: by the time the hardware throttles itself the fan should have been
/// flat out for a while.
fn default_sensors() -> Vec<SensorConfig> {
    vec![
        // The CPU package. Target 80 keeps it clear of the 87 degree
        // temp1_max without running the fan up over ordinary desktop work.
        SensorConfig {
            source: Source::Coretemp,
            key: "Package id 0".into(),
            target: 80.0,
            critical: 95.0,
            kp: 140.0,
            ki: 30.0,
            integral_limit_rpm: None,
            optional: false,
        },
        // CPU proximity, on the heatsink side. Measured 62-78, and unlike TPCD
        // it does track both load and fan speed, so it is a useful second
        // opinion on the package sensor.
        SensorConfig {
            source: Source::Applesmc,
            key: "TC0P".into(),
            target: 75.0,
            critical: 95.0,
            kp: 140.0,
            ki: 30.0,
            integral_limit_rpm: None,
            optional: false,
        },
        // The PCH die. Measured 82-91: barely coupled to either load (+3C from
        // idle to a four-thread load) or airflow (-3C from 3900 to 6200 rpm),
        // but it does drift up over a long hot session. The target sits at the
        // top of that range, so it contributes only when the PCH is at its
        // hottest, and integral_limit_rpm bounds that contribution to +1500 rpm
        // over the floor however long it stays there. Lower it below about 85
        // and the fan pins at maximum permanently in exchange for 3 degrees.
        SensorConfig {
            source: Source::Applesmc,
            key: "TPCD".into(),
            target: 90.0,
            critical: 100.0,
            kp: 200.0,
            ki: 40.0,
            integral_limit_rpm: Some(1500),
            optional: false,
        },
        // The palm rest skin — the sensor that corresponds to the chassis
        // feeling hot, and the one this daemon exists to take notice of.
        // Measured 50-53 throughout, including under full load at maximum fan.
        // Same caveat as TPCD: it moved one degree when the fan went from 3900
        // to 6200 rpm, so the target sits above everything observed and the
        // integral is capped.
        SensorConfig {
            source: Source::Applesmc,
            key: "Ts0S".into(),
            target: 55.0,
            critical: 70.0,
            kp: 250.0,
            ki: 50.0,
            integral_limit_rpm: Some(1200),
            optional: false,
        },
        // Memory proximity. Measured 54, and not present on every model.
        SensorConfig {
            source: Source::Applesmc,
            key: "TM0P".into(),
            target: 75.0,
            critical: 95.0,
            kp: 120.0,
            ki: 25.0,
            integral_limit_rpm: None,
            optional: true,
        },
    ]
}

impl Config {
    pub fn load(path: &std::path::Path) -> Result<Config> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading the config at {}", path.display()))?;
        let config: Config = toml::from_str(&raw)
            .with_context(|| format!("parsing the config at {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn poll_interval(&self) -> Duration {
        Duration::from_millis(self.poll_interval_ms)
    }

    fn validate(&self) -> Result<()> {
        if self.poll_interval_ms == 0 {
            bail!("poll_interval_ms must be greater than 0");
        }
        // TOML has `nan` and `inf` literals, and every comparison below is
        // false against NaN — so an unchecked NaN passes validation whole and
        // then panics inside `clamp` during slew limiting, or silently disables
        // the steering or critical thresholds it was written into.
        for (name, value) in [
            ("ramp_up_rpm_per_s", self.ramp_up_rpm_per_s),
            ("ramp_down_rpm_per_s", self.ramp_down_rpm_per_s),
            ("plausible_range_c[0]", self.plausible_range_c[0]),
            ("plausible_range_c[1]", self.plausible_range_c[1]),
        ] {
            if !value.is_finite() {
                bail!("{name} must be a finite number (got {value})");
            }
        }
        if self.ramp_up_rpm_per_s <= 0.0 || self.ramp_down_rpm_per_s <= 0.0 {
            bail!("ramp_up_rpm_per_s and ramp_down_rpm_per_s must be greater than 0");
        }
        let [lo, hi] = self.plausible_range_c;
        if lo >= hi {
            bail!("plausible_range_c must be [low, high] with low < high (got [{lo}, {hi}])");
        }
        if let (Some(min), Some(max)) = (self.min_rpm, self.max_rpm)
            && min > max
        {
            bail!("min_rpm ({min}) is above max_rpm ({max})");
        }
        if self.sensors.is_empty() {
            bail!("no sensors configured; the fan would have nothing to steer on");
        }
        for s in &self.sensors {
            for (name, value) in
                [("target", s.target), ("critical", s.critical), ("kp", s.kp), ("ki", s.ki)]
            {
                if !value.is_finite() {
                    bail!("sensor {} has a non-finite {name} ({value})", s.key);
                }
            }
            if s.target >= s.critical {
                bail!(
                    "sensor {} target ({}) must be below its critical ({})",
                    s.key,
                    s.target,
                    s.critical
                );
            }
            if s.target < lo || s.critical > hi {
                bail!(
                    "sensor {} has target/critical ({}/{}) outside plausible_range_c ([{lo}, {hi}]), so it could never be steered on",
                    s.key,
                    s.target,
                    s.critical
                );
            }
            if s.integral_limit_rpm == Some(0) && s.ki > 0.0 {
                bail!(
                    "sensor {} has integral_limit_rpm = 0 with a non-zero ki; set ki = 0 instead to disable the integral",
                    s.key
                );
            }
            if s.kp < 0.0 || s.ki < 0.0 {
                bail!("sensor {} has a negative gain, which would cool by heating", s.key);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &str) -> Result<Config> {
        let config: Config = toml::from_str(body)?;
        config.validate()?;
        Ok(config)
    }

    #[test]
    fn the_defaults_validate() {
        Config::default().validate().expect("the built-in defaults must be a valid config");
    }

    #[test]
    fn non_finite_numbers_are_rejected() {
        // `nan` and `inf` are ordinary TOML floats, and NaN compares false
        // against every bound, so nothing else in validate() catches them.
        for body in [
            "ramp_up_rpm_per_s = nan",
            "ramp_down_rpm_per_s = inf",
            "plausible_range_c = [nan, 125.0]",
            "[[sensor]]\nsource = \"coretemp\"\nkey = \"Package id 0\"\ntarget = nan\ncritical = 95.0",
            "[[sensor]]\nsource = \"coretemp\"\nkey = \"Package id 0\"\ntarget = 80.0\ncritical = nan",
            "[[sensor]]\nsource = \"coretemp\"\nkey = \"Package id 0\"\ntarget = 80.0\ncritical = 95.0\nkp = nan",
            "[[sensor]]\nsource = \"coretemp\"\nkey = \"Package id 0\"\ntarget = 80.0\ncritical = 95.0\nki = -inf",
        ] {
            assert!(parse(body).is_err(), "should have been rejected: {body:?}");
        }
    }
}
