// Screen awareness: when the person asks Mochi to look at their screen, one picture
// of the display they are working on goes to the model with that request, and is then
// gone. Nothing here watches the screen: there is no timer, no stream and no loop,
// and a capture only ever happens inside a request the person made.
//
//     capture (GDI, in memory) → fit to the limits → JPEG → handed to the request → dropped
//
// The screenshot is sensitive (passwords, messages, keys: anything on screen), and
// there is no reliable way to redact an arbitrary picture. So it is treated as data
// that must not leak by accident: it has no `Debug` that shows its bytes, it is never
// written to disk or to the log, it is not kept in the conversation, and it is dropped
// as soon as the request that needed it is over. It is, by design, sent to the model
// provider as part of that request; that is what the person asked for.

pub mod jpeg;

use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{json, Value};

use crate::platform;

pub const TOOL_NAME: &str = "capture_screen";

/// How big a screenshot may be, at each step. Written down once, here.
pub struct Limits {
    /// The most pixels a display may have to be captured at all (an 8K screen is
    /// 33.2 M). Past this the capture is refused before anything is allocated.
    pub max_source_pixels: u64,
    /// The longest side of what is sent. 1568 px is where the model's own pipeline
    /// starts to shrink an image, so more would only cost upload time.
    pub max_long_edge: u32,
    /// The most pixels of what is sent (~1.15 M, again the model's own limit).
    pub max_out_pixels: u32,
    /// The most bytes of the encoded JPEG. A screen is typically 150–500 KB.
    pub max_encoded_bytes: usize,
    /// JPEG qualities to try, best first, until one fits `max_encoded_bytes`.
    pub qualities: &'static [u8],
}

pub const LIMITS: Limits = Limits {
    max_source_pixels: 36_000_000,
    max_long_edge: 1568,
    max_out_pixels: 1_150_000,
    max_encoded_bytes: 1_500_000,
    qualities: &[80, 65, 50, 35],
};

/// The most the whole request may weigh with a screenshot in it (the conversation,
/// any attached file, and the image as base64). Past this the screenshot is left out
/// and the model is told why, rather than sending a request of unknown size.
pub const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;

// ── What can go wrong ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenError {
    /// There is no screen to capture (no display, or this platform has no capture yet).
    Unavailable,
    /// The system would not let Coucou capture.
    PermissionDenied,
    /// The system started the capture and it failed.
    CaptureFailed,
    /// The display, or the encoded picture, is bigger than the limits allow.
    TooLarge,
    EncodingFailed,
    /// The person stopped it.
    Cancelled,
}

impl ScreenError {
    /// What may be shown or told to the model: the kind of failure, never a path, a
    /// system error code or anything from the picture.
    pub fn message(&self) -> &'static str {
        match self {
            ScreenError::Unavailable => "The screen can't be captured on this system.",
            ScreenError::PermissionDenied => "Windows did not allow capturing the screen.",
            ScreenError::CaptureFailed => "The screen could not be captured.",
            ScreenError::TooLarge => "The screen is too large to capture.",
            ScreenError::EncodingFailed => "The screenshot could not be prepared.",
            ScreenError::Cancelled => "The screen capture was cancelled.",
        }
    }
}

// ── The pictures ──────────────────────────────────────────────────────────────

/// What the system hands over: a display's pixels, BGRA, row after row.
pub struct RawFrame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// A screenshot ready to send. It shows its size and nothing else when printed, so it
/// cannot reach a log through a stray `{:?}`.
pub struct Screenshot {
    jpeg: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl Screenshot {
    pub fn jpeg(&self) -> &[u8] {
        &self.jpeg
    }

    pub fn media_type(&self) -> &'static str {
        "image/jpeg"
    }

    pub fn len(&self) -> usize {
        self.jpeg.len()
    }

    /// A screenshot with whatever bytes a test wants, for the tests of what is done with one.
    #[cfg(test)]
    pub(crate) fn fake(width: u32, height: u32, jpeg: Vec<u8>) -> Screenshot {
        Screenshot {
            jpeg,
            width,
            height,
        }
    }
}

impl std::fmt::Debug for Screenshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Screenshot({}x{}, {} bytes, contents hidden)",
            self.width,
            self.height,
            self.jpeg.len()
        )
    }
}

/// Something that can take one screenshot. `cancel` is looked at between the steps, so
/// a request the person stopped does not carry on resizing and encoding.
pub trait ScreenCapture: Send + Sync + 'static {
    fn capture(&self, cancel: &AtomicBool) -> Result<Screenshot, ScreenError>;
}

/// The real screen.
pub struct SystemCapture;

impl ScreenCapture for SystemCapture {
    fn capture(&self, cancel: &AtomicBool) -> Result<Screenshot, ScreenError> {
        let frame = platform::capture::grab()?;
        prepare(frame, &LIMITS, cancel)
    }
}

// ── Making it fit ─────────────────────────────────────────────────────────────

/// The size to send for a display of `width` × `height`: the same shape, no side longer
/// than `max_long_edge`, no more than `max_out_pixels` pixels, never bigger than the
/// original, at least 1 × 1.
pub fn fit(width: u32, height: u32, limits: &Limits) -> (u32, u32) {
    let (w, h) = (f64::from(width), f64::from(height));
    let mut scale = 1.0f64;
    let long = w.max(h);
    if long > f64::from(limits.max_long_edge) {
        scale = f64::from(limits.max_long_edge) / long;
    }
    let pixels = w * h * scale * scale;
    if pixels > f64::from(limits.max_out_pixels) {
        scale *= (f64::from(limits.max_out_pixels) / pixels).sqrt();
    }
    // Rounded down, so the limits hold exactly.
    (
        ((w * scale).floor() as u32).max(1),
        ((h * scale).floor() as u32).max(1),
    )
}

