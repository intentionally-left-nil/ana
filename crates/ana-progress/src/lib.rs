//! A minimal, dependency-free single-line stderr progress renderer for
//! `ana`, shared by `ana-solver` and `ana-installer`.
//!
//! [`StatusLine`] renders one self-updating line to stderr (rewritten in
//! place with `\r` plus an ANSI erase-to-end-of-line) and erases it once
//! the phase it tracks finishes. Deliberately not built on `indicatif`:
//! `ana` wants transient, self-erasing status, not a persistent log.
//!
//! Disabled automatically when stderr isn't a terminal, or when
//! [`NO_PROGRESS_ENV_VAR`] is set to a non-empty value. Every method on a
//! disabled `StatusLine` is a cheap no-op.
#![deny(clippy::unwrap_used, clippy::expect_used)]

use std::borrow::Cow;
use std::ffi::OsStr;
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Force-disables progress rendering even when stderr is a terminal. Any
/// non-empty value disables it.
pub const NO_PROGRESS_ENV_VAR: &str = "ANA_NO_PROGRESS";

/// Fallback terminal width, in columns, used when the real width can't
/// be detected.
const FALLBACK_WIDTH: usize = 80;

/// Minimum time between two real redraws, so a high-frequency caller
/// (e.g. `on_download_progress`, fired once per network chunk) doesn't
/// translate into unbounded writes.
const MIN_REDRAW_INTERVAL: Duration = Duration::from_millis(33);

/// A single, self-updating status line on stderr. Cheap to construct;
/// safe to share across threads -- concurrent `update`/`clear` calls are
/// serialized (last write wins), not interleaved.
pub struct StatusLine {
    enabled: bool,
    /// Whether anything has actually been drawn yet, so `clear()` can
    /// no-op if nothing needs erasing.
    drawn: AtomicBool,
    /// When the last real redraw happened, for the throttle in `update`.
    last_drawn_at: Mutex<Option<Instant>>,
}

impl Default for StatusLine {
    fn default() -> Self {
        Self::new()
    }
}

impl StatusLine {
    pub fn new() -> Self {
        let enabled = std::io::stderr().is_terminal()
            && !no_progress_disabled(std::env::var_os(NO_PROGRESS_ENV_VAR));
        Self {
            enabled,
            drawn: AtomicBool::new(false),
            last_drawn_at: Mutex::new(None),
        }
    }

    /// True if this status line will actually render anything.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// True if calling [`update`](Self::update) right now would actually
    /// redraw anything. Lets a high-frequency caller skip the work of
    /// building the status text when the call would just be throttled
    /// away.
    pub fn should_render(&self) -> bool {
        self.enabled && {
            let last_drawn_at = self
                .last_drawn_at
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            should_redraw(*last_drawn_at, Instant::now())
        }
    }

    /// (Re)draws the current line as `text`. No-op if disabled, and
    /// throttled to [`MIN_REDRAW_INTERVAL`].
    pub fn update(&self, text: &str) {
        if !self.enabled {
            return;
        }
        {
            let mut last_drawn_at = self
                .last_drawn_at
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let now = Instant::now();
            if !should_redraw(*last_drawn_at, now) {
                return;
            }
            *last_drawn_at = Some(now);
        }
        let text = sanitize(text);
        let text = truncate(&text, terminal_width());
        let mut stderr = std::io::stderr().lock();
        let _ = write!(stderr, "\r\x1B[K{text}");
        let _ = stderr.flush();
        self.drawn.store(true, Ordering::Relaxed);
    }

    /// Erases the current line entirely. No-op if disabled or if nothing
    /// has been drawn yet.
    pub fn clear(&self) {
        if !self.enabled || !self.drawn.swap(false, Ordering::Relaxed) {
            return;
        }
        let mut stderr = std::io::stderr().lock();
        let _ = write!(stderr, "\r\x1B[K");
        let _ = stderr.flush();
    }
}

