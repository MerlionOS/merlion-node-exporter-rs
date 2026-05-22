//! `/sys/class/hwmon` — hardware monitor sensor readings.
//!
//! Walks every `hwmon*` directory under `/sys/class/hwmon`, identifies the
//! chip via the `name` file (or by following the `device` symlink), then
//! enumerates every recognised `<type><N>_<property>` attribute file and
//! emits a gauge in the units upstream uses: temperatures in Celsius,
//! voltages in volts, currents in amps, fans in RPM, power in watts,
//! energy as a counter in joules, etc.
//!
//! Reference: `node_exporter/collector/hwmon_linux.go` — the regex grammar
//! (`hwmonFilenameFormat`), sensor-type list, and per-type unit conversions
//! are matched byte-for-byte. The `device` subdirectory is also walked
//! exactly as upstream does, because some drivers expose sensors there
//! instead of (or in addition to) the hwmonN directory itself.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::config::Config;
use crate::metric::{Metric, MetricType, Sample};
use crate::registry::Collector;

/// Sensor types recognised by hwmon. Files whose `<type>` prefix isn't in
/// this set are skipped — matches upstream `hwmonSensorTypes`.
const SENSOR_TYPES: &[&str] = &[
    "vrm",
    "beep_enable",
    "update_interval",
    "in",
    "cpu",
    "fan",
    "pwm",
    "temp",
    "curr",
    "power",
    "energy",
    "humidity",
    "intrusion",
    "freq",
];

pub struct HwmonCollector;

impl Collector for HwmonCollector {
    fn name(&self) -> &'static str {
        "hwmon"
    }

    fn collect(&self, cfg: &Config) -> anyhow::Result<Vec<Metric>> {
        let root = cfg.sys_path("/sys/class/hwmon");
        collect_from_root(&root)
    }
}

/// Sensor data for one chip, keyed by sensor identifier (e.g. `temp1`) →
/// property (`input`, `max`, `label`, …) → raw textual value as read from
/// sysfs (trailing newline stripped).
type SensorData = BTreeMap<String, BTreeMap<String, String>>;

/// Per-metric-family accumulator. Hwmon emits a huge number of different
/// metric names depending on which chips are present, so we collect them
/// into one `Metric` per `(name, mtype)` and append samples — this keeps
/// the text output well-formed (one `# HELP` / `# TYPE` per family).
#[derive(Default)]
struct FamilyMap {
    families: Vec<Metric>,
}

impl FamilyMap {
    fn push(&mut self, name: &str, help: &str, mtype: MetricType, sample: Sample) {
        if let Some(m) = self.families.iter_mut().find(|m| m.name == name) {
            m.push(sample);
            return;
        }
        let mut m = Metric::new(name, help, mtype);
        m.push(sample);
        self.families.push(m);
    }
}

fn collect_from_root(root: &Path) -> anyhow::Result<Vec<Metric>> {
    let entries = match fs::read_dir(root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // hwmon is optional — no /sys/class/hwmon means no sensors
            // exposed by this kernel. Don't error; just emit nothing.
            return Ok(Vec::new());
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", root.display())),
    };

    let mut hwmon_dirs: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("iterating {}", root.display()))?;
        let path = entry.path();
        // Follow symlinks (each hwmonN is a symlink into /sys/devices).
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            hwmon_dirs.push(path);
        }
    }
    // Stable order so test output and metric ordering are deterministic.
    hwmon_dirs.sort();

    let mut families = FamilyMap::default();
    for dir in &hwmon_dirs {
        // Upstream returns `lastErr` after best-effort iteration; we do the
        // same — one broken chip shouldn't blank the rest of the scrape.
        if let Err(e) = update_one_chip(&mut families, dir) {
            tracing::debug!(dir = %dir.display(), error = %e, "hwmon: chip failed");
        }
    }

    Ok(families.families)
}