/// Which source pixels make up each destination pixel, and how much of each: the
/// average over the stretch of the source that the destination pixel covers.
fn spans(source: usize, destination: usize) -> Vec<Vec<(usize, f32)>> {
    let scale = source as f64 / destination as f64;
    (0..destination)
        .map(|d| {
            let (start, end) = (d as f64 * scale, (d + 1) as f64 * scale);
            let first = start.floor() as usize;
            let last = ((end.ceil() as usize).max(first + 1) - 1).min(source - 1);
            (first..=last)
                .map(|s| {
                    let overlap = end.min((s + 1) as f64) - start.max(s as f64);
                    (s, (overlap / scale) as f32)
                })
                .collect()
        })
        .collect()
}

/// The frame as RGB (three bytes a pixel) at `width` × `height`, by averaging, so thin
/// text stays readable. Looks at `cancel` every few rows.
fn resize(
    frame: &RawFrame,
    width: u32,
    height: u32,
    cancel: &AtomicBool,
) -> Result<Vec<u8>, ScreenError> {
    let (sw, sh) = (frame.width as usize, frame.height as usize);
    let (dw, dh) = (width as usize, height as usize);
    let stop = || {
        if cancel.load(Ordering::Relaxed) {
            Err(ScreenError::Cancelled)
        } else {
            Ok(())
        }
    };

    if (sw, sh) == (dw, dh) {
        let mut rgb = Vec::with_capacity(sw * sh * 3);
        for (row, pixels) in frame.bgra.chunks_exact(sw * 4).enumerate() {
            if row % 64 == 0 {
                stop()?;
            }
            rgb.extend(pixels.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0]]));
        }
        return Ok(rgb);
    }

    // Across, then down: each pass is an average, so the two together are a box filter.
    let across = spans(sw, dw);
    let down = spans(sh, dh);
    let mut middle = vec![0f32; sh * dw * 3];
    for (y, row) in frame.bgra.chunks_exact(sw * 4).enumerate() {
        if y % 64 == 0 {
            stop()?;
        }
        for (x, sources) in across.iter().enumerate() {
            let (mut r, mut g, mut b) = (0f32, 0f32, 0f32);
            for &(s, weight) in sources {
                let p = &row[s * 4..s * 4 + 4];
                (r, g, b) = (
                    r + f32::from(p[2]) * weight,
                    g + f32::from(p[1]) * weight,
                    b + f32::from(p[0]) * weight,
                );
            }
            let at = (y * dw + x) * 3;
            middle[at..at + 3].copy_from_slice(&[r, g, b]);
        }
    }
    let mut rgb = vec![0u8; dw * dh * 3];
    for (y, sources) in down.iter().enumerate() {
        if y % 64 == 0 {
            stop()?;
        }
        for x in 0..dw {
            for c in 0..3 {
                let value: f32 = sources
                    .iter()
                    .map(|&(s, weight)| middle[(s * dw + x) * 3 + c] * weight)
                    .sum();
                rgb[(y * dw + x) * 3 + c] = value.round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    Ok(rgb)
}

/// Checks a frame against the limits, resizes it, and encodes it. The frame is
/// consumed: its memory is released as soon as it has been resized.
pub fn prepare(
    frame: RawFrame,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<Screenshot, ScreenError> {
    if frame.width == 0 || frame.height == 0 {
        return Err(ScreenError::CaptureFailed);
    }
    if u64::from(frame.width) * u64::from(frame.height) > limits.max_source_pixels {
        return Err(ScreenError::TooLarge);
    }
    if frame.bgra.len() != frame.width as usize * frame.height as usize * 4 {
        return Err(ScreenError::CaptureFailed);
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(ScreenError::Cancelled);
    }
    let (width, height) = fit(frame.width, frame.height, limits);
    let rgb = resize(&frame, width, height, cancel)?;
    drop(frame);

    for &quality in limits.qualities {
        if cancel.load(Ordering::Relaxed) {
            return Err(ScreenError::Cancelled);
        }
        let jpeg =
            jpeg::encode(width, height, &rgb, quality).map_err(|_| ScreenError::EncodingFailed)?;
        if jpeg.len() <= limits.max_encoded_bytes {
            return Ok(Screenshot {
                jpeg,
                width,
                height,
            });
        }
    }
    Err(ScreenError::TooLarge)
}

// ── What the model is offered ─────────────────────────────────────────────────

/// The tool that lets the model ask for a look. It takes nothing: not a display, a
/// region, a path or a file. What is captured is decided here, not by the model.
pub fn tool_definition() -> Value {
    json!({
        "name": TOOL_NAME,
        "description": "Take one screenshot of the display the user is working on, so you can see what they are looking at. \
    Call it only when the user's request is about what is on their screen (for example \"what am I looking at\", \"what does this error mean\", \"look at this code\"). \
    Do not call it for questions that do not need the screen. You get one screenshot per request; it is not kept afterwards.",
        "input_schema": {
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        },
    })
}

/// Whether a call to the tool is well formed: it takes no arguments at all.
pub fn input_is_valid(input: &Value) -> bool {
    match input {
        Value::Null => true,
        Value::Object(map) => map.is_empty(),
        _ => false,
    }
}

/// Whether a model can be sent a picture. Every current Claude model can; the early
/// ones (Claude 2, Claude Instant) cannot. Anything that is not a Claude model is not
/// assumed to.
pub fn model_accepts_images(model: &str) -> bool {
    let model = model.trim().to_ascii_lowercase();
    model.starts_with("claude-")
        && !["claude-2", "claude-instant"]
            .iter()
            .any(|old| model.starts_with(old))
}

#[cfg(test)]
mod tests;
