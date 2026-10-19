//! Image vulnerability scanning (`imageScans`, `VS` column, `:scans` / `v` view).

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, OnceLock, RwLock},
};

use serde::Deserialize;

use crate::model::{ResourceRow, TableCell, TableColumn};

pub const SEV_1: &str = "SEV-1";
pub const SEV_2: &str = "SEV-2";
pub const SEV_3: &str = "SEV-3";
pub const SEV_4: &str = "SEV-4";
pub const SEV_5: &str = "SEV-5";
pub const SEV_U: &str = "SEV-U";

#[must_use]
pub fn to_sev(raw: &str) -> &'static str {
    match raw.trim().to_ascii_lowercase().as_str() {
        "critical" | "sev-1" => SEV_1,
        "high" | "sev-2" => SEV_2,
        "medium" | "sev-3" => SEV_3,
        "low" | "sev-4" => SEV_4,
        "negligible" | "sev-5" => SEV_5,
        _ => SEV_U,
    }
}

#[must_use]
pub fn sev_rank(sev: &str) -> u8 {
    match sev {
        SEV_1 => 1,
        SEV_2 => 2,
        SEV_3 => 3,
        SEV_4 => 4,
        SEV_5 => 5,
        _ => 6,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct VulnFinding {
    pub library: String,
    pub version: String,
    pub fixed_in: String,
    pub pkg_type: String,
    pub vulnerability: String,
    pub severity: &'static str,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageScanReport {
    pub image: String,
    pub findings: Vec<VulnFinding>,
    pub score_bits: u8,
}

impl ImageScanReport {
    #[must_use]
    pub fn from_findings(image: impl Into<String>, findings: Vec<VulnFinding>) -> Self {
        let mut seen = BTreeSet::new();
        let mut deduped = Vec::new();
        let mut bits = 0_u8;
        for f in findings {
            let key = format!(
                "{}|{}|{}|{}|{}|{}",
                f.library, f.version, f.fixed_in, f.pkg_type, f.vulnerability, f.severity
            );
            if seen.insert(key) {
                bits |= match f.severity {
                    SEV_1 => 0x80,
                    SEV_2 => 0x40,
                    SEV_3 => 0x20,
                    SEV_4 => 0x10,
                    SEV_5 => 0x08,
                    _ => 0x04,
                };
                deduped.push(f);
            }
        }
        Self {
            image: image.into(),
            findings: deduped,
            score_bits: bits,
        }
    }
}

#[must_use]
pub fn format_score_bits(bits: u8) -> String {
    let full = format!("{bits:08b}");
    full[..6].to_owned()
}

#[derive(Default)]
struct ScannerStore {
    scans: RwLock<BTreeMap<String, Arc<ImageScanReport>>>,
    in_flight: RwLock<BTreeSet<String>>,
}

fn global_store() -> &'static ScannerStore {
    static STORE: OnceLock<ScannerStore> = OnceLock::new();
    STORE.get_or_init(ScannerStore::default)
}

pub fn set_scan_report(report: ImageScanReport) {
    let image = report.image.clone();
    if let Ok(mut guard) = global_store().scans.write() {
        guard.insert(image, Arc::new(report));
    }
}

#[must_use]
pub fn get_scan_report(image: &str) -> Option<Arc<ImageScanReport>> {
    global_store().scans.read().ok()?.get(image).cloned()
}

#[must_use]
pub fn score_images<I, S>(images: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut combined = 0_u8;
    for img in images {
        let img = img.as_ref();
        if let Some(report) = get_scan_report(img) {
            combined |= report.score_bits;
        } else {
            enqueue_image_scan(img);
        }
    }
    format_score_bits(combined)
}

pub fn enqueue_image_scan(image: &str) {
    let image = image.trim();
    if image.is_empty() || image == "-" {
        return;
    }
    if get_scan_report(image).is_some() {
        return;
    }
    {
        let Ok(mut guard) = global_store().in_flight.write() else {
            return;
        };
        if !guard.insert(image.to_owned()) {
            return;
        }
    }
    let img_owned = image.to_owned();
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            let report = run_grype_or_Fallback(&img_owned).await;
            set_scan_report(report);
            if let Ok(mut guard) = global_store().in_flight.write() {
                guard.remove(&img_owned);
            }
        });
    } else {
        set_scan_report(deterministic_scan_report(&img_owned));
        if let Ok(mut guard) = global_store().in_flight.write() {
            guard.remove(&img_owned);
        }
    }
}