fn update_one_chip(families: &mut FamilyMap, dir: &Path) -> anyhow::Result<()> {
    let Some(chip) = hwmon_chip_label(dir) else {
        // Some hwmonN directories lack both a `name` and resolvable
        // `device` symlink (kernel quirk). Skip them silently — upstream
        // returns an error here but we filter at this level instead so a
        // single quirky chip doesn't show up as a scrape error.
        return Ok(());
    };

    let mut data: SensorData = BTreeMap::new();
    collect_sensor_data(dir, &mut data)?;
    let device_dir = dir.join("device");
    if device_dir.is_dir() {
        // ignore errors — `device` may not be readable on every kernel.
        let _ = collect_sensor_data(&device_dir, &mut data);
    }

    // Annotation metric: human-readable chip name, if present.
    if let Some(chip_name) = read_name_file(dir) {
        families.push(
            "node_hwmon_chip_names",
            "Annotation metric for human-readable chip names",
            MetricType::Gauge,
            Sample::new(1.0)
                .with_label("chip", &chip)
                .with_label("chip_name", &chip_name),
        );
    }

    for (sensor, props) in &data {
        emit_sensor(families, &chip, sensor, props);
    }

    Ok(())
}

fn emit_sensor(
    families: &mut FamilyMap,
    chip: &str,
    sensor: &str,
    props: &BTreeMap<String, String>,
) {
    let Some((sensor_type, _id)) = explode_sensor(sensor) else {
        return;
    };

    // Info-style label metric: 1.0 with chip/sensor/label labels.
    if let Some(label_text) = props.get("label") {
        let safe = sanitize_utf8(label_text);
        families.push(
            "node_hwmon_sensor_label",
            "Label for given chip and sensor",
            MetricType::Gauge,
            Sample::new(1.0)
                .with_label("chip", chip)
                .with_label("sensor", sensor)
                .with_label("label", &safe),
        );
    }

    match sensor_type {
        "beep_enable" => {
            let val = if props.get("").map(String::as_str) == Some("1") {
                1.0
            } else {
                0.0
            };
            families.push(
                "node_hwmon_beep_enabled",
                "Hardware beep enabled",
                MetricType::Gauge,
                Sample::new(val)
                    .with_label("chip", chip)
                    .with_label("sensor", sensor),
            );
            return;
        }
        "vrm" => {
            if let Some(v) = props.get("").and_then(|s| s.parse::<f64>().ok()) {
                families.push(
                    "node_hwmon_voltage_regulator_version",
                    "Hardware voltage regulator",
                    MetricType::Gauge,
                    Sample::new(v)
                        .with_label("chip", chip)
                        .with_label("sensor", sensor),
                );
            }
            return;
        }
        "update_interval" => {
            if let Some(v) = props.get("").and_then(|s| s.parse::<f64>().ok()) {
                families.push(
                    "node_hwmon_update_interval_seconds",
                    "Hardware monitor update interval",
                    MetricType::Gauge,
                    Sample::new(v * 0.001)
                        .with_label("chip", chip)
                        .with_label("sensor", sensor),
                );
            }
            return;
        }
        _ => {}
    }

    let prefix = format!("node_hwmon_{sensor_type}");
    for (element, raw_value) in props {
        if element == "label" {
            continue;
        }
        let Ok(parsed) = raw_value.parse::<f64>() else {
            continue;
        };

        let mut name = prefix.clone();
        if element == "input" {
            // `input` is the primary value; only suffix with `_input` if
            // there is also a bare `""` reading for this sensor.
            if props.contains_key("") {
                name.push_str("_input");
            }
        } else if !element.is_empty() {
            name.push('_');
            name.push_str(&clean_metric_name(element));
        }

        emit_typed(
            families,
            &TypedEmit {
                sensor_type,
                element,
                name: &name,
                parsed,
                chip,
                sensor,
                props,
            },
        );
    }
}

