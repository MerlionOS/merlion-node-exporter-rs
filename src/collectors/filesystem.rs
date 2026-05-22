//! `/proc/mounts` + `statvfs(2)` — per-mount filesystem capacity and inode usage.
//!
//! Emits the seven metric families upstream `node_exporter` exposes from
//! `filesystem_common.go` (minus the macOS-only `purgeable_bytes` and the
//! `mount_info` Linux mountinfo helper):
//!
//! - `node_filesystem_size_bytes`  (gauge) — `f_blocks * f_frsize`
//! - `node_filesystem_free_bytes`  (gauge) — `f_bfree  * f_frsize`
//! - `node_filesystem_avail_bytes` (gauge) — `f_bavail * f_frsize`
//! - `node_filesystem_files`       (gauge) — `f_files`
//! - `node_filesystem_files_free`  (gauge) — `f_ffree`
//! - `node_filesystem_readonly`    (gauge) — 1 if the mount carries the `ro`
//!   option, else 0
//! - `node_filesystem_device_error` (gauge) — 1 if `statvfs` failed on this
//!   mount, else 0; the size/free/files metrics are still emitted in that
//!   case (as zeros) so per-mount alerts don't go silent on the failed mount
//!
//! Every sample carries `device`, `mountpoint`, `fstype` labels — columns
//! 1, 2, 3 of `/proc/mounts`. The mountpoint string is post-processed to
//! undo the `\040 \011 \012 \134` octal escapes that `fstab(5)` mandates.
//!
//! Reference: upstream `collector/filesystem_linux.go` and
//! `collector/filesystem_common.go`.

use std::fs;

use anyhow::{Context, anyhow};

use crate::config::Config;
use crate::metric::{Metric, MetricType, Sample};
use crate::registry::Collector;

const FS_LABEL_NAMES: [&str; 3] = ["device", "mountpoint", "fstype"];

pub struct FilesystemCollector;

impl Collector for FilesystemCollector {
    fn name(&self) -> &'static str {
        "filesystem"
    }

    fn collect(&self, cfg: &Config) -> anyhow::Result<Vec<Metric>> {
        let path = cfg.proc_path("/proc/mounts");
        let raw =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let mounts = parse_mounts(&raw)?;
        Ok(collect_metrics(&mounts))
    }
}

/// One parsed row of `/proc/mounts`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MountEntry {
    pub device: String,
    pub mountpoint: String,
    pub fstype: String,
    pub options: String,
}

impl MountEntry {
    fn readonly(&self) -> bool {
        self.options.split(',').any(|opt| opt == "ro")
    }
}

/// Parse `/proc/mounts` into structured rows.
///
/// `/proc/mounts` is space-separated: `device mountpoint fstype options dump
/// pass`. Per `fstab(5)`, fields that contain spaces / tabs / newlines /
/// backslashes are octal-escaped (`\040 \011 \012 \134` respectively); we
/// undo those escapes for `device` and `mountpoint` so labels read naturally.
pub(crate) fn parse_mounts(raw: &str) -> anyhow::Result<Vec<MountEntry>> {
    let mut out = Vec::new();
    for (lineno, line) in raw.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split_ascii_whitespace();
        let device = fields
            .next()
            .ok_or_else(|| anyhow!("filesystem: line {} missing device", lineno + 1))?;
        let mountpoint = fields
            .next()
            .ok_or_else(|| anyhow!("filesystem: line {} missing mountpoint", lineno + 1))?;
        let fstype = fields
            .next()
            .ok_or_else(|| anyhow!("filesystem: line {} missing fstype", lineno + 1))?;
        let options = fields
            .next()
            .ok_or_else(|| anyhow!("filesystem: line {} missing options", lineno + 1))?;

        out.push(MountEntry {
            device: unescape_octal(device),
            mountpoint: unescape_octal(mountpoint),
            fstype: fstype.to_string(),
            options: options.to_string(),
        });
    }
    Ok(out)
}

