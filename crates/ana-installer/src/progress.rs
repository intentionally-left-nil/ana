//! Progress reporting for [`crate::reconcile`]'s real install --
//! `rattler::install::Reporter`, driving one [`ana_progress::StatusLine`]
//! through two sub-phases:
//!
//! - **caching**: populating the package cache for every package that
//!   needs installing. Count-based (`packages cached /
//!   packages_to_install()`), with cumulative downloaded/total bytes
//!   shown as supplementary text.
//! - **linking**: linking (and unlinking) packages into the prefix.
//!   Count-based too (`operations done / packages_to_install() +
//!   packages_to_uninstall()`).
//!
//! These sub-phases overlap in practice (rattler pipelines each
//! operation's cache-then-link sequence concurrently with every other
//! operation's), so [`InstallProgress::redraw`] shows caching only until
//! the *first* link/unlink completes, then switches to linking for the
//! rest of the transaction.
//!
//! [`InstallProgress`]'s `Drop` impl erases the line no matter how
//! install finishes -- success, error, or panic during unwind.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use ana_progress::StatusLine;
use rattler::install::{Reporter, Transaction, TransactionOperation};
use rattler_conda_types::{PrefixRecord, RepoDataRecord};

/// The width, in characters, of every bar this module renders.
const BAR_WIDTH: usize = 20;

/// Byte-level bookkeeping [`InstallProgress`] needs across concurrent
/// downloads.
#[derive(Default)]
struct Bytes {
    /// Each operation's package size in bytes, indexed the same way
    /// every `on_*` callback below is keyed. `0` for `Remove`
    /// operations and records with no known `size`.
    package_sizes: Vec<u64>,
    /// Bytes downloaded so far, same indexing as `package_sizes`.
    downloaded: Vec<u64>,
    /// Running total of `downloaded`, maintained incrementally rather
    /// than re-summed on every redraw.
    total_downloaded: u64,
    /// Running total of `package_sizes[idx]` for every `idx` that
    /// actually started downloading -- the denominator for the
    /// "x.y/z.w MiB" text.
    total_download_size: u64,
}

/// A [`Reporter`] for one [`crate::reconcile`] call's install -- see the
/// module docs.
pub(crate) struct InstallProgress {
    line: StatusLine,
    total_to_cache: AtomicUsize,
    cached: AtomicUsize,
    total_ops: AtomicUsize,
    ops_done: AtomicUsize,
    bytes: Mutex<Bytes>,
}

impl InstallProgress {
    pub(crate) fn new() -> Self {
        Self {
            line: StatusLine::new(),
            total_to_cache: AtomicUsize::new(0),
            cached: AtomicUsize::new(0),
            total_ops: AtomicUsize::new(0),
            ops_done: AtomicUsize::new(0),
            bytes: Mutex::new(Bytes::default()),
        }
    }

    fn redraw(&self) {
        if !self.line.should_render() {
            return;
        }
        // Once any link/unlink has completed, keep showing linking
        // progress -- caching and linking run concurrently, so `ops_done`
        // only ever increases and is the more accurate signal.
        if self.ops_done.load(Ordering::Relaxed) == 0 {
            let total_to_cache = self.total_to_cache.load(Ordering::Relaxed);
            let cached = self.cached.load(Ordering::Relaxed);
            self.redraw_caching(total_to_cache, cached);
        } else {
            self.redraw_linking();
        }
    }

    fn redraw_caching(&self, total_to_cache: usize, cached: usize) {
        let (downloaded, total_download_size) = {
            let bytes = self.bytes.lock().unwrap_or_else(PoisonError::into_inner);
            (bytes.total_downloaded, bytes.total_download_size)
        };
        let fraction = if total_to_cache == 0 {
            1.0
        } else {
            cached as f64 / total_to_cache as f64
        };
        self.line.update(&format!(
            "ana: installing packages \u{2014} caching [{bar}] {percent}%  \
             ({cached}/{total_to_cache} \u{b7} {down:.1}/{total:.1} MiB)",
            bar = ana_progress::bar(fraction, BAR_WIDTH),
            percent = ana_progress::percent(fraction),
            down = downloaded as f64 / (1024.0 * 1024.0),
            total = total_download_size as f64 / (1024.0 * 1024.0),
        ));
    }

