//! Compact Block Filter Manager.
//!
//! Manages BIP 157/8 compact block filter sync.
//!
use std::collections::BTreeSet;
use std::ops::{Bound, Range, RangeInclusive};

use thiserror::Error;

use bitcoin::network::constants::ServiceFlags;
use bitcoin::network::message_filter::{CFHeaders, CFilter, GetCFHeaders};
use bitcoin::util::bip158;
use bitcoin::{Script, Transaction, Txid};

use nakamoto_common::block::filter::{self, BlockFilter, Filters};
use nakamoto_common::block::time::{Clock, LocalDuration, LocalTime};
use nakamoto_common::block::tree::BlockTree;
use nakamoto_common::block::{BlockHash, Height};
use nakamoto_common::collections::{AddressBook, HashMap, HashSet};
use nakamoto_common::source;

use super::channel::SetTimeout;
use super::{Link, PeerId, Timeout};

/// Idle timeout.
pub const IDLE_TIMEOUT: LocalDuration = LocalDuration::BLOCK_INTERVAL;

/// Services required from peers for BIP 158 functionality.
pub const REQUIRED_SERVICES: ServiceFlags = ServiceFlags::COMPACT_FILTERS;

/// Maximum filter headers to be expected in a message.
pub const MAX_MESSAGE_CFHEADERS: usize = 2000;

/// Maximum filters to be expected in a message.
pub const MAX_MESSAGE_CFILTERS: usize = 1000;

/// An error originating in the CBF manager.
#[derive(Error, Debug)]
pub enum Error {
    /// The request was ignored. This happens if we're not able to fulfill the request.
    #[error("ignoring `{msg}` message from {from}")]
    Ignored {
        /// Message that was ignored.
        msg: &'static str,
        /// Message sender.
        from: PeerId,
    },
    /// Error due to an invalid peer message.
    #[error("invalid message received from {from}: {reason}")]
    InvalidMessage {
        /// Message sender.
        from: PeerId,
        /// Reason why the message is invalid.
        reason: &'static str,
    },
    /// Error with the underlying filters datastore.
    #[error("filters error: {0}")]
    Filters(#[from] filter::Error),
}

/// An event originating in the CBF manager.
#[derive(Debug, Clone)]
pub enum Event {
    /// Filter was received and validated.
    FilterReceived {
        /// Peer we received from.
        from: PeerId,
        /// The received filter.
        filter: BlockFilter,
        /// Filter height.
        height: Height,
        /// Hash of corresponding block.
        block_hash: BlockHash,
    },
    /// Filter was processed.
    FilterProcessed {
        /// The corresponding block hash.
        block: BlockHash,
        /// The filter height.
        height: Height,
        /// Whether or not this filter matched something in the watchlist.
        matched: bool,
    },
    /// Filter headers were imported successfully.
    FilterHeadersImported {
        /// New filter header chain height.
        height: Height,
        /// Block hash corresponding to the tip of the filter header chain.
        block_hash: BlockHash,
    },
    /// Started syncing filter headers with a peer.
    Syncing {
        /// The remote peer.
        peer: PeerId,
        /// The start height from which we're syncing.
        start_height: Height,
        /// The stop hash.
        stop_hash: BlockHash,
    },
    /// Request canceled.
    RequestCanceled {
        /// Reason for cancellation.
        reason: &'static str,
    },
    /// An active rescan has completed.
    RescanCompleted {
        /// Last height processed by rescan.
        height: Height,
    },
    /// Finished syncing filter headers up to the specified height.
    Synced(Height),
    /// A peer has timed out responding to a filter request.
    TimedOut(PeerId),
    /// Block header chain rollback detected.
    RollbackDetected(Height),
}

impl std::fmt::Display for Event {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Event::TimedOut(addr) => write!(fmt, "Peer {} timed out", addr),
            Event::FilterReceived {
                from,
                height,
                block_hash,
                ..
            } => {
                write!(
                    fmt,
                    "Filter {} received for block {} from {}",
                    height, block_hash, from
                )
            }
            Event::FilterProcessed {
                height, matched, ..
            } => {
                write!(
                    fmt,
                    "Filter processed at height {} (match = {})",
                    height, matched
                )
            }
            Event::FilterHeadersImported { height, .. } => {
                write!(fmt, "Imported filter header(s) up to height = {}", height,)
            }
            Event::Synced(height) => {
                write!(fmt, "Filter headers synced up to height = {}", height)
            }
            Event::Syncing {
                peer,
                start_height,
                stop_hash,
            } => write!(
                fmt,
                "Syncing filter headers with {}, start = {}, stop = {}",
                peer, start_height, stop_hash
            ),
            Event::RescanCompleted { height } => {
                write!(fmt, "Rescan completed at height {}", height)
            }
            Event::RequestCanceled { reason } => {
                write!(fmt, "Request canceled: {}", reason)
            }
            Event::RollbackDetected(height) => {
                write!(
                    fmt,
                    "Rollback detected: discarding filters from height {}..",
                    height
                )
            }
        }
    }
}

