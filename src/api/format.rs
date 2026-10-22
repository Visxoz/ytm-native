//! Counts, durations and running times as the UI words them.

use super::Track;

/// 1234567 → "1.2M".
pub fn compact(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => trim_decimal(n as f64 / 1e3, "K"),
        1_000_000..1_000_000_000 => trim_decimal(n as f64 / 1e6, "M"),
        _ => trim_decimal(n as f64 / 1e9, "B"),
    }
}

fn trim_decimal(value: f64, suffix: &str) -> String {
    if value >= 100.0 {
        format!("{value:.0}{suffix}")
    } else {
        format!("{:.1}", value).trim_end_matches(".0").to_owned() + suffix
    }
}

/// "1 song", "12 songs".
pub fn songs(n: u64) -> String {
    if n == 1 {
        "1 song".to_owned()
    } else {
        format!("{n} songs")
    }
}

/// 3725 → "1:02:05".
pub fn format_duration(secs: u32) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Total running time as "1 hr 5 min" or "42 min".
pub fn total_time(tracks: &[Track]) -> Option<String> {
    let total: u32 = tracks.iter().filter_map(|t| t.duration).sum();
    if total == 0 {
        return None;
    }
    let minutes = total.div_ceil(60);
    Some(if minutes >= 60 {
        format!("{} hr {} min", minutes / 60, minutes % 60)
    } else {
        format!("{minutes} min")
    })
}

pub fn playlist_meta(count: Option<u64>, tracks: &[Track]) -> String {
    let mut meta = vec![songs(count.unwrap_or(tracks.len() as u64))];
    meta.extend(total_time(tracks));
    meta.join(" • ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_numbers_and_durations() {
        assert_eq!(compact(999), "999");
        assert_eq!(compact(1_000), "1K");
        assert_eq!(compact(1_260_000), "1.3M");
        assert_eq!(compact(312_000), "312K");
        assert_eq!(format_duration(59), "0:59");
        assert_eq!(format_duration(3725), "1:02:05");
    }

    #[test]
    fn totals_running_time() {
        let t = |d| Track {
            id: String::new(),
            title: String::new(),
            artists: Vec::new(),
            album: None,
            duration: Some(d),
            thumb: None,
            track_nr: None,
            video: false,
        };
        assert_eq!(
            total_time(&[t(1500), t(2401)]).as_deref(),
            Some("1 hr 6 min")
        );
        assert_eq!(total_time(&[t(61)]).as_deref(), Some("2 min"));
        assert_eq!(total_time(&[]), None);
    }
}