/// Undo `fstab(5)` octal escapes (`\040 \011 \012 \134`).
fn unescape_octal(input: &str) -> String {
    if !input.contains('\\') {
        return input.to_string();
    }
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        // A backslash followed by exactly three octal digits ⇒ decode.
        if bytes[idx] == b'\\' && idx + 3 < bytes.len() {
            let d0 = bytes[idx + 1];
            let d1 = bytes[idx + 2];
            let d2 = bytes[idx + 3];
            if (b'0'..=b'7').contains(&d0)
                && (b'0'..=b'7').contains(&d1)
                && (b'0'..=b'7').contains(&d2)
            {
                let value = (d0 - b'0') * 64 + (d1 - b'0') * 8 + (d2 - b'0');
                out.push(value as char);
                idx += 4;
                continue;
            }
        }
        // `input` is &str (valid UTF-8); we step one byte at a time only for
        // the ASCII subset (`\\` is ASCII), so non-ASCII bytes always fall
        // through via the `else` branch which copies the next char.
        if bytes[idx].is_ascii() {
            out.push(bytes[idx] as char);
            idx += 1;
        } else {
            // Find the next char boundary, copy the multi-byte char.
            let ch = input[idx..].chars().next().expect("char at boundary");
            out.push(ch);
            idx += ch.len_utf8();
        }
    }
    out
}

/// Build the seven metric families from the parsed mount list. On Linux, run
/// `statvfs` per mount; on other platforms, mark every mount with
/// `device_error=1` (the read of `/proc/mounts` itself will normally have
/// already failed, but keep the code path total).
fn collect_metrics(mounts: &[MountEntry]) -> Vec<Metric> {
    let mut size = make_metric("node_filesystem_size_bytes", "Filesystem size in bytes.");
    let mut free = make_metric(
        "node_filesystem_free_bytes",
        "Filesystem free space in bytes.",
    );
    let mut avail = make_metric(
        "node_filesystem_avail_bytes",
        "Filesystem space available to non-root users in bytes.",
    );
    let mut files = make_metric("node_filesystem_files", "Filesystem total file nodes.");
    let mut files_free = make_metric(
        "node_filesystem_files_free",
        "Filesystem total free file nodes.",
    );
    let mut readonly = make_metric("node_filesystem_readonly", "Filesystem read-only status.");
    let mut device_error = make_metric(
        "node_filesystem_device_error",
        "Whether an error occurred while getting statistics for the given device.",
    );

    // Deduplicate the (device, mountpoint, fstype) triple. Upstream does the
    // same to avoid double-emission when /proc/mounts lists the same mount
    // twice (e.g., bind mounts with different options).
    let mut seen: std::collections::HashSet<(String, String, String)> =
        std::collections::HashSet::new();

    for mount in mounts {
        let key = (
            mount.device.clone(),
            mount.mountpoint.clone(),
            mount.fstype.clone(),
        );
        if !seen.insert(key) {
            continue;
        }

        let ro_value = f64::from(u8::from(mount.readonly()));
        readonly.push(labelled_sample(ro_value, mount));

        if let Ok(stats) = statvfs(&mount.mountpoint) {
            device_error.push(labelled_sample(0.0, mount));
            size.push(labelled_sample(stats.size_bytes, mount));
            free.push(labelled_sample(stats.free_bytes, mount));
            avail.push(labelled_sample(stats.avail_bytes, mount));
            files.push(labelled_sample(stats.files, mount));
            files_free.push(labelled_sample(stats.files_free, mount));
        } else {
            // Match upstream: still emit size/free/files samples for the
            // failed mount (as zeros), and flag device_error=1.
            device_error.push(labelled_sample(1.0, mount));
            size.push(labelled_sample(0.0, mount));
            free.push(labelled_sample(0.0, mount));
            avail.push(labelled_sample(0.0, mount));
            files.push(labelled_sample(0.0, mount));
            files_free.push(labelled_sample(0.0, mount));
        }
    }

    vec![size, free, avail, files, files_free, readonly, device_error]
}

fn make_metric(name: &'static str, help: &'static str) -> Metric {
    Metric::new(name, help, MetricType::Gauge)
}

fn labelled_sample(value: f64, mount: &MountEntry) -> Sample {
    Sample::new(value)
        .with_label(FS_LABEL_NAMES[0], &mount.device)
        .with_label(FS_LABEL_NAMES[1], &mount.mountpoint)
        .with_label(FS_LABEL_NAMES[2], &mount.fstype)
}

struct StatvfsResult {
    size_bytes: f64,
    free_bytes: f64,
    avail_bytes: f64,
    files: f64,
    files_free: f64,
}

