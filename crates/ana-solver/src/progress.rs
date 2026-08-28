//! Progress reporting for one [`super::solve`] call's two phases:
//! fetching channel/subdir repodata over the network ([`FetchProgress`],
//! a `rattler_repodata_gateway::{Reporter, DownloadReporter}` impl), and
//! the synchronous `resolvo` solve that follows it, which has no
//! progress hooks of its own -- see [`solve_label`].
//!
//! Both render through one [`ana_progress::StatusLine`] per call. The
//! fetch phase's line is only ever drawn if a fetch actually happens
//! over the network -- a warm, non-stale repodata cache never calls
//! [`FetchProgress`] at all. Its `Drop` impl erases the line as soon as
//! `Gateway::query(...)` resolves, on every path.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use ana_progress::StatusLine;
use rattler_repodata_gateway::{DownloadReporter, Reporter};
use url::Url;

/// The width, in characters, of every bar this module renders.
const BAR_WIDTH: usize = 20;

/// A [`Reporter`]/[`DownloadReporter`] for one [`super::solve`] call's
/// repodata fetch -- see the module docs.
pub(crate) struct FetchProgress {
    line: StatusLine,
    /// How many channel/subdir fetches this solve expects to need if
    /// every one requires a real network fetch
    /// (`channels.len() * platforms.len()`) -- the denominator for the
    /// fetch-progress fraction. Actual fetches are often fewer (cache
    /// hits never call this reporter).
    expected_fetches: usize,
    /// Assigns each [`DownloadReporter::on_download_start`] call its own
    /// index into `bytes_downloaded`.
    next_index: AtomicUsize,
    completed: AtomicUsize,
    /// Bytes downloaded so far, one slot per started download.
    bytes_downloaded: Mutex<Vec<usize>>,
    /// Running total of `bytes_downloaded`, maintained incrementally
    /// rather than re-summed on every redraw.
    total_bytes_downloaded: AtomicUsize,
}

impl FetchProgress {
    pub(crate) fn new(expected_fetches: usize) -> Self {
        Self {
            line: StatusLine::new(),
            expected_fetches,
            next_index: AtomicUsize::new(0),
            completed: AtomicUsize::new(0),
            bytes_downloaded: Mutex::new(Vec::new()),
            total_bytes_downloaded: AtomicUsize::new(0),
        }
    }

    fn redraw(&self) {
        if !self.line.should_render() {
            return;
        }
        let completed = self.completed.load(Ordering::Relaxed);
        let fraction = if self.expected_fetches == 0 {
            1.0
        } else {
            completed as f64 / self.expected_fetches as f64
        };
        let total_bytes = self.total_bytes_downloaded.load(Ordering::Relaxed);
        let mib = total_bytes as f64 / (1024.0 * 1024.0);
        self.line.update(&format!(
            "ana: fetching repodata [{bar}] {percent}%  ({completed}/{expected} \u{b7} {mib:.1} MiB)",
            bar = ana_progress::bar(fraction, BAR_WIDTH),
            percent = ana_progress::percent(fraction),
            expected = self.expected_fetches,
        ));
    }
}

impl Reporter for FetchProgress {
    fn download_reporter(&self) -> Option<&dyn DownloadReporter> {
        Some(self)
    }
}

impl Drop for FetchProgress {
    fn drop(&mut self) {
        self.line.clear();
    }
}

impl DownloadReporter for FetchProgress {
    fn on_download_start(&self, _url: &Url) -> usize {
        let index = self.next_index.fetch_add(1, Ordering::Relaxed);
        {
            let mut bytes = self
                .bytes_downloaded
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if bytes.len() <= index {
                bytes.resize(index + 1, 0);
            }
        }
        self.redraw();
        index
    }

    fn on_download_progress(
        &self,
        _url: &Url,
        index: usize,
        bytes_downloaded: usize,
        _total_bytes: Option<usize>,
    ) {
        {
            let mut bytes = self
                .bytes_downloaded
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if bytes.len() <= index {
                bytes.resize(index + 1, 0);
            }
            let previous = bytes[index];
            bytes[index] = bytes_downloaded;
            // `bytes_downloaded` is cumulative, not a delta.
            self.total_bytes_downloaded
                .fetch_add(bytes_downloaded.wrapping_sub(previous), Ordering::Relaxed);
        }
        self.redraw();
    }

    fn on_download_complete(&self, _url: &Url, _index: usize) {
        self.completed.fetch_add(1, Ordering::Relaxed);
        self.redraw();
    }
}

/// Shows a "solving environment..." status line on `line` for the
/// duration of `solve` (the caller's synchronous `backend.solve(task)`
/// call), clearing it again on every path -- including a panic inside
/// `solve` -- via a small `Drop` guard.
///
/// Unlike [`FetchProgress`], there's no percentage: `resolvo`'s
/// `SolverImpl::solve` is a single synchronous call with no progress
/// hooks. The label is shown unconditionally, even for the common,
/// millisecond-scale case where it just flashes briefly -- the same
/// tradeoff `cargo`/`uv`/`pixi` make for their own "resolving..."
/// messages.
pub(crate) fn solve_label<F, T>(line: &StatusLine, solve: F) -> T
where
    F: FnOnce() -> T,
{
    struct ClearOnDrop<'a>(&'a StatusLine);

    impl Drop for ClearOnDrop<'_> {
        fn drop(&mut self) {
            self.0.clear();
        }
    }

    line.update("ana: solving environment...");
    let _clear_on_drop = ClearOnDrop(line);
    solve()
}