/// Compact filter synchronization.
pub trait SyncFilters {
    /// Get compact filter headers from peer, starting at the start height, and ending at the
    /// stop hash.
    fn get_cfheaders(
        &self,
        addr: PeerId,
        start_height: Height,
        stop_hash: BlockHash,
        timeout: Timeout,
    );
    /// Get compact filters from a peer.
    fn get_cfilters(
        &self,
        addr: PeerId,
        start_height: Height,
        stop_hash: BlockHash,
        timeout: Timeout,
    );
    /// Send compact filter headers to a peer.
    fn send_cfheaders(&self, addr: PeerId, headers: CFHeaders);
    /// Send a compact filter to a peer.
    fn send_cfilter(&self, addr: PeerId, filter: CFilter);
}

/// The ability to emit CBF related events.
pub trait Events {
    /// Emit an CBF-related event.
    fn event(&self, event: Event);
}

/// An error from attempting to get compact filters.
#[derive(Error, Debug)]
pub enum GetFiltersError {
    /// The specified range is invalid, eg. it is out of bounds.
    #[error("the specified range is invalid")]
    InvalidRange,
    /// Not connected to any compact filter peer.
    #[error("not connected to any peer with compact filters support")]
    NotConnected,
}

/// CBF manager configuration.
#[derive(Debug)]
pub struct Config {
    /// How long to wait for a response from a peer.
    pub request_timeout: Timeout,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            request_timeout: Timeout::from_secs(30),
        }
    }
}

/// A CBF peer.
#[derive(Debug)]
struct Peer {
    height: Height,
    last_active: LocalTime,
}

/// Filter (re)scan state.
#[derive(Debug, Default)]
pub struct Rescan {
    /// Whether a rescan is currently in progress.
    active: bool,
    /// Current height up to which we've synced filters.
    /// Must be between `start` and `end`.
    current: Height,
    /// Start height of the filter rescan. If `None`, starts at the current filter
    /// header height.
    start: Option<Height>,
    /// End height of the filter rescan. If `None`, keeps scanning new blocks until stopped.
    end: Option<Height>,
    /// Addresses and outpoints to watch for.
    watch: HashSet<Script>,
    /// Transactions to watch for.
    transactions: HashMap<Txid, HashSet<Script>>,
    /// Filters requested and remaining to download.
    requested: BTreeSet<Height>,
    /// Received filters waiting to be matched.
    received: HashMap<Height, (BlockFilter, BlockHash)>,
}

/// A compact block filter manager.
#[derive(Debug)]
pub struct FilterManager<F, U> {
    config: Config,
    peers: AddressBook<PeerId, Peer>,
    rescan: Rescan,
    filters: F,
    upstream: U,
    /// Last time we idled.
    last_idle: Option<LocalTime>,
    /// Inflight requests.
    inflight: HashMap<BlockHash, LocalTime>,
    rng: fastrand::Rng,
}

impl<F: Filters, U: SyncFilters + Events + SetTimeout> FilterManager<F, U> {
    /// Create a new filter manager.
    pub fn new(config: Config, rng: fastrand::Rng, filters: F, upstream: U) -> Self {
        let peers = AddressBook::new(rng.clone());
        let rescan = Rescan::default();

        Self {
            config,
            peers,
            rescan,
            upstream,
            filters,
            inflight: HashMap::with_hasher(rng.clone().into()),
            last_idle: None,
            rng,
        }
    }

    /// Initialize the manager. Should only be called once.
    pub fn initialize(&mut self, _now: LocalTime) {
        self.upstream.set_timeout(IDLE_TIMEOUT);
    }

    /// Called periodically. Triggers syncing if necessary.
    pub fn idle<T: BlockTree>(&mut self, now: LocalTime, tree: &T) {
        if now - self.last_idle.unwrap_or_default() >= IDLE_TIMEOUT {
            self.sync(tree, now);
            self.last_idle = Some(now);
            self.upstream.set_timeout(IDLE_TIMEOUT);
            self.inflight.clear();
        }
    }

    /// A tick was received.
    pub fn received_tick<T: BlockTree>(&mut self, now: LocalTime, tree: &T) {
        self.idle(now, tree);
    }

    /// Rollback filter header chain by a given number of headers.
    pub fn rollback(&mut self, n: usize) -> Result<(), filter::Error> {
        self.filters.rollback(n)
    }

    /// Add a script to the list of scripts to watch.
    #[allow(dead_code)]
    pub fn watch(&mut self, script: Script) -> bool {
        self.rescan.watch.insert(script)
    }

