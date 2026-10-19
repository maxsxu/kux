use std::{env, process::Command};

fn main() {
    for name in [
        "KUX_BUILD_COMMIT",
        "KUX_BUILD_DATE",
        "SOURCE_DATE_EPOCH",
        "TARGET",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    // Also handle worktrees, whose .git entry is a file pointing at shared metadata.
    for name in ["HEAD", "index", "packed-refs"] {
        if let Some(path) = git(&["rev-parse", "--git-path", name]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    if let Some(reference) = git(&["symbolic-ref", "-q", "HEAD"])
        && let Some(path) = git(&["rev-parse", "--git-path", &reference])
    {
        println!("cargo:rerun-if-changed={path}");
    }
    let commit = env::var("KUX_BUILD_COMMIT")
        .ok()
        .filter(|value| metadata_value(value))
        .or_else(|| git(&["rev-parse", "HEAD"]))
        .unwrap_or_else(|| "unknown".to_owned());
    let date = env::var("KUX_BUILD_DATE")
        .ok()
        .filter(|value| valid_utc_date(value))
        .or_else(|| {
            env::var("SOURCE_DATE_EPOCH")
                .ok()?
                .parse()
                .ok()
                .and_then(format_epoch)
        })
        .unwrap_or_else(|| "unknown".to_owned());
    let target = env::var("TARGET").unwrap_or_else(|_| "unknown".to_owned());
    println!("cargo:rustc-env=KUX_BUILD_COMMIT={commit}");
    println!("cargo:rustc-env=KUX_BUILD_DATE={date}");
    println!("cargo:rustc-env=KUX_BUILD_TARGET={target}");
}

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(env::var_os("CARGO_MANIFEST_DIR")?)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    metadata_value(&value).then_some(value)
}

fn metadata_value(value: &str) -> bool {
    !value.is_empty() && !value.chars().any(char::is_control)
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        2 => {
            28 + u32::from(
                year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)),
            )
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn valid_utc_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'Z'
        || !bytes.iter().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16 | 19) || byte.is_ascii_digit()
        })
    {
        return false;
    }
    let part = |range: std::ops::Range<usize>| value[range].parse::<u32>().unwrap_or(u32::MAX);
    let (year, month, day) = (part(0..4), part(5..7), part(8..10));
    (1..=12).contains(&month)
        && (1..=days_in_month(year, month)).contains(&day)
        && part(11..13) < 24
        && part(14..16) < 60
        && part(17..19) < 60
}

fn format_epoch(seconds: u64) -> Option<String> {
    if seconds > 253_402_300_799 {
        return None;
    }
    let mut days = seconds / 86_400;
    let mut year = 1970;
    loop {
        let year_days = 337 + u64::from(days_in_month(year, 2));
        if days < year_days {
            break;
        }
        days -= year_days;
        year += 1;
    }
    let mut month = 1;
    while days >= u64::from(days_in_month(year, month)) {
        days -= u64::from(days_in_month(year, month));
        month += 1;
    }
    Some(format!(
        "{year:04}-{month:02}-{:02}T{:02}:{:02}:{:02}Z",
        days + 1,
        seconds % 86_400 / 3_600,
        seconds % 3_600 / 60,
        seconds % 60,
    ))
}

#[cfg(test)]
mod tests {
    use super::{format_epoch, valid_utc_date};

    #[test]
    fn dates_are_valid_utc_seconds_and_epoch_is_reproducible() {
        for (epoch, expected) in [
            (0, "1970-01-01T00:00:00Z"),
            (951_782_400, "2000-02-29T00:00:00Z"),
            (1_000_000_000, "2001-09-09T01:46:40Z"),
            (253_402_300_799, "9999-12-31T23:59:59Z"),
        ] {
            assert_eq!(format_epoch(epoch).as_deref(), Some(expected));
            assert!(valid_utc_date(expected));
        }
        assert!(format_epoch(253_402_300_800).is_none());
        for invalid in [
            "2026-10-04",
            "2026-10-04T12:34:56+00:00",
            "2026-10-04T12:34:56.000Z",
            "1900-02-29T00:00:00Z",
            "2026-13-01T00:00:00Z",
            "2026-10-04T24:00:00Z",
        ] {
            assert!(!valid_utc_date(invalid));
        }
    }
}