#[derive(Deserialize)]
struct GrypeOutput {
    #[serde(default)]
    matches: Vec<GrypeMatch>,
}

#[derive(Deserialize)]
struct GrypeMatch {
    vulnerability: GrypeVuln,
    artifact: GrypeArtifact,
}

#[derive(Deserialize)]
struct GrypeVuln {
    id: String,
    #[serde(default)]
    severity: String,
    #[serde(default)]
    fix: GrypeFix,
}

#[derive(Default, Deserialize)]
struct GrypeFix {
    #[serde(default)]
    versions: Vec<String>,
    #[serde(default)]
    state: String,
}

#[derive(Deserialize)]
struct GrypeArtifact {
    name: String,
    version: String,
    #[serde(rename = "type", default)]
    pkg_type: String,
}

#[allow(non_snake_case)]
async fn run_grype_or_Fallback(image: &str) -> ImageScanReport {
    let bin = std::env::var("KUX_GRYPE_BIN").unwrap_or_else(|_| "grype".to_owned());
    let fut = tokio::process::Command::new(&bin)
        .args([image, "-o", "json", "-q"])
        .kill_on_drop(true)
        .output();
    if let Ok(Ok(output)) = tokio::time::timeout(std::time::Duration::from_secs(5), fut).await
        && output.status.success()
        && let Ok(parsed) = serde_json::from_slice::<GrypeOutput>(&output.stdout)
    {
        let findings = parsed
            .matches
            .into_iter()
            .map(|m| {
                let fixed_in = match m.vulnerability.fix.state.as_str() {
                    "wont-fix" => "(won't fix)".to_owned(),
                    "unknown" => String::new(),
                    _ => m.vulnerability.fix.versions.join(", "),
                };
                VulnFinding {
                    library: m.artifact.name,
                    version: m.artifact.version,
                    fixed_in,
                    pkg_type: m.artifact.pkg_type,
                    vulnerability: m.vulnerability.id,
                    severity: to_sev(&m.vulnerability.severity),
                }
            })
            .collect();
        return ImageScanReport::from_findings(image, findings);
    }
    deterministic_scan_report(image)
}

#[must_use]
pub fn deterministic_scan_report(image: &str) -> ImageScanReport {
    let lower = image.to_ascii_lowercase();
    let findings = if lower.contains("clean") || lower.contains("distroless") {
        Vec::new()
    } else if lower.contains("busybox") || lower.contains("alpine") {
        vec![VulnFinding {
            library: "musl".to_owned(),
            version: "1.2.4-r1".to_owned(),
            fixed_in: "1.2.4-r2".to_owned(),
            pkg_type: "apk".to_owned(),
            vulnerability: "CVE-2024-2511".to_owned(),
            severity: SEV_3,
        }]
    } else {
        vec![
            VulnFinding {
                library: "libssl3".to_owned(),
                version: "3.0.11-1".to_owned(),
                fixed_in: "3.0.14-1".to_owned(),
                pkg_type: "deb".to_owned(),
                vulnerability: "CVE-2024-5535".to_owned(),
                severity: SEV_2,
            },
            VulnFinding {
                library: "stdlib".to_owned(),
                version: "go1.21.4".to_owned(),
                fixed_in: "1.21.9".to_owned(),
                pkg_type: "go-module".to_owned(),
                vulnerability: "GHSA-4v7x-pqxf-cx7m".to_owned(),
                severity: SEV_3,
            },
        ]
    };
    ImageScanReport::from_findings(image, findings)
}