    /// Add transaction outputs to list of transactions to watch.
    pub fn watch_transactions(&mut self, txs: &[Transaction]) {
        for tx in txs {
            self.rescan.transactions.insert(
                tx.txid(),
                tx.output.iter().map(|o| o.script_pubkey.clone()).collect(),
            );
        }
    }

    /// Remove transaction from list of transactions being watch.
    pub fn unwatch_transaction(&mut self, txid: &Txid) -> bool {
        self.rescan.transactions.remove(txid).is_some()
    }

    /// Rescan compact block filters.
    pub fn rescan<T: BlockTree>(
        &mut self,
        start: Bound<Height>,
        end: Bound<Height>,
        watch: Vec<Script>,
        tree: &T,
    ) -> Result<(), GetFiltersError> {
        if self.rescan.active {
            // TODO: Don't panic here.
            panic!("{}: Rescan already active", source!());
        }
        self.rescan.active = true;
        self.rescan.received = HashMap::with_hasher(self.rng.clone().into());
        self.rescan.start = match start {
            Bound::Unbounded => None,
            Bound::Included(h) => Some(h),
            Bound::Excluded(h) => Some(h + 1),
        };
        self.rescan.end = match end {
            Bound::Unbounded => None,
            Bound::Included(h) => Some(h),
            Bound::Excluded(h) => Some(h - 1),
        };
        self.rescan.current = self.rescan.start.unwrap_or_else(|| tree.height() + 1);
        self.rescan.watch = watch.into_iter().collect();
        self.rescan.transactions = HashMap::with_hasher(self.rng.clone().into());
        self.rescan.requested = BTreeSet::new();

        // Nb. If our filter header chain isn't caught up with our block header chain,
        // this range will be empty, and this will effectively do nothing.
        self.get_cfilters(self.rescan.current..=self.filters.height(), tree)
    }

    /// Send a `getcfilters` message to a random peer.
    ///
    /// If the range is greater than [`MAX_MESSAGE_CFILTERS`], requests filters from multiple
    /// peers.
    pub fn get_cfilters<T: BlockTree>(
        &mut self,
        range: RangeInclusive<Height>,
        tree: &T,
    ) -> Result<(), GetFiltersError> {
        if range.is_empty() {
            return Ok(());
        }
        if self.peers.is_empty() {
            return Err(GetFiltersError::NotConnected);
        }

        let iter = HeightIterator {
            start: *range.start(),
            stop: *range.end() + 1,
            step: MAX_MESSAGE_CFILTERS as Height,
        };

        // TODO: Only ask peers synced to a certain height.
        for (r, peer) in iter.zip(self.peers.cycle()) {
            let stop_hash = tree
                .get_block_by_height(r.end - 1)
                .ok_or(GetFiltersError::InvalidRange)?
                .block_hash();
            let timeout = self.config.request_timeout;

            self.upstream
                .get_cfilters(*peer, r.start, stop_hash, timeout);
        }

        if self.rescan.active {
            self.rescan.requested.extend(range);
        }

        Ok(())
    }

    /// Handle a `cfheaders` message from a peer.
    ///
    /// Returns the new filter header height, or an error.
    pub fn received_cfheaders<T: BlockTree>(
        &mut self,
        from: &PeerId,
        msg: CFHeaders,
        tree: &T,
        time: LocalTime,
    ) -> Result<Height, Error> {
        let from = *from;
        let stop_hash = msg.stop_hash;

        if self.inflight.remove(&stop_hash).is_none() {
            return Err(Error::Ignored {
                from,
                msg: "cfheaders: unsolicited message",
            });
        }

        if msg.filter_type != 0x0 {
            return Err(Error::InvalidMessage {
                from,
                reason: "cfheaders: invalid filter type",
            });
        }

        let prev_header = msg.previous_filter_header;
        let (_, tip) = self.filters.tip();

        // If the previous header doesn't match our tip, this could be a stale
        // message arriving too late. Ignore it.
        if tip != &prev_header {
            return Ok(self.filters.height());
        }

        let start_height = self.filters.height();
        let stop_height = if let Some((height, _)) = tree.get_block(&stop_hash) {
            height
        } else {
            return Err(Error::InvalidMessage {
                from,
                reason: "cfheaders: unknown stop hash",
            });
        };

        let hashes = msg.filter_hashes;
        let count = hashes.len();

        if start_height > stop_height {
            return Err(Error::InvalidMessage {
                from,
                reason: "cfheaders: start height is greater than stop height",
            });
        }

        if count > MAX_MESSAGE_CFHEADERS {
            return Err(Error::InvalidMessage {
                from,
                reason: "cfheaders: header count exceeds maximum",
            });
        }

        if count == 0 {
            return Err(Error::InvalidMessage {
                from,
                reason: "cfheaders: empty header list",
            });
        }

        if (stop_height - start_height) as usize != count {
            return Err(Error::InvalidMessage {
                from,
                reason: "cfheaders: header count does not match height range",
            });
        }

        // Ok, looks like everything's valid..

        let mut last_header = prev_header;
        let mut headers = Vec::with_capacity(count);

        // Create headers out of the hashes.
        for filter_hash in hashes {
            last_header = filter_hash.filter_header(&last_header);
            headers.push((filter_hash, last_header));
        }
        self.filters
            .import_headers(headers)
            .map(|height| {
                self.upstream.event(Event::FilterHeadersImported {
                    height,
                    block_hash: stop_hash,
                });
                self.headers_imported(start_height, height, tree).unwrap(); // TODO

                assert!(height <= tree.height());

                if height == tree.height() {
                    self.upstream.event(Event::Synced(height));
                } else {
                    self.sync(tree, time);
                }
                height
            })
            .map_err(Error::from)
    }

