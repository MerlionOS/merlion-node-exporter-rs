//! `textfile` — re-exports metrics from `*.prom` files in a configured
//! directory.
//!
//! This is the upstream pattern for letting batch jobs, cron tasks, and
//! ad-hoc scripts expose their own metrics without running an HTTP server:
//! each writer drops a file conforming to the Prometheus text exposition
//! format into a shared directory, and the exporter merges them into its
//! own `/metrics` output on every scrape.
//!
//! Behaviour matches `node_exporter/collector/textfile.go`:
//!
//! - Only top-level `*.prom` files in the configured directory are read
//!   (no recursion).
//! - One `node_textfile_mtime_seconds{file="<basename>"}` gauge per file,
//!   so operators can alert on stale producers.
//! - `node_textfile_scrape_error` is `1` when *any* file in the directory
//!   failed to read or parse on this scrape, otherwise `0`. Files that did
//!   parse still contribute their metrics — partial scrape output is more
//!   useful than none.
//! - Unconfigured directory (`Config::textfile_directory == None`) is a
//!   no-op: the collector emits nothing and reports success.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::config::Config;
use crate::metric::{Metric, MetricType, Sample};
use crate::registry::Collector;

pub struct TextfileCollector;

impl Collector for TextfileCollector {
    fn name(&self) -> &'static str {
        "textfile"
    }

    fn collect(&self, cfg: &Config) -> anyhow::Result<Vec<Metric>> {
        let Some(dir) = cfg.textfile_directory.as_deref() else {
            return Ok(Vec::new());
        };
        Ok(collect_from_dir(dir))
    }
}

fn collect_from_dir(dir: &Path) -> Vec<Metric> {
    let mut had_error = false;
    let mut mtime = Metric::new(
        "node_textfile_mtime_seconds",
        "Unixtime mtime of textfiles successfully read.",
        MetricType::Gauge,
    );

    let entries = match fs::read_dir(dir) {
        Ok(it) => it,
        Err(e) => {
            tracing::warn!(dir = %dir.display(), error = %e, "textfile: cannot read directory");
            return vec![scrape_error_metric(1.0)];
        }
    };

    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "prom"))
        .collect();
    files.sort();

    let mut families: Vec<Metric> = Vec::new();

    for path in &files {
        let basename = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();

        let raw = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(file = %path.display(), error = %e, "textfile: read failed");
                had_error = true;
                continue;
            }
        };

        match parse_exposition(&raw) {
            Ok(parsed) => {
                merge_families(&mut families, parsed);
                if let Some(t) = mtime_of(path) {
                    mtime.push(Sample::new(t).with_label("file", basename));
                }
            }
            Err(e) => {
                tracing::warn!(file = %path.display(), error = %e, "textfile: parse failed");
                had_error = true;
            }
        }
    }

    families.push(mtime);
    families.push(scrape_error_metric(if had_error { 1.0 } else { 0.0 }));
    families
}

fn scrape_error_metric(v: f64) -> Metric {
    Metric::new(
        "node_textfile_scrape_error",
        "1 if there was an error opening or reading a file, 0 otherwise.",
        MetricType::Gauge,
    )
    .with_sample(Sample::new(v))
}

fn mtime_of(path: &Path) -> Option<f64> {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
}

/// Merge newly parsed families into the running accumulator. Samples for
/// a name already seen are appended; HELP/TYPE come from the first file
/// to define them.
fn merge_families(into: &mut Vec<Metric>, new: Vec<Metric>) {
    for fresh in new {
        if let Some(existing) = into.iter_mut().find(|m| m.name == fresh.name) {
            existing.samples.extend(fresh.samples);
        } else {
            into.push(fresh);
        }
    }
}

#[derive(Default)]
struct FamilyBuilder {
    help: Option<String>,
    mtype: Option<MetricType>,
    samples: Vec<Sample>,
}

fn parse_exposition(raw: &str) -> Result<Vec<Metric>, String> {
    let mut order: Vec<String> = Vec::new();
    let mut families: BTreeMap<String, FamilyBuilder> = BTreeMap::new();

    for (lineno, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        if let Some(rest) = line.strip_prefix('#') {
            let rest = rest.trim_start();
            if let Some(payload) = rest.strip_prefix("HELP ") {
                let (name, help) = split_once_ws(payload).unwrap_or((payload, ""));
                let fb = ensure_family(&mut order, &mut families, name);
                fb.help = Some(unescape_help(help));
            } else if let Some(payload) = rest.strip_prefix("TYPE ") {
                let (name, ty) = split_once_ws(payload)
                    .ok_or_else(|| format!("line {}: malformed TYPE", lineno + 1))?;
                let fb = ensure_family(&mut order, &mut families, name);
                fb.mtype = Some(parse_type(ty));
            }
            // Other `#` lines are free-form comments — skip.
            continue;
        }

        let (name, labels, value) =
            parse_sample(line).map_err(|e| format!("line {}: {}", lineno + 1, e))?;
        let fb = ensure_family(&mut order, &mut families, &name);
        fb.samples.push(Sample { labels, value });
    }

    let mut out = Vec::with_capacity(order.len());
    for name in order {
        if let Some(fb) = families.remove(&name) {
            let help = fb.help.unwrap_or_default();
            let mtype = fb.mtype.unwrap_or(MetricType::Untyped);
            let mut m = Metric::new(&name, &help, mtype);
            m.samples = fb.samples;
            out.push(m);
        }
    }
    Ok(out)
}

