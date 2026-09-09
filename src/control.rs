//! The control policy: one PI controller per sensor, and the fan runs at
//! whichever of them is asking for the most.
//!
//! Taking the maximum rather than an average is the whole point of the design.
//! A daemon that averages, or that reads only the CPU package, will sit at a
//! comfortable speed while some other sensor bakes — which is exactly the
//! failure mode this replaces.

use std::path::{Path, PathBuf};

use crate::config::{Config, SensorConfig};
use crate::sysfs::read_temp;

/// Why the fan is at the speed it is. Carried out to the log so a surprising
/// fan speed can always be traced to the sensor that asked for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// Every sensor is below the temperature at which it starts asking for air.
    Idle,
    /// A sensor is above target and its controller is asking for this speed.
    Steering(String),
    /// A sensor is above its critical threshold.
    Critical(String),
    /// A sensor stopped reading and we are failing safe.
    Failed(String),
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Reason::Idle => write!(f, "every sensor cold enough to ask for nothing"),
            Reason::Steering(k) => write!(f, "steering on {k}"),
            Reason::Critical(k) => write!(f, "{k} above critical"),
            Reason::Failed(k) => write!(f, "{k} unreadable, failing safe"),
        }
    }
}

pub struct Sensor {
    pub cfg: SensorConfig,
    path: PathBuf,
    /// Accumulated integral term, in rpm above the floor.
    integral: f64,
    /// The last reading that passed the plausibility check.
    last_good: Option<f64>,
    consecutive_bad: u32,
    /// Set once the sensor has been unreadable for longer than the grace period.
    failed: bool,
}

impl Sensor {
    pub fn new(cfg: SensorConfig, path: &Path) -> Sensor {
        Sensor {
            cfg,
            path: path.to_path_buf(),
            integral: 0.0,
            last_good: None,
            consecutive_bad: 0,
            failed: false,
        }
    }

    /// Prime the controller with the reading taken during startup validation,
    /// so the first poll steers on a real temperature rather than on nothing.
    pub fn seed(&mut self, temp: f64) {
        self.last_good = Some(temp);
    }

    pub fn key(&self) -> &str {
        &self.cfg.key
    }

    pub fn last_reading(&self) -> Option<f64> {
        self.last_good
    }

    pub fn is_failed(&self) -> bool {
        self.failed
    }

    /// Take one reading, riding out transient unreadability on the last good
    /// value. Returns the error to log the first time a sample goes bad.
    fn sample(&mut self, range: [f64; 2], grace: u32) -> Option<String> {
        let problem = match read_temp(&self.path) {
            Err(e) => Some(format!("{e:#}")),
            Ok(t) if !(range[0]..=range[1]).contains(&t) => {
                Some(format!("read {t:.1}C, outside the plausible range"))
            }
            Ok(t) => {
                self.last_good = Some(t);
                self.consecutive_bad = 0;
                self.failed = false;
                None
            }
        };
        if problem.is_some() {
            self.consecutive_bad += 1;
            if self.consecutive_bad > grace {
                self.failed = true;
            }
        }
        // Only worth a log line on the transition into trouble.
        if self.consecutive_bad == 1 { problem } else { None }
    }

    /// The speed this sensor asks for on temperature alone: a linear ramp from
    /// the floor at `baseline_from` to the ceiling at `critical`.
    ///
    /// This is what keeps the daemon from idling a fan the SMC had spun up. A
    /// controller that only responds above its target contributes exactly
    /// nothing until it crosses that target, so a board with every sensor a
    /// couple of degrees under sits at the floor — slower than the curve it
    /// took over from, which is the opposite of the point of reading the extra
    /// sensors at all. The ramp makes the demand rise monotonically with
    /// temperature over the whole range instead.
    fn baseline(&self, temp: f64, min_rpm: f64, max_rpm: f64) -> f64 {
        // Validation guarantees baseline_from < target < critical, so the span
        // is positive.
        let span = self.cfg.critical - self.cfg.baseline_from;
        let fraction = ((temp - self.cfg.baseline_from) / span).clamp(0.0, 1.0);
        min_rpm + fraction * (max_rpm - min_rpm)
    }

    /// How much this sensor's integral term is allowed to contribute.
    fn integral_ceiling(&self, min_rpm: f64, max_rpm: f64) -> f64 {
        let span = max_rpm - min_rpm;
        match self.cfg.integral_limit_rpm {
            Some(limit) => (limit as f64).min(span),
            None => span,
        }
    }