    /// Handle a `getcfheaders` message from a peer.
    pub fn received_getcfheaders<T: BlockTree>(
        &mut self,
        from: &PeerId,
        msg: GetCFHeaders,
        tree: &T,
    ) -> Result<(), Error> {
        let from = *from;

        if msg.filter_type != 0x0 {
            return Err(Error::InvalidMessage {
                from,
                reason: "getcfheaders: invalid filter type",
            });
        }

        let start_height = msg.start_height as Height;
        let stop_height = if let Some((height, _)) = tree.get_block(&msg.stop_hash) {
            height
        } else {
            // Can't handle this message, we don't have the stop block.
            return Err(Error::Ignored {
                msg: "getcfheaders",
                from,
            });
        };

        let headers = self.filters.get_headers(start_height..stop_height);
        if !headers.is_empty() {
            let hashes = headers.iter().map(|(hash, _)| *hash);
            let prev_header = self.filters.get_prev_header(start_height).expect(
                "FilterManager::received_getcfheaders: all headers up to the tip must exist",
            );

            self.upstream.send_cfheaders(
                from,
                CFHeaders {
                    filter_type: msg.filter_type,
                    stop_hash: msg.stop_hash,
                    previous_filter_header: prev_header,
                    filter_hashes: hashes.collect(),
                },
            );
            return Ok(());
        }
        // We must be syncing, since we have the block headers requested but
        // not the associated filter headers. Simply ignore the request.
        Err(Error::Ignored {
            msg: "getcfheaders",
            from,
        })
    }

    /// Handle a `cfilter` message.
    ///
    /// Returns a list of blocks that need to be fetched from the network.
    pub fn received_cfilter<T: BlockTree>(
        &mut self,
        from: &PeerId,
        msg: CFilter,
        tree: &T,
    ) -> Result<Vec<BlockHash>, Error> {
        let from = *from;

        if msg.filter_type != 0x0 {
            return Err(Error::Ignored {
                msg: "cfilter",
                from,
            });
        }

        let height = if let Some((height, _)) = tree.get_block(&msg.block_hash) {
            height
        } else {
            // Can't handle this message, we don't have the block.
            return Err(Error::Ignored {
                msg: "cfilter",
                from,
            });
        };

        // The expected hash for this block filter.
        let header = if let Some((_, header)) = self.filters.get_header(height) {
            header
        } else {
            // Can't handle this message, we don't have the header.
            return Err(Error::Ignored {
                msg: "cfilter",
                from,
            });
        };

        // Note that in case this fails, we have a bug in our implementation, since filter
        // headers are supposed to be downloaded in-order.
        let prev_header = self
            .filters
            .get_prev_header(height)
            .expect("FilterManager::received_cfilter: all headers up to the tip must exist");
        let filter = BlockFilter::new(&msg.filter);
        let block_hash = msg.block_hash;

        if filter.filter_header(&prev_header) != header {
            return Err(Error::InvalidMessage {
                from,
                reason: "cfilter: filter hash doesn't match header",
            });
        }

        self.upstream.event(Event::FilterReceived {
            from,
            block_hash,
            height,
            filter: filter.clone(),
        });

        if self.rescan.active && self.rescan.requested.remove(&height) {
            self.rescan.received.insert(height, (filter, block_hash));

            match self.process() {
                Ok(matches) => {
                    return Ok(matches);
                }
                Err(_err) => {
                    // TODO: We couldn't process all filters due to an invalid filter.
                    // We should probably do something about this!
                    // At least, log an event.
                }
            }
        }
        Ok(Vec::default())
    }

    /// Called when a peer disconnected.
    pub fn peer_disconnected(&mut self, id: &PeerId) {
        self.peers.remove(id);
    }

