use super::*;

const NOW_MS: i64 = 1_800_000_000_000;

fn track() -> Track {
    Track {
        video_id: "dQw4w9WgXcQ".into(),
        title: "Never Gonna Give You Up".into(),
        artist: "Rick Astley".into(),
        album: Some("Whenever You Need Somebody".into()),
        artwork_url: Some("https://lh3.googleusercontent.com/abc=w544-h544".into()),
        duration: 213.0,
        position: 30.5,
        paused: false,
    }
}

#[test]
fn clamps_text_to_discord_limits() {
    assert_eq!(clamp_text("  Song  ").as_deref(), Some("Song"));
    assert_eq!(clamp_text("   "), None);
    assert_eq!(clamp_text("A").map(|t| utf16_len(&t)), Some(TEXT_MIN_UNITS));

    let long = "é".repeat(TEXT_MAX_UNITS + 10);
    let clamped = clamp_text(&long).unwrap();
    assert_eq!(utf16_len(&clamped), TEXT_MAX_UNITS);
    assert!(clamped.ends_with(ELLIPSIS));

    let exact = "x".repeat(TEXT_MAX_UNITS);
    assert_eq!(clamp_text(&exact).as_deref(), Some(exact.as_str()));
}

#[test]
fn clamps_astral_text_in_utf16_units() {
    // 100 characters but 200 UTF-16 units, as Discord counts them.
    let fancy = "𝓯".repeat(100);
    let clamped = clamp_text(&fancy).unwrap();
    assert!(utf16_len(&clamped) <= TEXT_MAX_UNITS);
    assert_eq!(clamped, format!("{}{ELLIPSIS}", "𝓯".repeat(63)));

    let exact = "😀".repeat(TEXT_MAX_UNITS / 2);
    assert_eq!(clamp_text(&exact).as_deref(), Some(exact.as_str()));

    // An emoji that would straddle the limit is dropped whole, never split.
    let straddling = format!("{}😀😀", "a".repeat(126));
    assert_eq!(
        clamp_text(&straddling).unwrap(),
        format!("{}{ELLIPSIS}", "a".repeat(126))
    );
}

fn validate(track: Track) -> Result<Presence, &'static str> {
    track.validate(NOW_MS)
}

#[test]
fn playing_track_spans_from_position() {
    let presence = validate(track()).unwrap();
    assert_eq!(
        presence.url,
        "https://music.youtube.com/watch?v=dQw4w9WgXcQ"
    );
    assert_eq!(
        presence.span,
        Some((NOW_MS - 30_500, NOW_MS - 30_500 + 213_000))
    );
}

#[test]
fn paused_or_unknown_duration_has_no_span() {
    let paused = validate(Track {
        paused: true,
        ..track()
    })
    .unwrap();
    assert_eq!(paused.span, None);
    let live = validate(Track {
        duration: 0.0,
        position: 900.0,
        ..track()
    })
    .unwrap();
    assert_eq!(live.span, None);
}

#[test]
fn rejects_missing_title_or_artist() {
    assert!(
        validate(Track {
            title: " ".into(),
            ..track()
        })
        .is_err()
    );
    assert!(
        validate(Track {
            artist: String::new(),
            ..track()
        })
        .is_err()
    );
}

#[test]
fn rejects_malformed_video_ids() {
    for id in [
        "",
        "dQw4w9WgXc",
        "dQw4w9WgXcQQ",
        "dQw4w9WgX/Q",
        "dQw4w9WgXc&",
        "dQw4w9WgXcé",
    ] {
        assert!(
            validate(Track {
                video_id: id.into(),
                ..track()
            })
            .is_err(),
            "{id}"
        );
    }
    assert!(
        validate(Track {
            video_id: "a-_09AZaz-_".into(),
            ..track()
        })
        .is_ok()
    );
}

#[test]
fn rejects_out_of_range_numbers() {
    for duration in [-1.0, f64::NAN, f64::INFINITY, MAX_DURATION_SECS + 1.0] {
        assert!(
            validate(Track {
                duration,
                ..track()
            })
            .is_err(),
            "{duration}"
        );
    }
    for position in [-0.1, f64::NAN, 213.0 + POSITION_SLACK_SECS + 1.0] {
        assert!(
            validate(Track {
                position,
                ..track()
            })
            .is_err(),
            "{position}"
        );
    }
    assert!(
        validate(Track {
            position: 214.0,
            ..track()
        })
        .is_ok()
    );
}

#[test]
fn keeps_artwork_only_from_youtube_image_hosts() {
    let artwork = |url: &str| {
        validate(Track {
            artwork_url: Some(url.into()),
            ..track()
        })
        .unwrap()
        .artwork_url
    };
    assert!(artwork("https://i.ytimg.com/vi/dQw4w9WgXcQ/hqdefault.jpg").is_some());
    assert!(artwork("https://lh3.googleusercontent.com/abc=w544-h544").is_some());
    assert!(artwork("https://ytimg.com/x.jpg").is_some());

    assert!(artwork("http://i.ytimg.com/x.jpg").is_none());
    assert!(artwork("https://evilytimg.com/x.jpg").is_none());
    assert!(artwork("https://i.ytimg.com.evil.com/x.jpg").is_none());
    assert!(artwork("https://googleusercontent.com.evil.com/x.jpg").is_none());
    assert!(artwork("javascript:alert(1)").is_none());
    assert!(artwork("not a url").is_none());
    let too_long = format!("https://i.ytimg.com/{}", "a".repeat(URL_MAX_CHARS));
    assert!(artwork(&too_long).is_none());
}

#[test]
fn dedupe_ignores_jitter_but_not_seeks_or_pauses() {
    let base = validate(track()).unwrap();
    let jitter = track().validate(NOW_MS + 1_500).unwrap();
    let seeked = track().validate(NOW_MS + 5_000).unwrap();
    let paused = validate(Track {
        paused: true,
        ..track()
    })
    .unwrap();
    assert!(base.looks_like(&jitter));
    assert!(!base.looks_like(&seeked));
    assert!(!base.looks_like(&paused));
}