    /// This sensor's demanded speed.
    fn demand(&mut self, dt: f64, min_rpm: f64, max_rpm: f64) -> (f64, Reason) {
        if self.failed {
            return (max_rpm, Reason::Failed(self.cfg.key.clone()));
        }
        // Nothing has ever read successfully; the startup check should have
        // caught that, so treat it the same as a failure.
        let Some(temp) = self.last_good else {
            return (max_rpm, Reason::Failed(self.cfg.key.clone()));
        };
        if temp >= self.cfg.critical {
            // The integral is deliberately left exactly as it was: neither
            // accumulated while critical, nor synthesised on the way out. The
            // descent out of an excursion is `ramp_down_rpm_per_s`'s job, and a
            // charged integral both takes it away from the slew limit and can
            // outlast the excursion indefinitely — a sensor that settles at
            // precisely its target has no error left to unwind it with.
            return (max_rpm, Reason::Critical(self.cfg.key.clone()));
        }

        let error = temp - self.cfg.target;
        // Clamping the integral to the usable rpm span is the anti-windup: a
        // sensor that sits above target for an hour cannot bank an arbitrarily
        // large term that then takes an hour to unwind once it cools.
        self.integral =
            (self.integral + self.cfg.ki * error * dt).clamp(0.0, self.integral_ceiling(min_rpm, max_rpm));

        // The baseline and the controller compose by taking whichever is
        // asking for more, not by adding: `integral_limit_rpm` exists to stop a
        // sensor airflow cannot reach from winding the fan up to maximum, and
        // summing the two would hand it that anyway by another route.
        let rpm =
            (min_rpm + self.cfg.kp * error + self.integral).max(self.baseline(temp, min_rpm, max_rpm));
        let reason = if rpm <= min_rpm {
            Reason::Idle
        } else {
            Reason::Steering(self.cfg.key.clone())
        };
        (rpm.clamp(min_rpm, max_rpm), reason)
    }
}

/// The commanded speed for one poll.
#[derive(Debug, Clone)]
pub struct Command {
    pub rpm: u32,
    pub reason: Reason,
}

/// Turns sensor readings into a fan speed, with rate limiting.
pub struct Governor {
    pub sensors: Vec<Sensor>,
    min_rpm: f64,
    max_rpm: f64,
    ramp_up: f64,
    ramp_down: f64,
    plausible: [f64; 2],
    grace: u32,
    /// The speed last commanded, which the slew limit works from.
    current: f64,
}

impl Governor {
    /// `start_rpm` is what the fan is already doing, so taking over from the
    /// SMC does not begin by stepping a spun-up fan down to the floor and
    /// slew-limiting its way back. The first `dt` is nearly zero, so whatever
    /// this is set to is very close to the first speed commanded.
    pub fn new(
        config: &Config,
        sensors: Vec<Sensor>,
        min_rpm: u32,
        max_rpm: u32,
        start_rpm: u32,
    ) -> Governor {
        Governor {
            sensors,
            min_rpm: min_rpm as f64,
            max_rpm: max_rpm as f64,
            ramp_up: config.ramp_up_rpm_per_s,
            ramp_down: config.ramp_down_rpm_per_s,
            plausible: config.plausible_range_c,
            grace: config.sensor_grace_polls,
            current: start_rpm.clamp(min_rpm, max_rpm) as f64,
        }
    }

    /// Sample every sensor, returning any newly-bad ones for the caller to log.
    pub fn sample(&mut self) -> Vec<(String, String)> {
        let (plausible, grace) = (self.plausible, self.grace);
        self.sensors
            .iter_mut()
            .filter_map(|s| s.sample(plausible, grace).map(|p| (s.cfg.key.clone(), p)))
            .collect()
    }