    /// Called when a new peer was negotiated.
    pub fn peer_negotiated<T: BlockTree>(
        &mut self,
        id: PeerId,
        height: Height,
        services: ServiceFlags,
        link: Link,
        clock: &impl Clock,
        tree: &T,
    ) {
        if !link.is_outbound() {
            return;
        }
        if !services.has(REQUIRED_SERVICES) {
            return;
        }
        let time = clock.local_time();

        self.peers.insert(
            id,
            Peer {
                last_active: time,
                height,
            },
        );
        self.sync(tree, time);
    }

    /// Send a `getcfheaders` message to a random peer.
    pub fn send_getcfheaders<T: BlockTree>(
        &mut self,
        range: Range<Height>,
        tree: &T,
        time: LocalTime,
    ) -> Option<(PeerId, Height, BlockHash)> {
        let count = range.end as usize - range.start as usize;

        debug_assert!(range.start < range.end);
        debug_assert!(!range.is_empty());

        if range.is_empty() {
            return None;
        }
        let start_height = range.start;

        // Cap request to `MAX_MESSAGE_CFHEADERS`.
        let stop_hash = if count > MAX_MESSAGE_CFHEADERS {
            let stop_height = range.start + MAX_MESSAGE_CFHEADERS as Height - 1;
            let stop_block = tree
                .get_block_by_height(stop_height)
                .expect("all headers up to the tip exist");

            stop_block.block_hash()
        } else {
            let (hash, _) = tree.tip();

            hash
        };
        if self.inflight.contains_key(&stop_hash) {
            // Don't request the same thing twice.
            return None;
        }

        // TODO: We should select peers that are caught up to the requested height.
        if let Some((peer, _)) = self.peers.sample() {
            self.upstream.get_cfheaders(
                *peer,
                start_height,
                stop_hash,
                self.config.request_timeout,
            );
            self.inflight.insert(stop_hash, time);

            return Some((*peer, start_height, stop_hash));
        } else {
            // TODO: Emit 'NotConnected' instead, and make sure we retry later, or when a
            // peer connects.
            self.upstream.event(Event::RequestCanceled {
                reason: "no peers with required services",
            });
        }
        None
    }

    /// Attempt to sync the filter header chain.
    pub fn sync<T: BlockTree>(&mut self, tree: &T, time: LocalTime) {
        let filter_height = self.filters.height();
        let block_height = tree.height();

        if filter_height < block_height {
            // We need to sync the filter header chain.
            let start_height = self.filters.height() + 1;
            let stop_height = tree.height();

            if let Some((peer, start_height, stop_hash)) =
                self.send_getcfheaders(start_height..stop_height + 1, tree, time)
            {
                self.upstream.event(Event::Syncing {
                    peer,
                    start_height,
                    stop_hash,
                });
            }
        } else if filter_height > block_height {
            panic!("{}: filter chain is longer than header chain!", source!());
        }
    }

    // PRIVATE METHODS /////////////////////////////////////////////////////////

    /// Called when filter headers were successfully imported.
    ///
    /// The height is the new filter header chain height, and the hash is the
    /// hash of the block corresponding to the last filter header.
    ///
    /// When new headers are imported, we want to download the corresponding compact filters
    /// to check them for matches.
    fn headers_imported<T: BlockTree>(
        &mut self,
        start: Height,
        stop: Height,
        tree: &T,
    ) -> Result<(), GetFiltersError> {
        if !self.rescan.active {
            return Ok(());
        }

        let start = Height::max(start, self.rescan.current);
        let stop = Height::min(stop, self.rescan.end.unwrap_or(stop));
        let range = start..=stop; // If the range is empty, it means we are not caught up yet.

        self.get_cfilters(range, tree)?;

        Ok(())
    }

    /// Process the next filters in the queue that can be processed.
    ///
    /// Checks whether any of the queued filters is next in line (by height) and if so,
    /// processes it and returns the result of trying to match it with the watch list.
    fn process(&mut self) -> Result<Vec<BlockHash>, bip158::Error> {
        // TODO: For BIP32 wallets, add one more address to check, if the
        // matching one was the highest-index one.
        let mut matches = Vec::new();
        let mut current = self.rescan.current;

        while let Some((filter, block_hash)) = self.rescan.received.remove(&current) {
            // Match scripts first, then match transactions. All outputs of a transaction must
            // match to consider the transaction matched.
            let mut matched = false;

            if !self.rescan.watch.is_empty() {
                matched = filter.match_any(
                    &block_hash,
                    &mut self.rescan.watch.iter().map(|k| k.as_bytes()),
                )?;
            }
            if !matched && !self.rescan.transactions.is_empty() {
                matched = self.rescan.transactions.values().any(|outs| {
                    let mut outs = outs.iter().map(|k| k.as_bytes());
                    filter.match_all(&block_hash, &mut outs).unwrap_or(false)
                })
            }

            if matched {
                matches.push(block_hash);
            }

            self.upstream.event(Event::FilterProcessed {
                block: block_hash,
                height: current,
                matched,
            });
            current += 1;
        }
        self.rescan.current = current;

        if let Some(stop) = self.rescan.end {
            if self.rescan.current == stop {
                self.rescan.active = false;
                self.upstream.event(Event::RescanCompleted { height: stop });
            }
        }

        Ok(matches)
    }
}