#[cfg(target_os = "linux")]
fn statvfs(mountpoint: &str) -> anyhow::Result<StatvfsResult> {
    use std::ffi::CString;
    use std::mem::MaybeUninit;

    let c_path = CString::new(mountpoint)
        .with_context(|| format!("mountpoint {mountpoint:?} contains interior NUL"))?;
    // SAFETY: `libc::statvfs64` is a Linux POSIX syscall; we pass an
    // initialised C string pointer and an uninitialised `statvfs64` buffer
    // that the kernel populates on success. Any non-zero return means the
    // buffer was not written, so we never `assume_init` in that case.
    let mut buf = MaybeUninit::<libc::statvfs64>::uninit();
    let rc = unsafe { libc::statvfs64(c_path.as_ptr(), buf.as_mut_ptr()) };
    if rc != 0 {
        return Err(
            anyhow!(std::io::Error::last_os_error()).context(format!("statvfs({mountpoint})"))
        );
    }
    let buf = unsafe { buf.assume_init() };

    // `f_frsize` is the fragment size — the unit upstream multiplies block
    // counts by (see filesystem_linux.go:processStat). On every common Linux
    // filesystem `f_frsize == f_bsize`, but POSIX specifies `f_frsize` here.
    //
    // Pedantic-clippy allowances: statvfs64 fields are `u64` / `c_ulong`;
    // mount sizes up to ~9 EiB lose no precision after the `as f64` cast on
    // any plausible host, and the multiplications can't overflow `f64`'s
    // range. Annotating per-line keeps the rationale next to the cast.
    #[allow(
        clippy::cast_precision_loss,
        reason = "petabyte-scale filesystems still fit exactly in f64"
    )]
    let frsize = buf.f_frsize as f64;
    #[allow(clippy::cast_precision_loss, reason = "see frsize above")]
    let blocks = buf.f_blocks as f64;
    #[allow(clippy::cast_precision_loss, reason = "see frsize above")]
    let bfree = buf.f_bfree as f64;
    #[allow(clippy::cast_precision_loss, reason = "see frsize above")]
    let bavail = buf.f_bavail as f64;
    #[allow(clippy::cast_precision_loss, reason = "inode counts fit in f64")]
    let files = buf.f_files as f64;
    #[allow(clippy::cast_precision_loss, reason = "see files above")]
    let ffree = buf.f_ffree as f64;

    Ok(StatvfsResult {
        size_bytes: blocks * frsize,
        free_bytes: bfree * frsize,
        avail_bytes: bavail * frsize,
        files,
        files_free: ffree,
    })
}