/// Arguments to [`emit_typed`]. Bundled into a struct because the
/// upstream Go dispatch naturally fans out across a lot of context
/// (sensor type, element name, parsed numeric value, chip/sensor labels,
/// and the rest of the property map for the freq-with-label corner case).
struct TypedEmit<'a> {
    sensor_type: &'a str,
    element: &'a str,
    name: &'a str,
    parsed: f64,
    chip: &'a str,
    sensor: &'a str,
    props: &'a BTreeMap<String, String>,
}

fn emit_typed(families: &mut FamilyMap, args: &TypedEmit<'_>) {
    // Unit-less status flags.
    if args.element == "fault" || args.element == "alarm" {
        families.push(
            args.name,
            &format!(
                "Hardware sensor {} status ({})",
                args.element, args.sensor_type
            ),
            MetricType::Gauge,
            sample(args.parsed, args.chip, args.sensor),
        );
        return;
    }
    if args.element == "beep" {
        let beep_name = format!("{}_enabled", args.name);
        families.push(
            &beep_name,
            "Hardware monitor sensor has beeping enabled",
            MetricType::Gauge,
            sample(args.parsed, args.chip, args.sensor),
        );
        return;
    }

    if emit_voltage_current_temp(families, args) {
        return;
    }
    if emit_energy_power(families, args) {
        return;
    }
    if emit_fan_humidity_freq(families, args) {
        return;
    }

    // Fallback: emit the parsed number as a unit-less gauge.
    families.push(
        args.name,
        &format!(
            "Hardware monitor {} element {}",
            args.sensor_type, args.element
        ),
        MetricType::Gauge,
        sample(args.parsed, args.chip, args.sensor),
    );
}

/// Returns `true` if the value was emitted under voltage / temperature /
/// current rules.
fn emit_voltage_current_temp(families: &mut FamilyMap, a: &TypedEmit<'_>) -> bool {
    match a.sensor_type {
        "in" | "cpu" => {
            let n = format!("{}_volts", a.name);
            families.push(
                &n,
                &format!("Hardware monitor for voltage ({})", a.element),
                MetricType::Gauge,
                sample(a.parsed * 0.001, a.chip, a.sensor),
            );
            true
        }
        "temp" if a.element != "type" => {
            let elem = if a.element.is_empty() {
                "input"
            } else {
                a.element
            };
            let n = format!("{}_celsius", a.name);
            families.push(
                &n,
                &format!("Hardware monitor for temperature ({elem})"),
                MetricType::Gauge,
                sample(a.parsed * 0.001, a.chip, a.sensor),
            );
            true
        }
        "curr" => {
            let n = format!("{}_amps", a.name);
            families.push(
                &n,
                &format!("Hardware monitor for current ({})", a.element),
                MetricType::Gauge,
                sample(a.parsed * 0.001, a.chip, a.sensor),
            );
            true
        }
        _ => false,
    }
}

/// Returns `true` if the value was emitted under energy / power rules.
fn emit_energy_power(families: &mut FamilyMap, a: &TypedEmit<'_>) -> bool {
    match a.sensor_type {
        "energy" => {
            let n = format!("{}_joule_total", a.name);
            families.push(
                &n,
                &format!("Hardware monitor for joules used so far ({})", a.element),
                MetricType::Counter,
                sample(a.parsed / 1_000_000.0, a.chip, a.sensor),
            );
            true
        }
        "power" if a.element == "accuracy" => {
            families.push(
                a.name,
                "Hardware monitor power meter accuracy, as a ratio",
                MetricType::Gauge,
                sample(a.parsed / 1_000_000.0, a.chip, a.sensor),
            );
            true
        }
        "power"
            if a.element == "average_interval"
                || a.element == "average_interval_min"
                || a.element == "average_interval_max" =>
        {
            let n = format!("{}_seconds", a.name);
            families.push(
                &n,
                &format!(
                    "Hardware monitor power usage update interval ({})",
                    a.element
                ),
                MetricType::Gauge,
                sample(a.parsed * 0.001, a.chip, a.sensor),
            );
            true
        }
        "power" => {
            let n = format!("{}_watt", a.name);
            families.push(
                &n,
                &format!("Hardware monitor for power usage in watts ({})", a.element),
                MetricType::Gauge,
                sample(a.parsed / 1_000_000.0, a.chip, a.sensor),
            );
            true
        }
        _ => false,
    }
}