/// Iterator over height ranges.
struct HeightIterator {
    start: Height,
    stop: Height,
    step: Height,
}

impl Iterator for HeightIterator {
    type Item = Range<Height>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.start < self.stop {
            let start = self.start;
            let stop = self.stop.min(start + self.step - 1);

            self.start = stop + 1;

            Some(start..stop)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use bitcoin::consensus::Params;
    use bitcoin::network::message::NetworkMessage;
    use bitcoin::network::message_filter::GetCFilters;
    use bitcoin::BlockHeader;
    use bitcoin_hashes::hex::FromHex;
    use crossbeam_channel as chan;

    use nakamoto_chain::store::Genesis;
    use quickcheck::TestResult;
    use quickcheck_macros::quickcheck;

    use nakamoto_chain::block::{cache::BlockCache, store};
    use nakamoto_chain::filter::cache::{FilterCache, StoredHeader};
    use nakamoto_common::block::filter::{FilterHash, FilterHeader};
    use nakamoto_common::network::Network;
    use nakamoto_common::nonempty::NonEmpty;
    use nakamoto_test::block::gen;
    use nakamoto_test::BITCOIN_HEADERS;

    use crate::protocol::channel::Channel;
    use crate::protocol::test::messages;
    use crate::protocol::PROTOCOL_VERSION;
    use crate::protocol::{self, Out};

    use super::*;

    mod util {
        use super::*;

        pub fn setup(
            network: Network,
            height: Height,
        ) -> (
            FilterManager<FilterCache<store::Memory<StoredHeader>>, Channel>,
            BlockCache<store::Memory<BlockHeader>>,
            NonEmpty<bitcoin::Block>,
            chan::Receiver<Out>,
        ) {
            let mut rng = fastrand::Rng::new();
            let genesis = network.genesis_block();
            let chain = gen::blockchain(genesis, height, &mut rng);
            let (sender, outputs) = chan::unbounded();
            let tree = {
                let headers = NonEmpty::from_vec(chain.iter().map(|b| b.header).collect()).unwrap();
                let store = store::Memory::new(headers);
                let params = Params::new(network.into());

                BlockCache::from(store, params, &[]).unwrap()
            };

            let cache = FilterCache::from(store::memory::Memory::genesis(network)).unwrap();
            let upstream = Channel::new(network, PROTOCOL_VERSION, "test", sender);

            (
                FilterManager::new(Config::default(), rng, cache, upstream),
                tree,
                chain,
                outputs,
            )
        }

        #[allow(dead_code)]
        pub fn is_sorted<T>(data: &[T]) -> bool
        where
            T: Ord,
        {
            data.windows(2).all(|w| w[0] <= w[1])
        }

        pub fn events(receiver: &chan::Receiver<Out>) -> impl Iterator<Item = Event> + '_ {
            receiver.try_iter().filter_map(|o| match o {
                Out::Event(protocol::Event::FilterManager(e)) => Some(e),
                _ => None,
            })
        }
    }

    const FILTER_HASHES: [&str; 15] = [
        "9acd599f31639d36b8e531d12afb430bb17e7cdd6e73c993c343e417cda1f299",
        "0bfdf66fef865ea20f1a3c4d12a9570685aa89cdd8a950755ef7e870520533ad",
        "155215e98328f097cf085f721edff6f4e9e1072e14012052b86297aa21085dcb",
        "227a8f6d137745df7445afcc5b1484c5a70bd1edb2f2886943dcb396803d1d85",
        "fb86fad94ad95c042894083c7dce973406481b0fd674163fde5d4f52a7bc074d",
        "37a8db7d504b65c63f0d5559ab616e586257b3d0672d574e7fcc7018eb45aa35",
        "a1a81f3571c98b30ce69ddf2f9e6a014074d73327d0e0d6cdc4d493fe64e3f2a",
        "a16c3a9a9da80a10999f73e88fbf5cd63a0266115c5f1f683ee1f1c534ad232d",
        "f52a72367e64fffdbd5239c00f380db0ac77901a8a8faa9c642d592b87b4b7ca",
        "81c4c5606d54107bfb9dccbaf23b7a2459f8444816623ba23e3de91f16a525da",
        "1f64677b953cbc851277f95edb29065c7859cae744ef905b5950f8e79ed97c8a",
        "8cde7d77626801155a891eea0688d7eb5c37ca74d84493254ff4e4c2a886de4a",
        "3eb61e435e1ed1675b5c1fcc4a89b4dba3695a8b159aabe4c03833ecd7c41704",
        "802221cd81ad57748b713d8055b5fc6d5f7cef71b9d59d690857ef835704cab8",
        "503adfa2634006e453900717f070ffc11a639ee1a0416e4e137f396c7706e6b7",
    ];

