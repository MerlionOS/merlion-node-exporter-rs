//! Prometheus text exposition format encoder.
//!
//! Implements the subset of the 0.0.4 text format that `node_exporter` emits:
//! `# HELP`, `# TYPE`, and one sample per line. Histograms/summaries are out
//! of scope for the MVP collectors.

use std::fmt::Write as _;

use crate::metric::Metric;

pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Encode a slice of metric families into the Prometheus text format.
pub fn encode(metrics: &[Metric]) -> String {
    let mut out = String::with_capacity(metrics.len() * 128);
    for m in metrics {
        if m.samples.is_empty() {
            continue;
        }
        writeln!(out, "# HELP {} {}", m.name, escape_help(&m.help)).unwrap();
        writeln!(out, "# TYPE {} {}", m.name, m.mtype).unwrap();
        for s in &m.samples {
            out.push_str(&m.name);
            write_labels(&mut out, &s.labels);
            out.push(' ');
            write_value(&mut out, s.value);
            out.push('\n');
        }
    }
    out
}

fn write_labels(out: &mut String, labels: &[(String, String)]) {
    if labels.is_empty() {
        return;
    }
    out.push('{');
    for (i, (k, v)) in labels.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(k);
        out.push_str("=\"");
        for ch in v.chars() {
            match ch {
                '\\' => out.push_str("\\\\"),
                '"' => out.push_str("\\\""),
                '\n' => out.push_str("\\n"),
                c => out.push(c),
            }
        }
        out.push('"');
    }
    out.push('}');
}

// Integer-valued doubles are emitted without a decimal point so the text
// format matches what upstream `node_exporter` produces; the float_cmp /
// truncation lints are intentional here.
#[allow(clippy::float_cmp, clippy::cast_possible_truncation)]
fn write_value(out: &mut String, v: f64) {
    if v.is_nan() {
        out.push_str("NaN");
    } else if v.is_infinite() {
        out.push_str(if v.is_sign_negative() { "-Inf" } else { "+Inf" });
    } else if v == v.trunc() && v.abs() < 1e15 {
        write!(out, "{}", v as i64).unwrap();
    } else {
        write!(out, "{v}").unwrap();
    }
}

fn escape_help(help: &str) -> String {
    let mut out = String::with_capacity(help.len());
    for ch in help.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metric::{MetricType, Sample};

    #[test]
    fn encodes_counter_with_labels() {
        let m = Metric::new("node_test_total", "Test counter.", MetricType::Counter)
            .with_sample(Sample::new(3.0).with_label("path", "/a"));
        let s = encode(&[m]);
        assert!(s.contains("# HELP node_test_total Test counter."));
        assert!(s.contains("# TYPE node_test_total counter"));
        assert!(s.contains(r#"node_test_total{path="/a"} 3"#));
    }

    #[test]
    fn escapes_label_value() {
        let m = Metric::new("g", "g", MetricType::Gauge)
            .with_sample(Sample::new(1.0).with_label("k", "a\"b\\c\nd"));
        let s = encode(&[m]);
        assert!(s.contains(r#"k="a\"b\\c\nd""#));
    }

    #[test]
    fn integer_values_print_without_decimal() {
        let m = Metric::new("g", "g", MetricType::Gauge).with_sample(Sample::new(42.0));
        assert!(encode(&[m]).contains("g 42\n"));
    }

    #[test]
    fn float_values_print_full_precision() {
        let m = Metric::new("g", "g", MetricType::Gauge).with_sample(Sample::new(1.5));
        assert!(encode(&[m]).contains("g 1.5\n"));
    }

    #[test]
    fn skips_empty_families() {
        let m = Metric::new("empty", "h", MetricType::Gauge);
        assert_eq!(encode(&[m]), "");
    }
}