/// Returns `true` if the value was emitted under fan / humidity / freq
/// rules.
fn emit_fan_humidity_freq(families: &mut FamilyMap, a: &TypedEmit<'_>) -> bool {
    match a.sensor_type {
        "humidity" => {
            families.push(
                a.name,
                &format!(
                    "Hardware monitor for humidity, as a ratio (multiply with 100.0 to get the humidity as a percentage) ({})",
                    a.element
                ),
                MetricType::Gauge,
                sample(a.parsed / 1_000_000.0, a.chip, a.sensor),
            );
            true
        }
        "fan"
            if a.element == "input"
                || a.element == "min"
                || a.element == "max"
                || a.element == "target" =>
        {
            let n = format!("{}_rpm", a.name);
            families.push(
                &n,
                &format!(
                    "Hardware monitor for fan revolutions per minute ({})",
                    a.element
                ),
                MetricType::Gauge,
                sample(a.parsed, a.chip, a.sensor),
            );
            true
        }
        "freq" if a.element == "input" => {
            // Upstream only emits freq when a label sub-file is present —
            // and it *replaces* the `sensor` label with the cleaned text
            // label. Match that exactly.
            if let Some(text_label) = a.props.get("label") {
                let cleaned = clean_metric_name(text_label);
                let n = format!("{}_freq_mhz", a.name);
                families.push(
                    &n,
                    "Hardware monitor for GPU frequency in MHz",
                    MetricType::Gauge,
                    Sample::new(a.parsed / 1_000_000.0)
                        .with_label("chip", a.chip)
                        .with_label("sensor", &cleaned),
                );
            }
            // Whether or not we emitted, freq+input is "handled" — don't
            // fall through to the catch-all gauge.
            true
        }
        _ => false,
    }
}

fn sample(value: f64, chip: &str, sensor: &str) -> Sample {
    Sample::new(value)
        .with_label("chip", chip)
        .with_label("sensor", sensor)
}

/// Walk every regular file under `dir`, route it through
/// [`explode_sensor_filename`], and populate the [`SensorData`] map.
/// Matches `collectSensorData` in the upstream Go.
fn collect_sensor_data(dir: &Path, data: &mut SensorData) -> anyhow::Result<()> {
    let entries =
        fs::read_dir(dir).with_context(|| format!("reading hwmon dir {}", dir.display()))?;
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some((sensor_type, num, property)) = explode_sensor_filename(name) else {
            continue;
        };
        if !SENSOR_TYPES.contains(&sensor_type) {
            continue;
        }

        let path = entry.path();
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let value = raw.trim_end_matches('\n').to_string();
        let sensor_key = format!("{sensor_type}{num}");
        data.entry(sensor_key)
            .or_default()
            .insert(property.to_string(), value);
    }
    Ok(())
}

/// Mimics upstream's `hwmonFilenameFormat`:
///   `^(?P<type>[^0-9]+)(?P<id>[0-9]*)?(_(?P<property>.+))?$`
///
/// Returns `(sensor_type, sensor_id, property)`. `property` is `""` when
/// the file has the form `<type><id>` with no underscore (e.g. `vrm`).
fn explode_sensor_filename(filename: &str) -> Option<(&str, u32, &str)> {
    let bytes = filename.as_bytes();
    // type: leading run of non-digits
    let mut i = 0;
    while i < bytes.len() && !bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == 0 {
        return None;
    }
    let sensor_type = &filename[..i];

    // id: optional run of digits
    let id_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    let id_str = &filename[id_start..i];
    let id: u32 = if id_str.is_empty() {
        0
    } else {
        id_str.parse().ok()?
    };

    // optional `_<property>`
    let property = if i == bytes.len() {
        ""
    } else if bytes[i] == b'_' && i + 1 < bytes.len() {
        &filename[i + 1..]
    } else {
        // The remainder doesn't start with `_` — file like `tempA1` would
        // fall here. Upstream regex would still match with property="".
        // To stay strict we return None for shapes the regex rejects.
        return None;
    };

    Some((sensor_type, id, property))
}