    const FILTERS: [&[u8]; 11] = [
        &[1, 127, 168, 128],
        &[1, 140, 59, 16],
        &[1, 140, 120, 216],
        &[1, 19, 255, 16],
        &[1, 63, 182, 112],
        &[1, 56, 58, 48],
        &[1, 12, 113, 176],
        &[1, 147, 204, 216],
        &[1, 117, 5, 160],
        &[1, 141, 61, 184],
        &[1, 155, 155, 152],
    ];

    #[test]
    fn test_receive_filters() {
        let network = Network::Mainnet;
        let peer = &([0, 0, 0, 0], 0).into();
        let time = LocalTime::now();
        let tree = {
            let genesis = network.genesis();
            let params = network.params();

            assert_eq!(genesis, BITCOIN_HEADERS.head);

            BlockCache::from(store::Memory::new(BITCOIN_HEADERS.clone()), params, &[]).unwrap()
        };
        let (sender, _receiver) = chan::unbounded();

        let mut cbfmgr = {
            let rng = fastrand::Rng::new();
            let cache = FilterCache::from(store::memory::Memory::genesis(network)).unwrap();
            let upstream = Channel::new(network, PROTOCOL_VERSION, "test", sender);

            FilterManager::new(Config::default(), rng, cache, upstream)
        };

        // Import the headers.
        {
            let msg = CFHeaders {
                filter_type: 0,
                stop_hash: BlockHash::from_hex(
                    "00000000b3322c8c3ef7d2cf6da009a776e6a99ee65ec5a32f3f345712238473",
                )
                .unwrap(),
                previous_filter_header: FilterHeader::from_hex(
                    "02c2392180d0ce2b5b6f8b08d39a11ffe831c673311a3ecf77b97fc3f0303c9f",
                )
                .unwrap(),
                filter_hashes: FILTER_HASHES
                    .iter()
                    .map(|h| FilterHash::from_hex(h).unwrap())
                    .collect(),
            };
            cbfmgr.inflight.insert(msg.stop_hash, time);
            cbfmgr.received_cfheaders(peer, msg, &tree, time).unwrap();
        }

        assert_eq!(cbfmgr.filters.height(), 15);
        cbfmgr.filters.verify(network).unwrap();

        let cfilters = FILTERS
            .iter()
            .zip(BITCOIN_HEADERS.iter())
            .map(|(f, h)| CFilter {
                filter_type: 0x0,
                block_hash: h.block_hash(),
                filter: f.to_vec(),
            });

        // Now import the filters.
        for msg in cfilters {
            cbfmgr.received_cfilter(peer, msg, &tree).unwrap();
        }
    }

    #[test]
    fn test_height_iterator() {
        let mut it = super::HeightIterator {
            start: 3,
            stop: 19,
            step: 5,
        };
        assert_eq!(it.next(), Some(3..7));
        assert_eq!(it.next(), Some(8..12));
        assert_eq!(it.next(), Some(13..17));
        assert_eq!(it.next(), Some(18..19));
        assert_eq!(it.next(), None);
    }

    /// Test that we can start a rescan without any peers, and it'll pick up when peers connect.
    #[test]
    #[ignore]
    fn test_not_connected() {
        todo!()
    }

    /// Test that we can specify a birth date in the future.
    #[test]
    #[ignore]
    fn test_rescan_future_birth() {
        todo!()
    }

    /// Test that an unbounded rescan will continuously ask for filters.
    #[test]
    #[ignore]
    fn test_rescan_unbouned() {
        todo!()
    }

    /// Test that a bounded rescan will eventually complete.
    #[test]
    #[ignore]
    fn test_rescan_completed() {
        todo!()
    }

    /// Test that an empty watchlist can never match a block.
    #[test]
    #[ignore]
    fn test_empty_watchlist() {
        todo!()
    }