/// Strips control characters (ANSI escapes, `\r`, `\n`, ...) out of
/// `text` before it's rendered. Several callers render text that
/// ultimately comes from repodata, which is channel-supplied and so
/// attacker-controlled if a channel is malicious or a fetch is MITM'd.
fn sanitize(text: &str) -> Cow<'_, str> {
    if text.chars().any(|c| c.is_control()) {
        Cow::Owned(text.chars().filter(|c| !c.is_control()).collect())
    } else {
        Cow::Borrowed(text)
    }
}

/// The pure decision behind [`StatusLine::update`]'s throttle, factored
/// out for testing with synthetic `Instant`s. The first redraw (`last`
/// is `None`) always proceeds.
fn should_redraw(last: Option<Instant>, now: Instant) -> bool {
    match last {
        None => true,
        Some(previous) => now.duration_since(previous) >= MIN_REDRAW_INTERVAL,
    }
}

fn no_progress_disabled(value: Option<impl AsRef<OsStr>>) -> bool {
    value.is_some_and(|value| !value.as_ref().is_empty())
}

/// The real terminal width, in columns, for the terminal stderr is
/// attached to. Queried fresh on every call so a mid-run resize is
/// honored.
fn terminal_width() -> usize {
    let detected = terminal_size::terminal_size_of(std::io::stderr())
        .map(|(terminal_size::Width(width), _height)| width as usize);
    clamp_width(detected)
}

/// Falls back to [`FALLBACK_WIDTH`] when `detected` is `None`, then
/// reserves one column of margin against terminals that auto-wrap a line
/// that exactly fills the last column.
fn clamp_width(detected: Option<usize>) -> usize {
    detected.unwrap_or(FALLBACK_WIDTH).saturating_sub(1).max(1)
}

/// Truncates `text` to `max_width` characters, appending an ellipsis
/// when truncated.
fn truncate(text: &str, max_width: usize) -> Cow<'_, str> {
    if text.chars().count() <= max_width {
        Cow::Borrowed(text)
    } else if max_width == 0 {
        Cow::Borrowed("")
    } else {
        let truncated: String = text.chars().take(max_width - 1).collect();
        Cow::Owned(format!("{truncated}…"))
    }
}

/// Renders a fixed-width Unicode block progress bar: `fraction` (clamped
/// to `[0.0, 1.0]`) of `width` characters filled with `'█'`, the
/// remainder `'░'`.
pub fn bar(fraction: f64, width: usize) -> String {
    let fraction = if fraction.is_finite() {
        fraction.clamp(0.0, 1.0)
    } else {
        0.0
    };
    #[allow(clippy::cast_precision_loss, clippy::cast_sign_loss)]
    let filled = ((fraction * width as f64).round() as usize).min(width);
    let mut out = String::with_capacity(width * '█'.len_utf8());
    for _ in 0..filled {
        out.push('█');
    }
    for _ in filled..width {
        out.push('░');
    }
    out
}