fn ensure_family<'a>(
    order: &mut Vec<String>,
    families: &'a mut BTreeMap<String, FamilyBuilder>,
    name: &str,
) -> &'a mut FamilyBuilder {
    if !families.contains_key(name) {
        order.push(name.to_string());
        families.insert(name.to_string(), FamilyBuilder::default());
    }
    families.get_mut(name).expect("inserted above")
}

fn parse_type(s: &str) -> MetricType {
    match s.trim() {
        "counter" => MetricType::Counter,
        "gauge" => MetricType::Gauge,
        // Histogram/summary samples still parse as individual lines, but we
        // surface them as untyped because the encoder doesn't synthesise the
        // `_bucket` / `_sum` / `_count` structure that those types require.
        _ => MetricType::Untyped,
    }
}

fn split_once_ws(s: &str) -> Option<(&str, &str)> {
    let s = s.trim_start();
    let end = s.find(|c: char| c.is_ascii_whitespace())?;
    Some((&s[..end], s[end..].trim_start()))
}

fn unescape_help(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('\\') | None => out.push('\\'),
                Some('n') => out.push('\n'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

type Labels = Vec<(String, String)>;

/// Parse one sample line: `name[{labels}] value [timestamp]`.
fn parse_sample(line: &str) -> Result<(String, Labels, f64), String> {
    let line = line.trim();
    // Metric name runs up to whitespace, `{`, or end.
    let name_end = line
        .find(|c: char| c.is_ascii_whitespace() || c == '{')
        .ok_or_else(|| "missing value".to_string())?;
    let name = line[..name_end].to_string();
    if name.is_empty() {
        return Err("empty metric name".into());
    }

    let after_name = &line[name_end..];
    let (labels, after_labels) = if let Some(rest) = after_name.strip_prefix('{') {
        let close = rest
            .find('}')
            .ok_or_else(|| "unterminated label block".to_string())?;
        let labels = parse_labels(&rest[..close])?;
        (labels, &rest[close + 1..])
    } else {
        (Vec::new(), after_name)
    };

    let mut toks = after_labels.split_ascii_whitespace();
    let value_tok = toks.next().ok_or_else(|| "missing value".to_string())?;
    let value = parse_value(value_tok)?;
    // Ignore optional trailing timestamp.
    Ok((name, labels, value))
}

fn parse_value(tok: &str) -> Result<f64, String> {
    match tok {
        "NaN" | "nan" => Ok(f64::NAN),
        "+Inf" | "Inf" | "inf" => Ok(f64::INFINITY),
        "-Inf" | "-inf" => Ok(f64::NEG_INFINITY),
        _ => tok.parse().map_err(|_| format!("bad value {tok:?}")),
    }
}

/// Parse the comma-separated `k="v",k2="v2"` body inside `{...}`.
fn parse_labels(body: &str) -> Result<Labels, String> {
    let mut out = Vec::new();
    let mut rest = body.trim();
    while !rest.is_empty() {
        let eq = rest
            .find('=')
            .ok_or_else(|| "label missing =".to_string())?;
        let key = rest[..eq].trim().to_string();
        if key.is_empty() {
            return Err("empty label name".into());
        }
        let after_eq = rest[eq + 1..].trim_start();
        let after_eq = after_eq
            .strip_prefix('"')
            .ok_or_else(|| "label value must be quoted".to_string())?;
        let (value, tail) = scan_quoted(after_eq)?;
        out.push((key, value));
        rest = tail.trim_start();
        if let Some(t) = rest.strip_prefix(',') {
            rest = t.trim_start();
        } else if !rest.is_empty() {
            return Err(format!("expected ',' between labels, got {rest:?}"));
        }
    }
    Ok(out)
}

/// Read a double-quoted string starting at the first byte after the opening
/// quote and return (decoded value, slice past the closing quote).
fn scan_quoted(s: &str) -> Result<(String, &str), String> {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'"' {
            return Ok((out, &s[i + 1..]));
        }
        if b == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'\\' => out.push('\\'),
                b'"' => out.push('"'),
                b'n' => out.push('\n'),
                other => {
                    out.push('\\');
                    out.push(other as char);
                }
            }
            i += 2;
            continue;
        }
        // UTF-8 safety: advance one char rather than one byte.
        let ch = s[i..].chars().next().expect("non-empty");
        out.push(ch);
        i += ch.len_utf8();
    }
    Err("unterminated quoted string".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::tempdir;

    fn metric<'a>(metrics: &'a [Metric], name: &str) -> &'a Metric {
        metrics
            .iter()
            .find(|m| m.name == name)
            .unwrap_or_else(|| panic!("missing metric {name}"))
    }

    #[test]
    fn parses_simple_gauge() {
        let raw = "\
# HELP foo_bytes Some help.
# TYPE foo_bytes gauge
foo_bytes 42
";
        let m = parse_exposition(raw).unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].name, "foo_bytes");
        assert_eq!(m[0].help, "Some help.");
        assert_eq!(m[0].mtype, MetricType::Gauge);
        assert_eq!(m[0].samples.len(), 1);
        assert!((m[0].samples[0].value - 42.0).abs() < f64::EPSILON);
    }

    #[test]
    fn parses_labels_with_escapes() {
        let raw = r#"foo{a="x",b="y\"z\\w"} 1"#;
        let m = parse_exposition(raw).unwrap();
        assert_eq!(m[0].samples[0].labels.len(), 2);
        assert_eq!(m[0].samples[0].labels[0], ("a".into(), "x".into()));
        assert_eq!(m[0].samples[0].labels[1], ("b".into(), "y\"z\\w".into()),);
    }

    #[test]
    fn untyped_when_no_type_directive() {
        let m = parse_exposition("foo 1").unwrap();
        assert_eq!(m[0].mtype, MetricType::Untyped);
        assert_eq!(m[0].help, "");
    }

    #[test]
    fn ignores_trailing_timestamp() {
        let m = parse_exposition("foo 1 1700000000000").unwrap();
        assert!((m[0].samples[0].value - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn parses_special_values() {
        let m = parse_exposition("a 1\nb NaN\nc +Inf\nd -Inf").unwrap();
        assert!(
            m.iter().find(|x| x.name == "b").unwrap().samples[0]
                .value
                .is_nan()
        );
        assert!(metric(&m, "c").samples[0].value.is_infinite());
        assert!(metric(&m, "d").samples[0].value.is_sign_negative());
    }

    #[test]
    fn rejects_unterminated_labels() {
        assert!(parse_exposition("foo{a=\"x 1").is_err());
    }

    #[test]
    fn rejects_unquoted_label_value() {
        assert!(parse_exposition("foo{a=x} 1").is_err());
    }

    #[test]
    fn unconfigured_directory_returns_empty() {
        let cfg = Config::default();
        let out = TextfileCollector.collect(&cfg).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn missing_directory_emits_scrape_error_one() {
        let out = collect_from_dir(Path::new("/definitely/not/a/path/here"));
        let err = metric(&out, "node_textfile_scrape_error");
        assert!((err.samples[0].value - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn happy_path_directory_with_two_files() {
        let dir = tempdir().unwrap();
        let mut f = File::create(dir.path().join("a.prom")).unwrap();
        writeln!(f, "# HELP a_total Total.").unwrap();
        writeln!(f, "# TYPE a_total counter").unwrap();
        writeln!(f, "a_total 1").unwrap();
        drop(f);
        let mut f = File::create(dir.path().join("b.prom")).unwrap();
        writeln!(f, "b_gauge 2").unwrap();
        drop(f);
        // A non-.prom file in the same dir must be ignored.
        File::create(dir.path().join("ignore.txt"))
            .unwrap()
            .write_all(b"junk\n")
            .unwrap();

        let out = collect_from_dir(dir.path());
        assert!((metric(&out, "a_total").samples[0].value - 1.0).abs() < f64::EPSILON);
        assert!((metric(&out, "b_gauge").samples[0].value - 2.0).abs() < f64::EPSILON);
        let mtime = metric(&out, "node_textfile_mtime_seconds");
        assert_eq!(mtime.samples.len(), 2);
        let err = metric(&out, "node_textfile_scrape_error");
        assert!((err.samples[0].value - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn malformed_file_marks_scrape_error_but_keeps_good_files() {
        let dir = tempdir().unwrap();
        File::create(dir.path().join("good.prom"))
            .unwrap()
            .write_all(b"good_gauge 3\n")
            .unwrap();
        File::create(dir.path().join("bad.prom"))
            .unwrap()
            .write_all(b"bad{a=missing_quote} 1\n")
            .unwrap();

        let out = collect_from_dir(dir.path());
        assert!((metric(&out, "good_gauge").samples[0].value - 3.0).abs() < f64::EPSILON);
        let err = metric(&out, "node_textfile_scrape_error");
        assert!((err.samples[0].value - 1.0).abs() < f64::EPSILON);
        // mtime should only have the good file recorded.
        let mtime = metric(&out, "node_textfile_mtime_seconds");
        assert_eq!(mtime.samples.len(), 1);
        assert_eq!(mtime.samples[0].labels[0].1, "good.prom");
    }

    #[test]
    fn merges_same_metric_across_files() {
        let dir = tempdir().unwrap();
        File::create(dir.path().join("a.prom"))
            .unwrap()
            .write_all(b"shared{src=\"a\"} 1\n")
            .unwrap();
        File::create(dir.path().join("b.prom"))
            .unwrap()
            .write_all(b"shared{src=\"b\"} 2\n")
            .unwrap();

        let out = collect_from_dir(dir.path());
        let shared = metric(&out, "shared");
        assert_eq!(shared.samples.len(), 2);
    }
}
