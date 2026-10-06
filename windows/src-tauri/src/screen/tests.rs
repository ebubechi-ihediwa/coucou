// The screenshot pipeline without a screen: frames made in memory, run through the
// real limits, resize and encoder.

use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::json;

use super::*;

/// A frame of one BGRA colour.
fn solid(width: u32, height: u32, bgra: [u8; 4]) -> RawFrame {
    RawFrame {
        width,
        height,
        bgra: (0..width * height).flat_map(|_| bgra).collect(),
    }
}

/// A frame of noise: the worst case for a JPEG.
fn noise(width: u32, height: u32) -> RawFrame {
    let mut seed = 99u32;
    RawFrame {
        width,
        height,
        bgra: (0..width * height * 4)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed >> 24) as u8
            })
            .collect(),
    }
}

fn no_cancel() -> AtomicBool {
    AtomicBool::new(false)
}

fn dimensions(jpeg: &[u8]) -> (u32, u32) {
    let sof = jpeg
        .windows(2)
        .position(|w| w == [0xFF, 0xC0])
        .expect("a frame header");
    (
        u32::from(u16::from_be_bytes([jpeg[sof + 7], jpeg[sof + 8]])),
        u32::from(u16::from_be_bytes([jpeg[sof + 5], jpeg[sof + 6]])),
    )
}

// ── Fitting to the limits ─────────────────────────────────────────────────────

#[test]
fn the_documented_limits_are_the_ones_in_force() {
    assert_eq!(LIMITS.max_long_edge, 1568);
    assert_eq!(LIMITS.max_out_pixels, 1_150_000);
    assert_eq!(LIMITS.max_encoded_bytes, 1_500_000);
    assert_eq!(LIMITS.max_source_pixels, 36_000_000);
}

// Checked when it is compiled: an 8K screen (33.2 M pixels) is allowed in and a 12K one is
// not, and the biggest picture, as base64, sits well inside the request cap.
const _: () = {
    assert!(7680u64 * 4320 <= LIMITS.max_source_pixels);
    assert!(11_520u64 * 6480 > LIMITS.max_source_pixels);
    assert!(LIMITS.max_encoded_bytes * 4 / 3 < MAX_REQUEST_BYTES / 2);
};

#[test]
fn common_screens_are_brought_down_to_the_limits_and_keep_their_shape() {
    for (w, h, expected) in [
        (1920, 1080, (1429, 804)),
        (2560, 1440, (1429, 804)),
        (3840, 2160, (1429, 804)),
        (1366, 768, (1366, 768)),
        (800, 600, (800, 600)),
        (1, 1, (1, 1)),
    ] {
        let got = fit(w, h, &LIMITS);
        assert_eq!(got, expected, "{w}x{h}");
        let (ratio_in, ratio_out) = (
            f64::from(w) / f64::from(h),
            f64::from(got.0) / f64::from(got.1),
        );
        assert!((ratio_in - ratio_out).abs() < 0.01, "{w}x{h} -> {got:?}");
    }
}

#[test]
fn whatever_the_screen_the_limits_hold_and_nothing_is_enlarged() {
    let sizes = [
        (1, 1),
        (1, 5000),
        (5000, 1),
        (10, 5000),
        (3440, 1440),
        (5120, 1440),
        (7680, 4320),
        (4096, 4096),
        (1568, 1568),
        (1500, 700),
        (1920, 1200),
        (640, 480),
        (65_535, 2),
    ];
    for (w, h) in sizes {
        let (dw, dh) = fit(w, h, &LIMITS);
        assert!(dw >= 1 && dh >= 1, "{w}x{h}");
        assert!(
            dw <= w && dh <= h,
            "{w}x{h} -> {dw}x{dh} is bigger than the original"
        );
        assert!(dw.max(dh) <= LIMITS.max_long_edge, "{w}x{h} -> {dw}x{dh}");
        assert!(
            u64::from(dw) * u64::from(dh) <= u64::from(LIMITS.max_out_pixels),
            "{w}x{h} -> {dw}x{dh}"
        );
    }
}

// ── Resizing ──────────────────────────────────────────────────────────────────

#[test]
fn every_destination_pixel_is_a_weighted_average_of_its_source() {
    for (source, destination) in [(10, 5), (7, 3), (1920, 1430), (5, 5), (100, 1), (3, 2)] {
        let spans = spans(source, destination);
        assert_eq!(spans.len(), destination);
        for (d, sources) in spans.iter().enumerate() {
            let total: f32 = sources.iter().map(|&(_, w)| w).sum();
            assert!(
                (total - 1.0).abs() < 1e-4,
                "{source}->{destination} #{d}: {total}"
            );
            assert!(sources.iter().all(|&(s, w)| s < source && w > 0.0));
        }
    }
    // Same size: each destination pixel is exactly one source pixel.
    assert!(spans(6, 6)
        .iter()
        .enumerate()
        .all(|(d, s)| s.len() == 1 && s[0].0 == d));
}