/// Renders `fraction` (clamped to `[0.0, 1.0]`) as a whole-number
/// percentage, for the `NN%` suffix next to a [`bar`].
pub fn percent(fraction: f64) -> u32 {
    let fraction = if fraction.is_finite() {
        fraction.clamp(0.0, 1.0)
    } else {
        0.0
    };
    #[allow(clippy::cast_precision_loss, clippy::cast_sign_loss)]
    {
        (fraction * 100.0).round() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_progress_disabled_is_true_only_for_non_empty_values() {
        assert!(!no_progress_disabled(None::<&OsStr>));
        assert!(!no_progress_disabled(Some(OsStr::new(""))));
        assert!(no_progress_disabled(Some(OsStr::new("1"))));
        assert!(no_progress_disabled(Some(OsStr::new("0"))));
        assert!(no_progress_disabled(Some(OsStr::new("false"))));
    }

    #[test]
    fn should_redraw_always_allows_the_first_draw() {
        assert!(should_redraw(None, Instant::now()));
    }

    #[test]
    fn should_redraw_throttles_a_redraw_that_is_too_soon() {
        let first = Instant::now();
        let too_soon = first + Duration::from_millis(1);
        assert!(!should_redraw(Some(first), too_soon));
    }

    #[test]
    fn should_redraw_allows_a_redraw_once_the_interval_has_elapsed() {
        let first = Instant::now();
        let later = first + MIN_REDRAW_INTERVAL;
        assert!(should_redraw(Some(first), later));
    }

    #[test]
    fn bar_renders_proportional_fill() {
        assert_eq!(bar(0.0, 10), "░░░░░░░░░░");
        assert_eq!(bar(1.0, 10), "██████████");
        assert_eq!(bar(0.5, 10), "█████░░░░░");
        assert_eq!(bar(0.46, 10), "█████░░░░░");
    }

    #[test]
    fn bar_clamps_out_of_range_and_non_finite_fractions() {
        assert_eq!(bar(-1.0, 4), "░░░░");
        assert_eq!(bar(2.0, 4), "████");
        assert_eq!(bar(f64::NAN, 4), "░░░░");
        assert_eq!(bar(f64::INFINITY, 4), "████".replace('█', "░"));
    }

    #[test]
    fn percent_rounds_and_clamps() {
        assert_eq!(percent(0.0), 0);
        assert_eq!(percent(1.0), 100);
        assert_eq!(percent(0.436), 44);
        assert_eq!(percent(-1.0), 0);
        assert_eq!(percent(2.0), 100);
        assert_eq!(percent(f64::NAN), 0);
    }

    #[test]
    fn truncate_leaves_short_text_untouched() {
        assert_eq!(truncate("short", 80), Cow::Borrowed("short"));
    }

    #[test]
    fn truncate_shortens_long_text_with_an_ellipsis() {
        let long = "x".repeat(90);
        let truncated = truncate(&long, 80);
        assert_eq!(truncated.chars().count(), 80);
        assert!(truncated.ends_with('…'));
    }

    #[test]
    fn truncate_at_exactly_the_width_is_untouched() {
        let exact = "x".repeat(80);
        assert_eq!(truncate(&exact, 80), Cow::Borrowed(exact.as_str()));
    }

    #[test]
    fn truncate_handles_a_zero_width_without_underflowing() {
        assert_eq!(truncate("anything", 0), Cow::Borrowed(""));
    }

    #[test]
    fn clamp_width_falls_back_when_detection_fails() {
        assert_eq!(clamp_width(None), FALLBACK_WIDTH - 1);
    }

    #[test]
    fn clamp_width_reserves_one_column_of_margin() {
        assert_eq!(clamp_width(Some(100)), 99);
    }

    #[test]
    fn clamp_width_never_reaches_zero_even_for_a_one_column_terminal() {
        assert_eq!(clamp_width(Some(1)), 1);
        assert_eq!(clamp_width(Some(0)), 1);
    }

    /// A `StatusLine` that is disabled unconditionally, unlike
    /// `StatusLine::new()` whose `enabled` depends on whether the test
    /// binary's stderr happens to be a terminal.
    fn disabled_status_line() -> StatusLine {
        StatusLine {
            enabled: false,
            drawn: AtomicBool::new(false),
            last_drawn_at: Mutex::new(None),
        }
    }

    #[test]
    fn disabled_status_line_never_marks_itself_drawn() {
        let line = disabled_status_line();
        assert!(!line.enabled());
        line.update("this should never render");
        assert!(!line.drawn.load(Ordering::Relaxed));
        line.clear();
    }

    #[test]
    fn disabled_status_line_never_should_render() {
        let line = disabled_status_line();
        assert!(!line.enabled());
        assert!(!line.should_render());
    }

    #[test]
    fn sanitize_leaves_plain_text_untouched() {
        assert_eq!(
            sanitize("ana: installing packages"),
            Cow::Borrowed("ana: installing packages")
        );
    }

    #[test]
    fn sanitize_strips_ansi_escape_sequences() {
        let injected = "evil\x1B[31mred\x1B[0m";
        assert_eq!(sanitize(injected), "evil[31mred[0m");
        assert!(!sanitize(injected).contains('\x1B'));
    }

    #[test]
    fn sanitize_strips_carriage_returns_and_newlines() {
        assert_eq!(sanitize("line one\r\nline two"), "line oneline two");
    }
}
