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
    /// The temperature at which this sensor starts asking for air. Below it
    /// the sensor demands the minimum; from here up to `critical` it demands a
    /// linear ramp across the whole speed range, so the demand rises with
    /// temperature instead of staying flat at the floor until `target`.
    ///
    /// Without this the daemon takes a fan the SMC had spun up and walks it
    /// down to the floor whenever every sensor happens to be a degree or two
    /// under target — steering *down* on the strength of sensors nothing else
    /// reads, which is the reverse of the point.
    ///
    /// Unset, it starts at the bottom of the plausible range, ramping across
    /// everything the sensor can legibly read. That is the most air any ramp
    /// can ask for, and deliberately so: an operator who has not said where a
    /// sensor should begin spinning up has not said it is safe to wait, and
    /// every other unknown here — a failed sensor, an implausible reading, a
    /// tachometer that will not read — already resolves towards air. It is
    /// louder than any measured configuration, so it is the kind of wrong that
    /// gets noticed and corrected rather than the kind that cooks a laptop.
    #[serde(default = "default_baseline_from")]
    pub baseline_from: f64,
    /// The temperature this sensor is steered towards. Past it the PI
    /// controller takes over from the baseline ramp wherever it asks for more.
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

/// The bottom of the plausible temperature range, and so the earliest a
/// baseline ramp can start. A reading below this is a misreport rather than a
/// temperature, so there is nothing colder for a ramp to begin at.
const PLAUSIBLE_LOW_C: f64 = 5.0;

fn default_baseline_from() -> f64 {
    PLAUSIBLE_LOW_C
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
            plausible_range_c: [PLAUSIBLE_LOW_C, 125.0],
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
        // temp1_max without running the fan up over ordinary desktop work, and
        // the baseline picks up from 60 so a package sitting in the seventies
        // is answered with air rather than with the floor.
        SensorConfig {
            source: Source::Coretemp,
            key: "Package id 0".into(),
            baseline_from: 60.0,
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
            baseline_from: 55.0,
            target: 75.0,
            critical: 95.0,
            kp: 140.0,
            ki: 30.0,
            integral_limit_rpm: None,
            optional: false,
        },
        // The PCH die. Measured 82-91: barely coupled to either load (+3C from
        // idle to a four-thread load) or airflow (-3C from 3900 to 6200 rpm),
        // but it does drift up over a long hot session. The baseline starts at
        // the bottom of that range, so the whole of it asks for progressively
        // more air — 89C, which used to ask for nothing at all, asks for about
        // 3900 rpm. The target stays at the top of the range and
        // integral_limit_rpm bounds the winding to +1500 rpm, because the
        // integral is the term that would otherwise pin the fan at maximum
        // indefinitely on a sensor airflow cannot reach.
        SensorConfig {
            source: Source::Applesmc,
            key: "TPCD".into(),
            baseline_from: 80.0,
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
        // to 6200 rpm, so the baseline is deliberately shallow — 50 to a
        // critical of 70 — the target sits above everything observed, and the
        // integral is capped.
        SensorConfig {
            source: Source::Applesmc,
            key: "Ts0S".into(),
            baseline_from: 50.0,
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
            baseline_from: 55.0,
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
            for (name, value) in [
                ("baseline_from", s.baseline_from),
                ("target", s.target),
                ("critical", s.critical),
                ("kp", s.kp),
                ("ki", s.ki),
            ] {
                if !value.is_finite() {
                    bail!("sensor {} has a non-finite {name} ({value})", s.key);
                }
            }
            if s.baseline_from >= s.target {
                bail!(
                    "sensor {} baseline_from ({}) must be below its target ({})",
                    s.key,
                    s.baseline_from,
                    s.target
                );
            }
            if s.target >= s.critical {
                bail!(
                    "sensor {} target ({}) must be below its critical ({})",
                    s.key,
                    s.target,
                    s.critical
                );
            }
            if s.baseline_from < lo {
                bail!(
                    "sensor {} has baseline_from ({}) below the floor of plausible_range_c ({lo}), so its ramp would start below any temperature it can legibly read; unset it defaults to {}",
                    s.key,
                    s.baseline_from,
                    PLAUSIBLE_LOW_C
                );
            }
            if s.critical > hi {
                bail!(
                    "sensor {} has critical ({}) above the ceiling of plausible_range_c ({hi}), so it could never be steered on",
                    s.key,
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

    /// One `[[sensor]]` table with the given body appended.
    fn sensor(body: &str) -> String {
        format!("[[sensor]]\nsource = \"coretemp\"\nkey = \"Package id 0\"\n{body}")
    }

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
            "ramp_up_rpm_per_s = nan".to_string(),
            "ramp_down_rpm_per_s = inf".to_string(),
            "plausible_range_c = [nan, 125.0]".to_string(),
            sensor("baseline_from = nan\ntarget = 80.0\ncritical = 95.0"),
            sensor("baseline_from = 60.0\ntarget = nan\ncritical = 95.0"),
            sensor("baseline_from = 60.0\ntarget = 80.0\ncritical = nan"),
            sensor("baseline_from = 60.0\ntarget = 80.0\ncritical = 95.0\nkp = nan"),
            sensor("baseline_from = 60.0\ntarget = 80.0\ncritical = 95.0\nki = -inf"),
        ] {
            assert!(parse(&body).is_err(), "should have been rejected: {body:?}");
        }
    }

    #[test]
    fn a_baseline_that_starts_at_or_above_target_is_rejected() {
        // The ramp runs baseline_from -> critical and the controller takes over
        // at target; a baseline starting at or past the target would invert
        // that, and an empty or negative span would divide by zero.
        assert!(parse(&sensor("baseline_from = 80.0\ntarget = 80.0\ncritical = 95.0")).is_err());
        assert!(parse(&sensor("baseline_from = 90.0\ntarget = 80.0\ncritical = 95.0")).is_err());
        assert!(parse(&sensor("baseline_from = 60.0\ntarget = 80.0\ncritical = 95.0")).is_ok());
    }

    #[test]
    fn a_baseline_below_the_plausible_range_is_rejected() {
        // Its ramp would start below anything the sensor can legibly read, so
        // it would arrive already partly wound up with nothing to say why.
        assert!(parse(&sensor("baseline_from = 2.0\ntarget = 80.0\ncritical = 95.0")).is_err());
    }

    #[test]
    fn a_sensor_that_names_no_baseline_ramps_from_the_bottom_of_the_plausible_range() {
        // The one unknown that must not resolve to silence. An operator who has
        // not said where this sensor should spin up gets the earliest ramp
        // there is — more air than any measured configuration asks for, never
        // less.
        let config = parse(&sensor("target = 80.0\ncritical = 95.0")).expect("valid");
        assert_eq!(config.sensors[0].baseline_from, PLAUSIBLE_LOW_C);
    }

    #[test]
    fn raising_the_plausible_floor_past_the_default_baseline_is_a_startup_error() {
        // The default only makes sense against the default floor. Raising one
        // without the other is caught and named rather than silently clamped.
        let body = format!("plausible_range_c = [40.0, 125.0]\n{}", sensor("target = 80.0\ncritical = 95.0"));
        assert!(parse(&body).is_err());
    }
}
