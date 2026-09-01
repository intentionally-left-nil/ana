//! Read-only per-channel package queries -- the `ana search` seam.
//!
//! The search-side counterpart to [`ana_lockfile::Solver`]: the trait
//! lives here so `ana`'s search flow is testable with fakes, and
//! [`RattlerSolver`] is the real, network-backed impl. Unlike a solve, a
//! query is non-recursive and never fails as a whole -- each channel
//! gets its own [`ChannelQueryOutcome`], so a caller can report "no
//! matches on this channel" apart from "couldn't reach this channel".

use std::sync::Arc;

use rattler_conda_types::{Channel, ChannelUrl, MatchSpec, Platform, RepoDataRecord};
use rattler_repodata_gateway::{ChannelRelationsMode, Gateway, GatewayError, RepoData};

use crate::progress::SharedFetchProgress;
use crate::RattlerSolver;

/// One channel's outcome from a [`ChannelQuery::query_channels`] call.
#[derive(Debug)]
pub struct ChannelQueryOutcome {
    /// The queried channel's canonical base URL.
    pub channel: ChannelUrl,
    /// The matching records (possibly empty), or why this channel's
    /// query failed. Records are shared with the gateway's cache by
    /// refcount rather than deep-copied out of it.
    pub result: Result<Vec<Arc<RepoDataRecord>>, ChannelQueryError>,
}

/// Why one channel's query produced no records.
#[derive(Debug, thiserror::Error)]
pub enum ChannelQueryError {
    /// The channel serves none of the queried subdirs. A missing
    /// non-`noarch` subdir reads as empty rather than an error (the
    /// gateway treats a channel publishing only some platforms as
    /// valid), so in practice this means the channel's `noarch`
    /// repodata 404'd -- the channel isn't really there. Carries the
    /// missing subdir's name.
    #[error("no {0:?} subdir")]
    SubdirNotFound(String),

    /// Any other failure: network, authentication, repodata parsing,
    /// cache.
    #[error("{0}")]
    Fetch(String),
}

/// A read-only, per-channel package query. Implementations do the
/// network-bound work; classification and rendering are the caller's.
pub trait ChannelQuery {
    /// Queries every channel in `channels` for records matching `spec`
    /// across `platforms` (the exact list -- include
    /// [`Platform::NoArch`] explicitly when its records are wanted, as
    /// they almost always are). One outcome per channel, in the same
    /// order; a failing channel fails its own outcome only.
    fn query_channels(
        &self,
        channels: &[Channel],
        platforms: &[Platform],
        spec: &MatchSpec,
    ) -> Vec<ChannelQueryOutcome>;
}

impl ChannelQuery for RattlerSolver {
    fn query_channels(
        &self,
        channels: &[Channel],
        platforms: &[Platform],
        spec: &MatchSpec,
    ) -> Vec<ChannelQueryOutcome> {
        self.runtime_handle
            .block_on(query_channels(&self.gateway, channels, platforms, spec))
    }
}

/// The async body of [`RattlerSolver::query_channels`] -- a free
/// function (not a method) so it borrows only `&Gateway`, like
/// [`crate::solve`].
async fn query_channels(
    gateway: &Gateway,
    channels: &[Channel],
    platforms: &[Platform],
    spec: &MatchSpec,
) -> Vec<ChannelQueryOutcome> {
    let progress = SharedFetchProgress::new(channels.len() * platforms.len());

    // One query per channel, run concurrently: a single combined query
    // would fail as a unit when any one channel is unreachable, losing
    // the per-channel attribution search exists to provide. The gateway
    // caches repodata across queries, so repeat searches don't re-fetch.
    let queries = channels.iter().map(|channel| {
        let query = gateway
            .query(
                vec![channel.clone()],
                platforms.iter().copied(),
                vec![spec.clone()],
            )
            .recursive(false)
            .channel_relations(ChannelRelationsMode::Disabled)
            .with_reporter(progress.clone());
        let url = channel.base_url.clone();
        async move {
            let result = query
                .await
                .map(|output| {
                    output
                        .iter()
                        .flat_map(RepoData::iter_arc)
                        .map(Arc::clone)
                        .collect()
                })
                .map_err(classify);
            ChannelQueryOutcome {
                channel: url,
                result,
            }
        }
    });
    futures::future::join_all(queries).await
}

fn classify(err: GatewayError) -> ChannelQueryError {
    match err {
        GatewayError::SubdirNotFoundError(err) => ChannelQueryError::SubdirNotFound(err.subdir),
        other => ChannelQueryError::Fetch(other.to_string()),
    }
}