/// Convenience over `explode_sensor_filename` for the sensor-key form
/// (`temp1`, `in0`, …) — returns `(type, id)` only.
fn explode_sensor(sensor: &str) -> Option<(&str, u32)> {
    explode_sensor_filename(sensor).map(|(t, n, _)| (t, n))
}

/// Derive the chip label (`chip="..."`) for a hwmon directory. Tries, in
/// upstream's order:
///   1. resolve `<dir>/device` and combine its parent dir name + own name;
///   2. read `<dir>/name`;
///   3. fall back to the last path component (`hwmon0`).
///
/// Returns `None` only if none of those produce a usable string — caller
/// treats that as "skip silently".
fn hwmon_chip_label(dir: &Path) -> Option<String> {
    // Preference 1: device symlink target.
    let dev_link = dir.join("device");
    if let Ok(resolved) = fs::canonicalize(&dev_link) {
        let dev_name = resolved.file_name().and_then(|s| s.to_str()).unwrap_or("");
        let dev_type = resolved
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .unwrap_or("");
        let clean_name = clean_metric_name(dev_name);
        let clean_type = clean_metric_name(dev_type);
        if !clean_type.is_empty() && !clean_name.is_empty() {
            return Some(format!("{clean_type}_{clean_name}"));
        }
        if !clean_name.is_empty() {
            return Some(clean_name);
        }
    }

    // Preference 2: `<dir>/name`.
    if let Some(name) = read_name_file(dir) {
        return Some(name);
    }

    // Preference 3: last path component.
    let last = dir.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let cleaned = clean_metric_name(last);
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

fn read_name_file(dir: &Path) -> Option<String> {
    let raw = fs::read_to_string(dir.join("name")).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let cleaned = clean_metric_name(trimmed);
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

/// Lower-case and replace every character that isn't `[a-z0-9:_]` with
/// `_`, then trim leading/trailing underscores — matches upstream's
/// `cleanMetricName`.
fn clean_metric_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        let lc = c.to_ascii_lowercase();
        if lc.is_ascii_alphanumeric() || lc == ':' || lc == '_' {
            out.push(lc);
        } else {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches('_');
    trimmed.to_string()
}

/// Lossy UTF-8 sanitisation matching upstream's `strings.ToValidUTF8` with
/// `"\u{FFFD}"` replacement — but the input here is already a Rust `&str`
/// so we just substitute control characters that would corrupt the text
/// exposition format.
fn sanitize_utf8(s: &str) -> String {
    s.chars()
        .map(|c| if (c as u32) < 0x20 { '\u{FFFD}' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Build a minimal hwmonN directory tree under `root` for tests.
    /// Returns the directory path.
    fn make_hwmon(root: &Path, idx: usize, name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = root.join(format!("hwmon{idx}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("name"), name).unwrap();
        for (fname, contents) in files {
            fs::write(dir.join(fname), contents).unwrap();
        }
        dir
    }

    #[test]
    fn explode_filename_simple() {
        assert_eq!(
            explode_sensor_filename("temp1_input"),
            Some(("temp", 1, "input"))
        );
        assert_eq!(explode_sensor_filename("in0_max"), Some(("in", 0, "max")));
        assert_eq!(
            explode_sensor_filename("fan2_label"),
            Some(("fan", 2, "label"))
        );
    }

    #[test]
    fn explode_filename_no_property() {
        // bare type+id with no underscore — e.g. some intrusion files
        assert_eq!(explode_sensor_filename("temp1"), Some(("temp", 1, "")));
    }

    #[test]
    fn explode_filename_no_id() {
        // type spans the underscore when there's no digit run between
        // segments. Upstream regex: type=[^0-9]+ matches "update_interval"
        // greedily, leaving id="" and property="".
        assert_eq!(
            explode_sensor_filename("update_interval"),
            Some(("update_interval", 0, ""))
        );
    }

    #[test]
    fn explode_filename_rejects_pure_digits() {
        assert_eq!(explode_sensor_filename("123"), None);
    }

    #[test]
    fn clean_metric_name_lowercases_and_substitutes() {
        assert_eq!(clean_metric_name("CoreTemp"), "coretemp");
        assert_eq!(clean_metric_name("nct-6779.f"), "nct_6779_f");
        assert_eq!(clean_metric_name("__weird__"), "weird");
    }

    #[test]
    fn missing_root_returns_empty_not_error() {
        let tmp = TempDir::new().unwrap();
        let metrics = collect_from_root(&tmp.path().join("nope")).unwrap();
        assert!(metrics.is_empty());
    }

    #[test]
    fn parses_temp_voltage_fan_fixture() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        make_hwmon(
            root,
            0,
            "coretemp",
            &[
                ("temp1_input", "42000"), // 42.0 °C
                ("temp1_max", "85000"),
                ("temp1_label", "Package id 0"),
                ("in0_input", "1200"),  // 1.2 V
                ("fan1_input", "3500"), // 3500 RPM
                ("fan1_min", "1000"),
            ],
        );

        let metrics = collect_from_root(root).unwrap();

        // node_hwmon_temp_celsius — both input and max get this name (with
        // different help) — actually, only `_input` triggers the simple
        // `_celsius` suffix; `_max` becomes `node_hwmon_temp_max_celsius`.
        let temp_input = metrics
            .iter()
            .find(|m| m.name == "node_hwmon_temp_celsius")
            .expect("temp celsius family present");
        assert!((temp_input.samples[0].value - 42.0).abs() < 1e-9);
        assert_eq!(temp_input.samples[0].labels[0].0, "chip");
        assert_eq!(temp_input.samples[0].labels[0].1, "coretemp");
        assert_eq!(temp_input.samples[0].labels[1].0, "sensor");
        assert_eq!(temp_input.samples[0].labels[1].1, "temp1");

        let temp_max = metrics
            .iter()
            .find(|m| m.name == "node_hwmon_temp_max_celsius")
            .expect("temp max present");
        assert!((temp_max.samples[0].value - 85.0).abs() < 1e-9);

        let voltage = metrics
            .iter()
            .find(|m| m.name == "node_hwmon_in_volts")
            .expect("voltage family present");
        assert!((voltage.samples[0].value - 1.2).abs() < 1e-9);

        let fan_rpm = metrics
            .iter()
            .find(|m| m.name == "node_hwmon_fan_rpm")
            .expect("fan rpm family present");
        // `_input` is the bare `_rpm`-suffixed family.
        assert_eq!(fan_rpm.samples.len(), 1);
        assert!((fan_rpm.samples[0].value - 3500.0).abs() < 1e-9);

        let fan_min_rpm = metrics
            .iter()
            .find(|m| m.name == "node_hwmon_fan_min_rpm")
            .expect("fan min rpm family present");
        assert!((fan_min_rpm.samples[0].value - 1000.0).abs() < 1e-9);

        // info-style label metric
        let label = metrics
            .iter()
            .find(|m| m.name == "node_hwmon_sensor_label")
            .expect("sensor_label present");
        assert!((label.samples[0].value - 1.0).abs() < f64::EPSILON);
        let lab = &label.samples[0].labels;
        assert!(lab.iter().any(|(k, v)| k == "label" && v == "Package id 0"));

        // chip_names annotation
        let chip_names = metrics
            .iter()
            .find(|m| m.name == "node_hwmon_chip_names")
            .expect("chip_names annotation present");
        assert!(
            chip_names.samples[0]
                .labels
                .iter()
                .any(|(k, v)| k == "chip_name" && v == "coretemp")
        );
    }

    #[test]
    fn skips_directory_without_name() {
        let scratch = TempDir::new().unwrap();
        let root = scratch.path();
        // Create hwmon0 with no name file and no device symlink. Upstream
        // would error here; we skip silently.
        let dir = root.join("hwmon0");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("temp1_input"), "50000").unwrap();

        // We DO produce a metric, because the fallback derives the chip
        // label from the directory name "hwmon0". This matches upstream's
        // preference-3 behaviour.
        let metrics = collect_from_root(root).unwrap();
        let celsius = metrics
            .iter()
            .find(|m| m.name == "node_hwmon_temp_celsius")
            .expect("temp present even without name file");
        assert_eq!(celsius.samples[0].labels[0].1, "hwmon0");

        // But no node_hwmon_chip_names should be emitted — that one
        // requires a readable `name` file.
        assert!(metrics.iter().all(|m| m.name != "node_hwmon_chip_names"));
    }

    #[test]
    fn skips_truly_empty_directory() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        // hwmon0 with neither name, device, nor any recognised sensor files.
        let dir = root.join("hwmon0");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("garbage_file"), "nope").unwrap();

        let metrics = collect_from_root(root).unwrap();
        // Only sensor families would have been emitted; with no recognised
        // files, there are none.
        assert!(
            metrics.iter().all(
                |m| m.name != "node_hwmon_chip_names" && !m.name.starts_with("node_hwmon_temp")
            )
        );
    }

    #[test]
    fn ignores_malformed_numeric_content() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        make_hwmon(
            root,
            0,
            "fakechip",
            &[("temp1_input", "not a number"), ("temp1_max", "75000")],
        );

        let metrics = collect_from_root(root).unwrap();
        // temp1_input is unparseable → no _celsius sample for that;
        // _max still parses fine, named node_hwmon_temp_max_celsius.
        let max = metrics
            .iter()
            .find(|m| m.name == "node_hwmon_temp_max_celsius")
            .expect("temp_max_celsius present");
        assert_eq!(max.samples.len(), 1);
        assert!((max.samples[0].value - 75.0).abs() < 1e-9);

        // And no plain node_hwmon_temp_celsius (since input was garbage).
        assert!(metrics.iter().all(|m| m.name != "node_hwmon_temp_celsius"));
    }

    #[test]
    fn unrecognised_sensor_types_skipped() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        make_hwmon(
            root,
            0,
            "fakechip",
            &[
                ("bogus1_input", "100"),
                ("name", "fakechip"), // overwritten by make_hwmon
                ("temp1_input", "30000"),
            ],
        );

        let metrics = collect_from_root(root).unwrap();
        // The bogus type doesn't appear anywhere.
        assert!(
            metrics
                .iter()
                .all(|m| !m.name.starts_with("node_hwmon_bogus"))
        );
        // But temp1 is there.
        assert!(metrics.iter().any(|m| m.name == "node_hwmon_temp_celsius"));
    }

    #[test]
    fn energy_is_counter_with_microjoule_conversion() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        make_hwmon(
            root,
            0,
            "rapl",
            &[
                ("energy1_input", "2500000"), // 2.5 J
            ],
        );

        let metrics = collect_from_root(root).unwrap();
        let energy = metrics
            .iter()
            .find(|m| m.name == "node_hwmon_energy_joule_total")
            .expect("energy_joule_total present");
        assert_eq!(energy.mtype, MetricType::Counter);
        assert!((energy.samples[0].value - 2.5).abs() < 1e-9);
    }
}