    /// Test that rescanning triggers filter syncing immediately.
    #[test]
    fn test_rescan_getcfilters() {
        let birth = 11;
        let best = 42;
        let time = LocalTime::now();
        let network = Network::Regtest;
        let (mut cbfmgr, tree, chain, outputs) = util::setup(network, best);
        let mut msgs = protocol::test::messages(&outputs);
        let remote: PeerId = ([88, 88, 88, 88], 8333).into();
        let tip = tree.get_block_by_height(best).unwrap().block_hash();
        let filter_type = 0x0;
        let previous_filter_header = FilterHeader::genesis(network);
        let filter_hashes = gen::cfheaders_from_blocks(previous_filter_header, chain.iter())
            .into_iter()
            .skip(1) // Skip genesis
            .map(|(h, _)| h)
            .collect::<Vec<_>>();

        cbfmgr.initialize(time);
        cbfmgr.peer_negotiated(
            remote,
            best,
            REQUIRED_SERVICES,
            Link::Outbound,
            &time,
            &tree,
        );
        msgs.find(|(_, m)| matches!(m, NetworkMessage::GetCFHeaders(_)))
            .unwrap();

        cbfmgr
            .received_cfheaders(
                &remote,
                CFHeaders {
                    filter_type,
                    stop_hash: tip,
                    previous_filter_header,
                    filter_hashes,
                },
                &tree,
                time,
            )
            .unwrap();

        // Start rescan.
        cbfmgr
            .rescan(Bound::Included(birth), Bound::Unbounded, vec![], &tree)
            .unwrap();

        let expected = GetCFilters {
            filter_type,
            start_height: birth as u32,
            stop_hash: tip,
        };
        msgs.find(|(_, m)| matches!(m, NetworkMessage::GetCFilters(msg) if msg == &expected))
            .expect("Rescanning should trigger filters to be fetched");
    }

    /// Test that if we start with our cfheader chain behind our header
    /// chain, we immediately try to catch up.
    #[test]
    #[ignore]
    fn test_cfheaders_behind() {
        todo!()
    }

    #[quickcheck]
    fn prop_rescan(birth: Height, best: Height) -> quickcheck::TestResult {
        // We don't gain anything by testing longer chains.
        if !(1..16).contains(&best) || birth > best {
            return TestResult::discard();
        }
        log::debug!("-- Test case with birth = {}, best = {} --", birth, best);

        let mut rng = fastrand::Rng::new();
        let network = Network::Regtest;
        let remote: PeerId = ([88, 88, 88, 88], 8333).into();

        let (mut cbfmgr, tree, chain, outputs) = util::setup(network, best);
        let time = LocalTime::now();
        let tip = chain.last().block_hash();
        let filter_type = 0x0;

        // Generate a watchlist and keep track of the matching block heights.
        let (watch, heights, _) = gen::watchlist(birth, chain.iter(), &mut rng);

        cbfmgr.initialize(time);
        cbfmgr.peer_negotiated(
            remote,
            best,
            REQUIRED_SERVICES,
            Link::Outbound,
            &time,
            &tree,
        );
        cbfmgr
            .rescan(Bound::Included(birth), Bound::Unbounded, watch, &tree)
            .unwrap();

        let mut msgs = messages(&outputs);
        let mut events = util::events(&outputs);

        msgs.find(|(_, m)| {
            dbg!(&m);
            matches!(
                m,
                NetworkMessage::GetCFHeaders(GetCFHeaders {
                    start_height,
                    stop_hash,
                    ..
                }) if *start_height == 1 && stop_hash == &tip
            )
        })
        .unwrap();

        let previous_filter_header = FilterHeader::genesis(network);
        let filter_hashes = gen::cfheaders_from_blocks(previous_filter_header, chain.iter())
            .into_iter()
            .skip(1) // Skip genesis
            .map(|(h, _)| h)
            .collect::<Vec<_>>();

        let height = cbfmgr
            .received_cfheaders(
                &remote,
                CFHeaders {
                    filter_type,
                    stop_hash: tip,
                    previous_filter_header,
                    filter_hashes,
                },
                &tree,
                time,
            )
            .unwrap();

        assert_eq!(height, best, "The new height is the best height");

        msgs.find(|(_, m)| {
            matches!(
                m,
                NetworkMessage::GetCFilters(GetCFilters {
                    start_height,
                    stop_hash,
                    ..
                }) if *start_height as Height == birth && stop_hash == &tip
            )
        })
        .unwrap();

        events
            .find(|e| matches!(e, Event::Synced(height) if height == &best))
            .unwrap();

        let mut filters: Vec<_> = (birth..=best)
            .map(|height| {
                let block = &chain[height as usize];
                let block_hash = block.block_hash();
                let filter = gen::cfilter(block);

                (
                    height,
                    CFilter {
                        filter_type,
                        block_hash,
                        filter: filter.content,
                    },
                )
            })
            .collect();

        // Shuffle filters so that they arrive out-of-order.
        rng.shuffle(&mut filters);

        let mut matches = Vec::new();
        for (h, filter) in filters {
            let hashes = cbfmgr.received_cfilter(&remote, filter, &tree).unwrap();

            matches.extend(
                hashes
                    .into_iter()
                    .filter_map(|h| tree.get_block(&h).map(|(height, _)| height)),
            );
            events
                .find(|e| matches!(e, Event::FilterReceived { height, .. } if height == &h))
                .unwrap();
        }

        assert_eq!(
            matches, heights,
            "The blocks requested are the ones that matched"
        );
        quickcheck::TestResult::passed()
    }
}