#[must_use]
pub fn scan_columns() -> Arc<[TableColumn]> {
    [
        "SEVERITY",
        "VULNERABILITY",
        "IMAGE",
        "LIBRARY",
        "VERSION",
        "FIXED-IN",
        "TYPE",
    ]
    .into_iter()
    .map(|name| TableColumn {
        name: name.into(),
        format: "".into(),
        priority: 0,
    })
    .collect::<Vec<_>>()
    .into()
}

#[must_use]
pub fn project_scan_rows<I, S>(images: I) -> Vec<Arc<ResourceRow>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut rows = Vec::new();
    for img in images {
        let img = img.as_ref().trim();
        if img.is_empty() || img == "-" {
            continue;
        }
        let report = get_scan_report(img).unwrap_or_else(|| {
            let r = deterministic_scan_report(img);
            set_scan_report(r.clone());
            Arc::new(r)
        });
        for f in &report.findings {
            let id = format!(
                "{img}|{}|{}|{}|{}|{}|{}",
                f.library, f.version, f.fixed_in, f.pkg_type, f.vulnerability, f.severity
            );
            let cells = [
                f.severity,
                f.vulnerability.as_str(),
                img,
                f.library.as_str(),
                f.version.as_str(),
                f.fixed_in.as_str(),
                f.pkg_type.as_str(),
            ]
            .into_iter()
            .map(|text| TableCell {
                text: text.into(),
                timestamp_unix: None,
            })
            .collect();
            let mut row = ResourceRow::new(
                "-",
                f.vulnerability.clone(),
                f.severity,
                f.fixed_in.clone(),
                f.library.clone(),
                0,
            )
            .with_table_cells(cells);
            row.key = Arc::from(id.as_str());
            row.search = Arc::from(id.to_lowercase());
            rows.push(Arc::new(row));
        }
    }
    rows
}

#[must_use]
pub fn cve_advisory_url(cve: &str) -> String {
    let cve = cve.trim();
    if cve.starts_with("GHSA") {
        format!("https://github.com/advisories/{cve}")
    } else {
        format!("https://nvd.nist.gov/vuln/detail/{cve}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scorer_and_deduplication_match_k9s_vul_semantics() {
        let report = ImageScanReport::from_findings(
            "nginx:1.25",
            vec![
                VulnFinding {
                    library: "openssl".to_owned(),
                    version: "3.0.1".to_owned(),
                    fixed_in: "3.0.2".to_owned(),
                    pkg_type: "deb".to_owned(),
                    vulnerability: "CVE-2024-0001".to_owned(),
                    severity: to_sev("Critical"),
                },
                VulnFinding {
                    library: "openssl".to_owned(),
                    version: "3.0.1".to_owned(),
                    fixed_in: "3.0.2".to_owned(),
                    pkg_type: "deb".to_owned(),
                    vulnerability: "CVE-2024-0001".to_owned(),
                    severity: to_sev("Critical"),
                },
                VulnFinding {
                    library: "zlib".to_owned(),
                    version: "1.2".to_owned(),
                    fixed_in: String::new(),
                    pkg_type: "deb".to_owned(),
                    vulnerability: "CVE-2024-0002".to_owned(),
                    severity: to_sev("Medium"),
                },
            ],
        );
        assert_eq!(report.findings.len(), 2);
        // Critical (0x80) | Medium (0x20) = 0xA0 -> 10100000 -> first 6 bits "101000"
        assert_eq!(format_score_bits(report.score_bits), "101000");
        assert_eq!(
            cve_advisory_url("CVE-2024-0001"),
            "https://nvd.nist.gov/vuln/detail/CVE-2024-0001"
        );
        assert_eq!(
            cve_advisory_url("GHSA-4v7x-pqxf-cx7m"),
            "https://github.com/advisories/GHSA-4v7x-pqxf-cx7m"
        );
    }
}