#[test]
fn a_solid_colour_stays_that_colour_and_blue_green_red_becomes_red_green_blue() {
    // The system gives BGRA; pixel B=10, G=20, R=30.
    let rgb = resize(&solid(40, 20, [10, 20, 30, 255]), 40, 20, &no_cancel()).unwrap();
    assert_eq!(&rgb[..3], &[30, 20, 10]);
    let shrunk = resize(&solid(40, 20, [10, 20, 30, 255]), 13, 7, &no_cancel()).unwrap();
    assert_eq!(shrunk.len(), 13 * 7 * 3);
    assert!(shrunk.chunks(3).all(|p| p == [30, 20, 10]));
}

#[test]
fn shrinking_averages_so_fine_detail_does_not_vanish_or_alias() {
    // A one-pixel checkerboard of black and white, halved: mid grey everywhere.
    let mut frame = solid(8, 8, [0, 0, 0, 255]);
    for y in 0..8usize {
        for x in 0..8usize {
            if (x + y) % 2 == 0 {
                frame.bgra[(y * 8 + x) * 4..(y * 8 + x) * 4 + 3].copy_from_slice(&[255, 255, 255]);
            }
        }
    }
    let rgb = resize(&frame, 4, 4, &no_cancel()).unwrap();
    assert!(rgb.iter().all(|&v| (126..=129).contains(&v)), "{rgb:?}");
}

#[test]
fn a_stopped_request_stops_resizing() {
    let stop = AtomicBool::new(true);
    assert_eq!(
        resize(&solid(300, 200, [1, 2, 3, 255]), 100, 60, &stop).unwrap_err(),
        ScreenError::Cancelled
    );
    assert_eq!(
        resize(&solid(300, 200, [1, 2, 3, 255]), 300, 200, &stop).unwrap_err(),
        ScreenError::Cancelled
    );
}

// ── The whole pipeline ────────────────────────────────────────────────────────

#[test]
fn a_big_screen_becomes_a_small_valid_jpeg_inside_the_limits() {
    let shot = prepare(
        solid(3840, 2160, [200, 120, 40, 255]),
        &LIMITS,
        &no_cancel(),
    )
    .unwrap();
    assert_eq!((shot.width, shot.height), (1429, 804));
    assert_eq!(dimensions(shot.jpeg()), (1429, 804));
    assert_eq!(&shot.jpeg()[..2], &[0xFF, 0xD8]);
    assert_eq!(&shot.jpeg()[shot.len() - 2..], &[0xFF, 0xD9]);
    assert!(shot.len() <= LIMITS.max_encoded_bytes);
    assert_eq!(shot.media_type(), "image/jpeg");
}

#[test]
fn a_small_screen_is_not_enlarged() {
    let shot = prepare(solid(640, 360, [255, 255, 255, 255]), &LIMITS, &no_cancel()).unwrap();
    assert_eq!((shot.width, shot.height), (640, 360));
}

#[test]
fn a_frame_that_is_not_what_it_claims_is_refused() {
    let cancel = no_cancel();
    assert_eq!(
        prepare(solid(0, 10, [0; 4]), &LIMITS, &cancel).unwrap_err(),
        ScreenError::CaptureFailed
    );
    assert_eq!(
        prepare(solid(10, 0, [0; 4]), &LIMITS, &cancel).unwrap_err(),
        ScreenError::CaptureFailed
    );
    for wrong in [0usize, 7, 4 * 10 * 10 - 1, 4 * 10 * 10 + 1] {
        let frame = RawFrame {
            width: 10,
            height: 10,
            bgra: vec![0; wrong],
        };
        assert_eq!(
            prepare(frame, &LIMITS, &cancel).unwrap_err(),
            ScreenError::CaptureFailed,
            "{wrong} bytes"
        );
    }
}

#[test]
fn a_display_over_the_pixel_limit_is_refused_before_it_is_processed() {
    let tight = Limits {
        max_source_pixels: 1_000,
        ..LIMITS
    };
    assert_eq!(
        prepare(solid(40, 40, [0; 4]), &tight, &no_cancel()).unwrap_err(),
        ScreenError::TooLarge
    );
    // Right at the limit is fine.
    assert!(prepare(solid(40, 25, [0; 4]), &tight, &no_cancel()).is_ok());
}

#[test]
fn a_picture_that_will_not_fit_the_byte_limit_is_refused_not_sent() {
    let tiny = Limits {
        max_encoded_bytes: 200,
        qualities: &[80, 40],
        ..LIMITS
    };
    assert_eq!(
        prepare(noise(300, 200), &tiny, &no_cancel()).unwrap_err(),
        ScreenError::TooLarge
    );
}