    fn redraw_linking(&self) {
        let total_ops = self.total_ops.load(Ordering::Relaxed);
        let done = self.ops_done.load(Ordering::Relaxed);
        let fraction = if total_ops == 0 {
            1.0
        } else {
            done as f64 / total_ops as f64
        };
        self.line.update(&format!(
            "ana: installing packages \u{2014} linking [{bar}] {percent}%  ({done}/{total_ops})",
            bar = ana_progress::bar(fraction, BAR_WIDTH),
            percent = ana_progress::percent(fraction),
        ));
    }
}

impl Drop for InstallProgress {
    fn drop(&mut self) {
        self.line.clear();
    }
}

impl Reporter for InstallProgress {
    fn on_transaction_start(&self, transaction: &Transaction<PrefixRecord, RepoDataRecord>) {
        let total_to_cache = transaction.packages_to_install();
        let total_ops = total_to_cache + transaction.packages_to_uninstall();
        self.total_to_cache.store(total_to_cache, Ordering::Relaxed);
        self.total_ops.store(total_ops, Ordering::Relaxed);

        let package_sizes: Vec<u64> = transaction
            .operations
            .iter()
            .map(|operation| {
                let record = match operation {
                    TransactionOperation::Install(new)
                    | TransactionOperation::Change { new, .. }
                    | TransactionOperation::Reinstall { new, .. } => Some(&new.package_record),
                    TransactionOperation::Remove(_) => None,
                };
                record.and_then(|record| record.size).unwrap_or(0)
            })
            .collect();
        let mut bytes = self.bytes.lock().unwrap_or_else(PoisonError::into_inner);
        bytes.downloaded = vec![0; package_sizes.len()];
        bytes.package_sizes = package_sizes;
        drop(bytes);

        self.redraw();
    }

    fn on_transaction_operation_start(&self, _operation: usize) {}

    fn on_populate_cache_start(&self, operation: usize, _record: &RepoDataRecord) -> usize {
        operation
    }

    fn on_validate_start(&self, cache_entry: usize) -> usize {
        cache_entry
    }

    fn on_validate_complete(&self, _validate_idx: usize) {}

    fn on_download_start(&self, cache_entry: usize) -> usize {
        {
            let mut bytes = self.bytes.lock().unwrap_or_else(PoisonError::into_inner);
            let size = bytes.package_sizes.get(cache_entry).copied().unwrap_or(0);
            bytes.total_download_size += size;
        }
        self.redraw();
        cache_entry
    }

    fn on_download_progress(&self, download_idx: usize, progress: u64, _total: Option<u64>) {
        {
            let mut bytes = self.bytes.lock().unwrap_or_else(PoisonError::into_inner);
            if bytes.downloaded.len() <= download_idx {
                bytes.downloaded.resize(download_idx + 1, 0);
            }
            let previous = bytes.downloaded[download_idx];
            bytes.downloaded[download_idx] = progress;
            // `progress` is cumulative, not a delta.
            bytes.total_downloaded = bytes
                .total_downloaded
                .saturating_sub(previous)
                .saturating_add(progress);
        }
        self.redraw();
    }

    fn on_download_completed(&self, _download_idx: usize) {
        self.redraw();
    }

    fn on_populate_cache_complete(&self, _cache_entry: usize) {
        self.cached.fetch_add(1, Ordering::Relaxed);
        self.redraw();
    }

    fn on_unlink_start(&self, operation: usize, _record: &PrefixRecord) -> usize {
        operation
    }

    fn on_unlink_complete(&self, _index: usize) {
        self.ops_done.fetch_add(1, Ordering::Relaxed);
        self.redraw();
    }

    fn on_link_start(&self, operation: usize, _record: &RepoDataRecord) -> usize {
        operation
    }

    fn on_link_complete(&self, _index: usize) {
        self.ops_done.fetch_add(1, Ordering::Relaxed);
        self.redraw();
    }

    fn on_transaction_operation_complete(&self, _operation: usize) {}

    fn on_transaction_complete(&self) {
        self.line.clear();
    }

    fn on_post_link_start(&self, package_name: &str, script_path: &str) -> usize {
        self.line.update(&format!(
            "ana: installing packages \u{2014} running post-link script for {package_name} ({script_path})"
        ));
        0
    }

    fn on_post_link_complete(&self, _index: usize, _success: bool) {
        self.redraw();
    }

    fn on_pre_unlink_start(&self, package_name: &str, script_path: &str) -> usize {
        self.line.update(&format!(
            "ana: installing packages \u{2014} running pre-unlink script for {package_name} ({script_path})"
        ));
        0
    }

    fn on_pre_unlink_complete(&self, _index: usize, _success: bool) {
        self.redraw();
    }
}