#[cfg(not(target_os = "linux"))]
fn statvfs(_mountpoint: &str) -> anyhow::Result<StatvfsResult> {
    // The collector is Linux-only. On other platforms the `/proc/mounts`
    // read in `collect` will already have failed (no /proc), but if a
    // simulated mount list is fed in we still return Err here so each row
    // is flagged with `device_error=1`.
    Err(anyhow!("statvfs is only implemented on Linux"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "\
/dev/root / ext4 rw,relatime,discard 0 0
tmpfs /run tmpfs rw,nosuid,nodev,size=1626288k,mode=755 0 0
cgroup2 /sys/fs/cgroup cgroup2 rw,nosuid,nodev,noexec,relatime,nsdelegate 0 0
";

    #[test]
    fn parses_happy_path() {
        let mounts = parse_mounts(FIXTURE).unwrap();
        assert_eq!(mounts.len(), 3);

        assert_eq!(mounts[0].device, "/dev/root");
        assert_eq!(mounts[0].mountpoint, "/");
        assert_eq!(mounts[0].fstype, "ext4");
        assert_eq!(mounts[0].options, "rw,relatime,discard");
        assert!(!mounts[0].readonly());

        assert_eq!(mounts[1].device, "tmpfs");
        assert_eq!(mounts[1].mountpoint, "/run");
        assert_eq!(mounts[1].fstype, "tmpfs");

        assert_eq!(mounts[2].device, "cgroup2");
        assert_eq!(mounts[2].mountpoint, "/sys/fs/cgroup");
        assert_eq!(mounts[2].fstype, "cgroup2");
    }

    #[test]
    fn unescapes_octal_spaces_in_mountpoint() {
        // A mountpoint with a literal space — fstab(5) encodes it as \040.
        let raw = "/dev/sdb1 /mnt/my\\040volume ext4 ro,relatime 0 0\n";
        let mounts = parse_mounts(raw).unwrap();
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].mountpoint, "/mnt/my volume");
        assert!(mounts[0].readonly(), "ro option => readonly() true");
    }

    #[test]
    fn unescapes_tabs_newlines_and_backslashes() {
        // \011 = tab, \012 = newline, \134 = backslash.
        let raw = "src /a\\011b\\012c\\134d ext4 rw 0 0\n";
        let mounts = parse_mounts(raw).unwrap();
        assert_eq!(mounts[0].mountpoint, "/a\tb\nc\\d");
    }

    #[test]
    fn leaves_non_octal_backslashes_alone() {
        // `\9` is not a 3-digit octal escape; pass through verbatim.
        let raw = "src /weird\\9path ext4 rw 0 0\n";
        let mounts = parse_mounts(raw).unwrap();
        assert_eq!(mounts[0].mountpoint, "/weird\\9path");
    }

    #[test]
    fn skips_blank_lines() {
        let raw = "\n/dev/sda1 / ext4 rw 0 0\n\n";
        let mounts = parse_mounts(raw).unwrap();
        assert_eq!(mounts.len(), 1);
    }

    #[test]
    fn rejects_truncated_row() {
        // Only two fields — missing fstype and options.
        let err = parse_mounts("/dev/sda1 /\n").unwrap_err().to_string();
        assert!(err.contains("missing fstype"), "{err}");
    }

    #[test]
    fn rejects_row_missing_options() {
        let err = parse_mounts("/dev/sda1 / ext4\n").unwrap_err().to_string();
        assert!(err.contains("missing options"), "{err}");
    }

    #[test]
    fn readonly_detects_ro_among_other_options() {
        let raw = "/dev/sr0 /media iso9660 nosuid,nodev,ro,relatime 0 0\n";
        let mounts = parse_mounts(raw).unwrap();
        assert!(mounts[0].readonly());
    }

    #[test]
    fn readonly_does_not_match_relatime_or_other_substrings() {
        // "relatime" contains the substring "ro" but is not the option "ro".
        let raw = "/dev/sda1 / ext4 rw,relatime 0 0\n";
        let mounts = parse_mounts(raw).unwrap();
        assert!(!mounts[0].readonly());
    }

    #[test]
    fn collect_metrics_emits_seven_families_per_unique_mount() {
        // Use mounts that won't statvfs successfully on macOS test hosts —
        // every row gets device_error=1, but we still want all seven
        // metric families and the right number of samples.
        let mounts = vec![
            MountEntry {
                device: "/dev/nonexistent-a".into(),
                mountpoint: "/__nope_a".into(),
                fstype: "ext4".into(),
                options: "rw".into(),
            },
            MountEntry {
                device: "/dev/nonexistent-b".into(),
                mountpoint: "/__nope_b".into(),
                fstype: "tmpfs".into(),
                options: "ro,relatime".into(),
            },
        ];
        let metrics = collect_metrics(&mounts);
        assert_eq!(
            metrics.len(),
            7,
            "size/free/avail/files/files_free/ro/deverr"
        );

        let names: Vec<&str> = metrics.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "node_filesystem_size_bytes",
                "node_filesystem_free_bytes",
                "node_filesystem_avail_bytes",
                "node_filesystem_files",
                "node_filesystem_files_free",
                "node_filesystem_readonly",
                "node_filesystem_device_error",
            ]
        );

        // Every metric carries one sample per unique mount.
        for m in &metrics {
            assert_eq!(m.samples.len(), 2, "{} sample count", m.name);
            for s in &m.samples {
                let label_names: Vec<&str> = s.labels.iter().map(|(k, _)| k.as_str()).collect();
                assert_eq!(label_names, vec!["device", "mountpoint", "fstype"]);
            }
        }

        // The readonly metric reflects the option string.
        let ro = metrics
            .iter()
            .find(|m| m.name == "node_filesystem_readonly")
            .unwrap();
        assert!((ro.samples[0].value - 0.0).abs() < f64::EPSILON);
        assert!((ro.samples[1].value - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn collect_metrics_deduplicates_identical_triples() {
        let m = MountEntry {
            device: "tmpfs".into(),
            mountpoint: "/run".into(),
            fstype: "tmpfs".into(),
            options: "rw".into(),
        };
        let metrics = collect_metrics(&[m.clone(), m]);
        for fam in &metrics {
            assert_eq!(fam.samples.len(), 1, "{} should be deduped", fam.name);
        }
    }

    /// Integration-ish smoke test: calling `collect()` on the real `Config`
    /// must return either Ok (Linux: /proc/mounts present, statvfs runs) or
    /// Err (macOS: /proc/mounts missing). Either outcome is allowed; the
    /// only thing this test guards against is a panic from the collector
    /// itself.
    #[test]
    fn collect_against_real_config_does_not_panic() {
        let cfg = Config::default();
        // Ok (Linux: /proc/mounts present, statvfs runs) or Err (macOS:
        // /proc/mounts missing). Either outcome is acceptable; we only
        // guard against a panic and shape-check the Ok side.
        if let Ok(metrics) = FilesystemCollector.collect(&cfg) {
            assert_eq!(metrics.len(), 7);
            assert!(metrics.iter().all(|fam| !fam.samples.is_empty()));
        }
    }
}