#[test]
fn a_lower_quality_is_tried_when_the_best_is_too_big() {
    let frame = || noise(200, 150);
    let at = |q: u8| {
        prepare(
            frame(),
            &Limits {
                qualities: std::slice::from_ref(Box::leak(Box::new(q))),
                ..LIMITS
            },
            &no_cancel(),
        )
        .unwrap()
        .len()
    };
    let (high, low) = (at(90), at(20));
    assert!(low < high);
    // A cap between the two: quality 90 does not fit, 20 does, so 20 is what is sent.
    let between = Limits {
        max_encoded_bytes: (high + low) / 2,
        qualities: &[90, 20],
        ..LIMITS
    };
    let shot = prepare(frame(), &between, &no_cancel()).unwrap();
    assert_eq!(shot.len(), low);
    assert!(shot.len() <= between.max_encoded_bytes);
}

#[test]
fn a_stopped_request_gives_nothing_back() {
    let stop = AtomicBool::new(true);
    assert_eq!(
        prepare(solid(100, 100, [1; 4]), &LIMITS, &stop).unwrap_err(),
        ScreenError::Cancelled
    );
    // Stopped in the middle: the flag is read between steps, so it takes effect on the next one.
    let flag = AtomicBool::new(false);
    let frame = solid(2000, 1200, [9, 9, 9, 255]);
    flag.store(true, Ordering::Relaxed);
    assert_eq!(
        prepare(frame, &LIMITS, &flag).unwrap_err(),
        ScreenError::Cancelled
    );
}

// ── What cannot leak ──────────────────────────────────────────────────────────

#[test]
fn a_screenshot_prints_its_size_and_nothing_of_its_contents() {
    let shot = prepare(solid(16, 16, [250, 5, 7, 255]), &LIMITS, &no_cancel()).unwrap();
    let shown = format!("{shot:?}");
    assert_eq!(
        shown,
        format!("Screenshot(16x16, {} bytes, contents hidden)", shot.len())
    );
    // The structures that hold one print the same way.
    let wrapped = format!("{:#?}", Some(&shot));
    assert!(
        wrapped.contains("contents hidden") && !wrapped.contains("255,"),
        "{wrapped}"
    );
}

#[test]
fn every_error_message_is_plain_and_gives_nothing_away() {
    for error in [
        ScreenError::Unavailable,
        ScreenError::PermissionDenied,
        ScreenError::CaptureFailed,
        ScreenError::TooLarge,
        ScreenError::EncodingFailed,
        ScreenError::Cancelled,
    ] {
        let message = error.message();
        assert!(message.ends_with('.') && message.len() > 20, "{message}");
        for bad in [
            "\\",
            "C:",
            "0x",
            "HRESULT",
            "panicked",
            "error code",
            "bytes",
            "http",
        ] {
            assert!(!message.contains(bad), "{message} contains {bad}");
        }
    }
}

// ── What the model is offered ─────────────────────────────────────────────────

#[test]
fn the_tool_takes_nothing_and_says_so_in_a_closed_schema() {
    let tool = tool_definition();
    assert_eq!(tool["name"], "capture_screen");
    assert_eq!(tool["input_schema"]["type"], "object");
    assert_eq!(tool["input_schema"]["additionalProperties"], false);
    assert_eq!(tool["input_schema"]["properties"], json!({}));
    assert!(tool["input_schema"].get("required").is_none());
    // Nothing the model could aim the capture with.
    let text = tool.to_string().to_lowercase();
    assert!(
        !text.contains("\"path\"") && !text.contains("\"file\"") && !text.contains("display_id")
    );
    // And the description tells it to be sparing.
    let description = tool["description"].as_str().unwrap();
    assert!(description.contains("only when") && description.contains("not kept"));
}

#[test]
fn a_call_with_any_argument_is_not_a_valid_call() {
    for good in [json!(null), json!({})] {
        assert!(input_is_valid(&good), "{good}");
    }
    for bad in [
        json!({ "display": 1 }),
        json!({ "path": "C:\\Users\\x\\a.png" }),
        json!({ "region": [0, 0, 10, 10] }),
        json!({ "x": null }),
        json!([]),
        json!("full"),
        json!(1),
        json!(true),
    ] {
        assert!(!input_is_valid(&bad), "{bad}");
    }
}

#[test]
fn only_models_that_can_read_images_are_sent_one() {
    for yes in [
        "claude-opus-5",
        "claude-sonnet-5",
        "claude-haiku-4-5",
        "claude-3-5-sonnet-20241022",
        " Claude-Opus-5 ",
    ] {
        assert!(model_accepts_images(yes), "{yes}");
    }
    for no in [
        "claude-2.1",
        "claude-2",
        "claude-instant-1.2",
        "gpt-4o",
        "",
        "opus",
        "gemini-pro",
        "claude",
    ] {
        assert!(!model_accepts_images(no), "{no}");
    }
}

#[test]
fn the_models_the_settings_window_offers_can_all_see() {
    for offered in [
        "claude-opus-5",
        "claude-sonnet-5",
        "claude-haiku-4-5",
        crate::claude::DEFAULT_MODEL,
    ] {
        assert!(model_accepts_images(offered), "{offered}");
    }
}