    /// Fold the sensors into one command. `dt` is the elapsed wall time since
    /// the previous call, in seconds.
    pub fn step(&mut self, dt: f64) -> Command {
        let (min, max) = (self.min_rpm, self.max_rpm);
        let mut best = (min, Reason::Idle);
        for sensor in &mut self.sensors {
            let (rpm, reason) = sensor.demand(dt, min, max);
            // A critical or failed sensor outranks a merely hot one even if the
            // arithmetic ties, so the log names the urgent reason.
            let outranks = rpm > best.0
                || (rpm >= best.0 && matches!(reason, Reason::Critical(_) | Reason::Failed(_)));
            if outranks {
                best = (rpm, reason);
            }
        }

        let (target, reason) = best;
        // An emergency is not rate limited on the way up. A critical or failed
        // sensor gets the air it is asking for on this poll rather than in two
        // seconds' time — and on the very first poll, where `dt` is nearly zero,
        // the slew limit is nearly zero too, so without this the answer to a
        // critical sensor at startup would be the speed we started at.
        let urgent = matches!(reason, Reason::Critical(_) | Reason::Failed(_));
        self.current = if urgent && target > self.current {
            target
        } else {
            let limit = if target > self.current { self.ramp_up } else { self.ramp_down } * dt;
            self.current + (target - self.current).clamp(-limit, limit)
        }
        .clamp(min, max);
        Command { rpm: self.current.round() as u32, reason }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sysfs::Source;

    fn cfg(key: &str, target: f64, critical: f64) -> SensorConfig {
        SensorConfig {
            source: Source::Applesmc,
            key: key.into(),
            // Deliberately shallow: most of these tests are about the
            // controller, and a wide baseline would answer for it.
            baseline_from: target - 5.0,
            target,
            critical,
            kp: 100.0,
            ki: 10.0,
            integral_limit_rpm: None,
            optional: false,
        }
    }

    fn sensor(key: &str, target: f64, critical: f64, temp: f64) -> Sensor {
        let mut s = Sensor::new(cfg(key, target, critical), Path::new("/nonexistent"));
        s.last_good = Some(temp);
        s
    }

    fn governor(sensors: Vec<Sensor>) -> Governor {
        let config = Config { ramp_up_rpm_per_s: 100_000.0, ramp_down_rpm_per_s: 100_000.0, ..Config::default() };
        Governor::new(&config, sensors, 2000, 6200, 2000)
    }

    #[test]
    fn sits_at_minimum_while_everything_is_below_target() {
        let mut g = governor(vec![sensor("TC0P", 75.0, 95.0, 60.0)]);
        let cmd = g.step(1.0);
        assert_eq!(cmd.rpm, 2000);
        assert_eq!(cmd.reason, Reason::Idle);
    }

    #[test]
    fn the_hottest_sensor_wins_not_the_average() {
        // One sensor well under target, one well over. An averaging daemon
        // would idle here; this must steer on the hot one.
        let mut g = governor(vec![
            sensor("Package id 0", 80.0, 95.0, 50.0),
            sensor("TPCD", 87.0, 100.0, 92.0),
        ]);
        let cmd = g.step(1.0);
        assert!(cmd.rpm > 2000, "expected the hot sensor to drive the fan, got {}", cmd.rpm);
        assert_eq!(cmd.reason, Reason::Steering("TPCD".into()));
    }

    #[test]
    fn a_sensor_over_critical_goes_straight_to_maximum() {
        let mut g = governor(vec![sensor("Package id 0", 80.0, 95.0, 96.0)]);
        let cmd = g.step(1.0);
        assert_eq!(cmd.rpm, 6200);
        assert_eq!(cmd.reason, Reason::Critical("Package id 0".into()));
    }

    #[test]
    fn an_unreadable_sensor_fails_to_maximum_not_to_minimum() {
        let mut s = sensor("TPCD", 87.0, 100.0, 85.0);
        s.failed = true;
        let mut g = governor(vec![s]);
        let cmd = g.step(1.0);
        assert_eq!(cmd.rpm, 6200);
        assert_eq!(cmd.reason, Reason::Failed("TPCD".into()));
    }

    #[test]
    fn an_implausible_reading_is_not_treated_as_cold() {
        // applesmc reports unpopulated sensors as 0C. Held below the grace
        // count the last good reading stands; past it the sensor fails safe.
        let mut s = Sensor::new(cfg("TCTD", 80.0, 95.0), Path::new("/nonexistent"));
        s.last_good = Some(90.0);
        for _ in 0..=5 {
            assert!(!s.is_failed());
            s.sample([5.0, 125.0], 5);
        }
        assert!(s.is_failed(), "a sensor that never reads again must fail safe");
    }

    #[test]
    fn the_baseline_ramp_asks_for_air_below_target_instead_of_the_floor() {
        // The complaint this exists for: a sensor a degree under its target
        // used to demand exactly the floor, so the daemon walked a fan the SMC
        // had spun up all the way down. baseline_from is target - 5 here, so
        // 74C is 4 degrees into the 25 degree ramp that runs from there to
        // critical.
        let mut g = governor(vec![sensor("TC0P", 75.0, 95.0, 74.0)]);
        let cmd = g.step(1.0);
        assert_eq!(cmd.rpm, 2672, "2000 + (74-70)/(95-70) * 4200");
        assert_eq!(cmd.reason, Reason::Steering("TC0P".into()));
    }

    #[test]
    fn the_baseline_ramp_never_falls_as_a_sensor_heats() {
        // "Steer up, never down": the demand must be monotone in temperature
        // across the whole range, with no dip where the baseline hands over to
        // the controller at target.
        let mut previous = 0;
        for tenths in 500..=949 {
            let temp = tenths as f64 / 10.0;
            let mut g = governor(vec![sensor("TC0P", 75.0, 95.0, temp)]);
            let rpm = g.step(1.0).rpm;
            assert!(rpm >= previous, "demand fell from {previous} to {rpm} at {temp}C");
            previous = rpm;
        }
    }

    #[test]
    fn the_integral_closes_a_gap_that_proportional_alone_leaves_open() {
        // A sensor parked 2 degrees over target: kp alone contributes a fixed
        // 200 rpm forever, and the baseline a fixed amount for that
        // temperature. The integral is what keeps pushing past both. This is
        // the mbpfan failure mode the daemon exists to fix.
        let mut g = governor(vec![sensor("TC0P", 75.0, 95.0, 77.0)]);
        let first = g.step(1.0).rpm;
        let mut last = first;
        for _ in 0..200 {
            last = g.step(1.0).rpm;
        }
        assert!(last > first, "integral should keep raising the demand ({first} -> {last})");
    }

    #[test]
    fn the_integral_cannot_wind_up_beyond_the_usable_range() {
        let mut g = governor(vec![sensor("TC0P", 75.0, 94.0, 93.0)]);
        // An hour above target must not bank a term that then takes an hour to
        // unwind once the sensor finally cools.
        for _ in 0..3_600 {
            g.step(1.0);
        }
        g.sensors[0].last_good = Some(50.0);
        let mut seconds = 0;
        while g.step(1.0).rpm > 2000 {
            seconds += 1;
            assert!(seconds < 60, "still spun up {seconds}s after cooling");
        }
    }

    #[test]
    fn a_capped_sensor_cannot_pin_the_fan_at_maximum_on_its_own() {
        // TPCD does not respond to airflow: measured 82-88C whether the fan is
        // at 3900 or 6200 rpm. Left uncapped it would wind up and sit the fan
        // at maximum forever for no cooling at all.
        let mut c = cfg("TPCD", 87.0, 100.0);
        c.kp = 0.0;
        c.integral_limit_rpm = Some(1500);
        // Started high enough that its own ramp asks for less than the cap at
        // 89C, so this measures the cap and not the baseline.
        c.baseline_from = 85.0;
        let mut s = Sensor::new(c, Path::new("/nonexistent"));
        s.last_good = Some(89.0);
        let mut g = governor(vec![s]);
        for _ in 0..3_600 {
            g.step(1.0);
        }
        assert_eq!(g.step(1.0).rpm, 3500, "capped integral should stop at min + limit");
    }

    #[test]
    fn a_capped_sensor_still_goes_to_maximum_when_it_is_critical() {
        // The cap must never get in the way of a real emergency.
        let mut c = cfg("TPCD", 87.0, 100.0);
        c.integral_limit_rpm = Some(1500);
        let mut s = Sensor::new(c, Path::new("/nonexistent"));
        s.last_good = Some(101.0);
        let mut g = governor(vec![s]);
        assert_eq!(g.step(1.0).rpm, 6200);
    }

    #[test]
    fn a_critical_excursion_leaves_nothing_charged_behind_it() {
        // A sensor that comes back down and settles at exactly its target has
        // an error of zero, so anything the excursion banked in the integral
        // would stay banked and hold the fan at maximum for good.
        let mut g = governor(vec![sensor("TC0P", 75.0, 95.0, 96.0)]);
        assert_eq!(g.step(1.0).rpm, 6200, "critical must still go to maximum");
        g.sensors[0].last_good = Some(75.0);
        let cmd = g.step(1.0);
        // Back to what the baseline alone asks for at 75C, carrying nothing
        // out of the excursion.
        assert_eq!(cmd.rpm, 2840, "2000 + (75-70)/(95-70) * 4200");
        assert_eq!(cmd.reason, Reason::Steering("TC0P".into()));
    }

    #[test]
    fn the_shipped_defaults_do_not_idle_the_fan_at_the_measured_operating_points() {
        // The rows of the README's measurement table, in the order the default
        // sensors are configured. Every one of these used to command the 2000
        // rpm floor: each sensor sat below its target, so the daemon consulted
        // five of them and asked for less air than the SMC curve it had just
        // switched off.
        for (what, temps, expected) in [
            ("light load", [70.0, 64.0, 85.0, 51.0, 54.0], 3200),
            ("four threads flat out", [86.0, 78.0, 88.0, 52.0, 54.0], 5120),
            ("sustained hot session", [77.0, 70.0, 91.0, 54.0, 54.0], 4310),
        ] {
            let sensors = Config::default()
                .sensors
                .into_iter()
                .zip(temps)
                .map(|(cfg, temp)| {
                    let mut s = Sensor::new(cfg, Path::new("/nonexistent"));
                    s.last_good = Some(temp);
                    s
                })
                .collect();
            // Slew limiting is not what is under test here.
            let config = Config {
                ramp_up_rpm_per_s: 100_000.0,
                ramp_down_rpm_per_s: 100_000.0,
                ..Config::default()
            };
            let mut g = Governor::new(&config, sensors, 2000, 6200, 2000);
            assert_eq!(g.step(1.0).rpm, expected, "{what}");
        }
    }

    #[test]
    fn ramp_down_is_rate_limited() {
        let config = Config { ramp_up_rpm_per_s: 100_000.0, ramp_down_rpm_per_s: 200.0, ..Config::default() };
        let mut g = Governor::new(&config, vec![sensor("TC0P", 75.0, 96.0, 95.0)], 2000, 6200, 2000);
        // Hold it above target until the controller has saturated.
        while g.step(1.0).rpm < 6200 {}
        g.sensors[0].last_good = Some(40.0);
        let cmd = g.step(1.0);
        assert_eq!(cmd.rpm, 6000, "should fall by at most ramp_down_rpm_per_s in one second");
    }

    #[test]
    fn ramp_up_is_fast_enough_to_reach_maximum_in_a_couple_of_seconds() {
        // The measured failure on this hardware is a load step putting the
        // package up 20C inside five seconds, so the fan must not be the
        // bottleneck getting to full speed.
        let config = Config::default();
        let mut g =
            Governor::new(&config, vec![sensor("Package id 0", 80.0, 95.0, 96.0)], 2000, 6200, 2000);
        let mut elapsed = 0.0;
        while g.step(0.1).rpm < 6200 {
            elapsed += 0.1;
            assert!(elapsed < 5.0, "took too long to reach maximum");
        }
    }

    #[test]
    fn a_critical_sensor_reaches_maximum_on_the_very_first_poll() {
        // The first poll's dt is a few microseconds, so the slew limit for it
        // is a few rpm. An emergency must not be metered out at that rate.
        let config = Config::default();
        let mut g =
            Governor::new(&config, vec![sensor("Package id 0", 80.0, 95.0, 99.0)], 2000, 6200, 2000);
        let cmd = g.step(0.000_02);
        assert_eq!(cmd.rpm, 6200, "a critical sensor must not be slew limited");
        assert_eq!(cmd.reason, Reason::Critical("Package id 0".into()));
    }

    #[test]
    fn a_failed_sensor_reaches_maximum_on_the_very_first_poll() {
        let config = Config::default();
        let mut s = sensor("TPCD", 87.0, 100.0, 85.0);
        s.failed = true;
        let mut g = Governor::new(&config, vec![s], 2000, 6200, 2000);
        assert_eq!(g.step(0.000_02).rpm, 6200);
    }

    #[test]
    fn taking_over_starts_from_the_speed_the_fan_is_already_running_at() {
        // The SMC had the fan at 5000 because the machine is warm. Starting
        // from the floor instead would drop it there for the first poll and
        // then slew back up.
        let config = Config::default();
        let mut g =
            Governor::new(&config, vec![sensor("Package id 0", 80.0, 95.0, 82.0)], 2000, 6200, 5000);
        assert_eq!(g.step(0.000_02).rpm, 5000);
    }

    #[test]
    fn a_start_speed_outside_the_configured_range_is_clamped_into_it() {
        let config = Config::default();
        let mut g =
            Governor::new(&config, vec![sensor("Package id 0", 80.0, 95.0, 50.0)], 2000, 4000, 6200);
        assert_eq!(g.step(0.000_02).rpm, 4000);
    }
}
