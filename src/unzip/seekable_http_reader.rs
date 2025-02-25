// Copyright 2022 Google LLC

// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::{
    cmp::min,
    collections::BTreeMap,
    io::{BufReader, ErrorKind, Read, Seek, SeekFrom},
    sync::{Arc, Condvar, Mutex},
};

use reqwest::blocking::Response;
use thiserror::Error;

use super::{
    cloneable_seekable_reader::HasLength,
    http_range_reader::{self, RangeFetcher},
};

/// This is how much we read from the underlying HTTP stream in a given thread,
/// before signalling other threads that they may wish to continue with their
/// CPU-bound unzipping. Empirically determined.
/// 128KB = 172ms
/// 512KB = 187ms
/// 1024KB = 152ms
/// 2048KB = 170ms
/// If we set this too high, we starve multiple threads - they can't start
/// acting on the data to unzip their files until the read is complete. If we
/// set this too low, the cache structure (a `BTreeMap`) becomes dominant in
/// CPU usage.
const MAX_BLOCK: usize = 1024 * 1024;

/// A hint to the [`SeekableHttpReaderEngine`] about the expected access pattern.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum AccessPattern {
    /// We expect accesses all over the file.
    RandomAccess,
    /// We expect accesses starting from the beginning and moving to the end,
    /// though there might be some jumping around if multiple threads are
    /// reading from roughly the same area of the file.
    SequentialIsh,
}

impl Default for AccessPattern {
    fn default() -> Self {
        Self::RandomAccess
    }
}

/// Errors that may be returned by a [`SeekableHttpReaderEngine` or `SeekableHttpReader`].
#[derive(Error, Debug)]
pub(crate) enum Error {
    #[error(
        "This HTTP resource did not advertise that it accepts ranges via the Accept-Ranges header"
    )]
    AcceptRangesNotSupported,
    #[error(transparent)]
    RangeFetcherError(http_range_reader::Error),
}

/// Some data that we've read from the network, but not yet returned to the
/// caller.
struct CacheCell {
    data: Vec<u8>,
    bytes_read: usize,
}

impl CacheCell {
    fn new(data: Vec<u8>) -> Self {
        Self {
            data,
            bytes_read: 0,
        }
    }

    fn len(&self) -> usize {
        self.data.len()
    }

    fn entirely_consumed(&self) -> bool {
        self.bytes_read >= self.len()
    }
}

/// Internal state of the [`SeekableHttpReaderEngine`], in a separate struct
/// because access is protected by a mutex.
#[derive(Default)]
struct State {
    /// The expected pattern of seeks and reads; a hint from the user.
    access_pattern: AccessPattern,
    /// Maximum size of the "cache"
    readahead_limit: Option<usize>,
    /// Current size of the cache
    current_size: usize,
    /// The readahead "cache", which is not really a cache in the strict sense,
    /// but is any data that we've already read from the underlying stream
    /// that is yet to be read by any reader.
    /// This exists because we assume we'll get accesses in any random order,
    /// and yet we don't want to create a new HTTP stream each time we need
    /// to rewind a bit. Therefore if we fast-forward, we store any data that
    /// we skipped over, in order to service any subsequent requests for those
    /// positions.
    cache: BTreeMap<u64, CacheCell>,
    /// Whether a read from the underlying HTTP stream is afoot. Only one thread
    /// can be doing a read at a time.
    read_in_progress: bool,
    /// Some statistics about how we're doing.
    stats: SeekableHttpReaderStatistics,
}

impl State {
    fn new(readahead_limit: Option<usize>, access_pattern: AccessPattern) -> Self {
        // Grow the readahead limit if it's less than block size, because we
        // must always store one block in order to service the most recent read.
        let readahead_limit = match readahead_limit {
            Some(readahead_limit) if readahead_limit > MAX_BLOCK => Some(readahead_limit),
            Some(_) => Some(MAX_BLOCK),
            _ => None,
        };
        Self {
            readahead_limit,
            access_pattern,
            ..Default::default()
        }
    }

    /// Insert a block into our readahead cache.
    fn insert(&mut self, pos: u64, block: Vec<u8>) {
        log::info!(
            "Inserting into cache, block is 0x{:x}-0x{:x}",
            pos,
            pos + block.len() as u64
        );
        let extra_size = block.len();
        self.cache.insert(pos, CacheCell::new(block));
        self.current_size += extra_size;
        if let Some(readahead_limit) = self.readahead_limit {
            // Shrink
            while self.current_size > readahead_limit {
                self.stats.cache_shrinks += 1;
                let first_block = self.cache.iter().next().map(|(pos, _)| pos).cloned();
                if let Some(pos) = first_block {
                    let block = self.cache.remove(&pos).unwrap();
                    self.current_size -= block.len();
                }
            }
        }
    }

    /// Read from the readahead cache, if we can.
    /// If '`discard_read_data` is true, we assume that all data
    /// will be consumed exactly once, so we discard the data that has been read.
    /// Sometimes we'll have blocks of data where we only want to read part of it,
    /// so then we will split the block and merely retain the bits that are
    /// not yet read by the readers.
    fn read_from_cache(&mut self, pos: u64, buf: &mut [u8]) -> Option<usize> {
        let discard_read_data = matches!(self.access_pattern, AccessPattern::SequentialIsh);
        let mut block_to_discard = None;
        let mut return_value = None;
        for (possible_block_start, block) in
            self.cache.range_mut(pos - min(pos, MAX_BLOCK as u64)..=pos)
        {
            let block_offset = pos as usize - *possible_block_start as usize;
            let block_len = block.len();
            if block_offset >= block_len {
                // This block is indeed before the read we want to do,
                // but doesn't extend as far as the starting point of our read.
                continue;
            }
            // OK, we've found a block which overlaps with the read that we
            // want to do.

            let block_len = block.len();
            let block_offset = pos as usize - *possible_block_start as usize;
            let to_read = min(buf.len(), block_len - block_offset);
            buf[..to_read].copy_from_slice(&block.data[block_offset..to_read + block_offset]);
            block.bytes_read += to_read;
            self.stats.cache_hits += 1;
            if discard_read_data && block.entirely_consumed() {
                // Discard this block, but outside this loop
                block_to_discard = Some(*possible_block_start);
                self.current_size -= block.len();
            }
            return_value = Some(to_read);
            break;
        }
        if let Some(block_to_discard) = block_to_discard {
            self.cache.remove(&block_to_discard);
        }
        return_value
    }
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cache")
            .field("max_size", &self.readahead_limit)
            .field("current_size", &self.current_size)
            .finish()
    }
}

/// Items related to reading from the underlying HTTP streams. This is
/// in a separate struct because it's protected by a mutex.
struct ReadingMaterials {
    range_fetcher: RangeFetcher,
    reader: Option<(BufReader<Response>, u64)>, // second item in tuple is current reader pos
}

/// A type which can produce objects that can be [`Read`] and [`Seek`] even
/// though they're accessing remote HTTP resources. This object in itself doesn't
/// support those traits, but its [`create_reader`] method can be used to emit
/// objects that do. This object can only be used to access HTTP resources which
/// support the `Range` header - an error will be reported on construction
/// of this object if such ranges are not supported by the remote server.
pub(crate) struct SeekableHttpReaderEngine {
    /// Total stream length
    len: u64,
    /// Facilities to read from the underlying HTTP stream(s)
    reader: Mutex<ReadingMaterials>,
    /// Overall state of this object, mostly related to the readahead cache
    /// of blocks we already read, but also with the all-important boolean
    /// stating whether any thread is already reading on the underlying stream.
    state: Mutex<State>,
    /// Condition variable to indicate that there's a new block in the
    /// readahead cache and all other threads should consider if their read
    /// request can be serviced.
    read_completed: Condvar,
}

/// Some results about the success (or otherwise) of this reader.
#[derive(Default, Debug, Clone)]
pub(crate) struct SeekableHttpReaderStatistics {
    /// The number of times we had to create an HTTP(S) stream.
    pub(crate) num_http_streams: usize,
    /// Number of times we found the read that we wanted in the cache
    /// of previous reads.
    pub(crate) cache_hits: usize,
    /// Number of times we had to actually do a read on the underlying stream.
    pub(crate) cache_misses: usize,
    /// Number of times we had to discard data from the cache because it
    /// was too big.
    pub(crate) cache_shrinks: usize,
}

impl SeekableHttpReaderEngine {
    /// Create a new seekable HTTP reader engine for this URI. This constructor
    /// will query the server to discover whether it supports HTTP ranges;
    /// if not, an error will be returned.
    pub(crate) fn new(
        uri: String,
        readahead_limit: Option<usize>,
        access_pattern: AccessPattern,
    ) -> Result<Arc<Self>, Error> {
        let range_fetcher = RangeFetcher::new(uri).map_err(Error::RangeFetcherError)?;
        if !range_fetcher.accepts_ranges() {
            return Err(Error::AcceptRangesNotSupported);
        }
        let len = range_fetcher.len();
        Ok(Arc::new(Self {
            len,
            reader: Mutex::new(ReadingMaterials {
                range_fetcher,
                reader: None,
            }),
            state: Mutex::new(State::new(readahead_limit, access_pattern)),
            read_completed: Condvar::new(),
        }))
    }

    /// Create an object which can be used to read from this HTTP location
    /// in a seekable fashion.
    pub(crate) fn create_reader(self: Arc<Self>) -> SeekableHttpReader {
        SeekableHttpReader {
            engine: self,
            pos: 0u64,
        }
    }

    /// Read some data, ideally from the cache of pre-read blocks, but
    /// otherwise from the underlying HTTP stream.
    fn read(&self, buf: &mut [u8], pos: u64) -> std::io::Result<usize> {
        // There is some mutex delicacy here. Goals are:
        // a) Allow exactly one thread to be reading on the underlying HTTP stream;
        // b) Allow other threads to query the cache of already-read blocks
        //    without blocking on ongoing reads on the stream.
        // We therefore need two mutexes - one for the cache (and, our state in
        // general) and another for the actual HTTP stream reader.
        // There is a risk of deadlock between these mutexes, since to do
        // an actual read we will need to release the state mutex to allow
        // others to do the reads. We avoid this by ensuring only a single
        // thread ever has permission to do anything with the reader mutex.
        // Specifically:
        // Claim STATE mutex
        // Is there block in cache?
        // - If yes, release STATE mutex, and return
        // - If no, check if read in progress
        //   Is there read in progress?
        //   - If yes, release STATE mutex, WAIT on condvar atomically
        //     check cache again
        //   - If no:
        //     set read in progress
        //     claim READER mutex
        //     release STATE mutex
        //     perform read
        //     claim STATE mutex
        //     insert results
        //     set read not in progress
        //     release STATE mutex
        //     release READER mutex
        //     NOTIFYALL on condvar

        // Cases where you have STATE but want READER: near the start
        // Cases where you have READER but want STATE: after read,
        // ... but this deadlock can't happen because only one thread
        //     will enter this 'read in progress' block.
        log::info!("Read: requested position 0x{:x}.", pos);

        if pos == self.len {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "read beyond end of stream",
            ));
        }

        // Claim CACHE mutex
        let mut state = self.state.lock().unwrap();
        // Is there block in cache?
        // - If yes, release CACHE mutex, and return
        if let Some(bytes_read_from_cache) = state.read_from_cache(pos, buf) {
            log::info!("Immediate cache success");
            return Ok(bytes_read_from_cache);
        }
        // - If no, check if read in progress
        let mut read_in_progress = state.read_in_progress;
        //   Is there read in progress?
        while read_in_progress {
            //   - If yes, release CACHE mutex, WAIT on condvar atomically
            state = self.read_completed.wait(state).unwrap();
            //     check cache again
            if let Some(bytes_read_from_cache) = state.read_from_cache(pos, buf) {
                log::info!("Deferred cache success");
                return Ok(bytes_read_from_cache);
            }
            read_in_progress = state.read_in_progress;
        }
        state.stats.cache_misses += 1;
        //   - If no:
        //     set read in progress
        state.read_in_progress = true;
        //     claim READER mutex
        let mut reading_stuff = self.reader.lock().unwrap();
        //     release STATE mutex
        drop(state);
        //     perform read
        // First check if we need to rewind.
        if let Some((_, readerpos)) = reading_stuff.reader.as_ref() {
            if pos < *readerpos {
                log::info!(
                    "New reader will be required at 0x{:x} - old reader pos was 0x{:x}",
                    pos,
                    *readerpos
                );
                reading_stuff.reader = None;
            }
        }
        let mut reader_created = false;
        if reading_stuff.reader.is_none() {
            log::info!("create_reader");
            reading_stuff.reader = Some((
                BufReader::new(
                    reading_stuff
                        .range_fetcher
                        .fetch_range(pos)
                        .map_err(|e| std::io::Error::new(ErrorKind::Unsupported, e.to_string()))?,
                ),
                pos,
            ));
            reader_created = true;
        };

        let (reader, reader_pos) = reading_stuff.reader.as_mut().unwrap();
        if pos > *reader_pos {
            log::info!("Read: fast-forward from 0x{:x} to 0x{:x}", *reader_pos, pos);
        }
        while pos >= *reader_pos {
            // Fast forward beyond the desired position, recording any reads in the cache
            // for later.
            let to_read = min(MAX_BLOCK, self.len as usize - *reader_pos as usize);
            let mut new_block = vec![0u8; to_read];
            reader.read_exact(&mut new_block)?;
            //     claim STATE mutex
            let mut state = self.state.lock().unwrap();
            state.insert(*reader_pos, new_block);
            // Tell any waiting threads they should re-check the cache
            self.read_completed.notify_all();
            *reader_pos += to_read as u64;
        }
        // Because the above condition is >=, and because we know the request was not
        // to read at the very end of the file, we know we now have some data in the
        // cache which can satisfy the request.
        //     claim STATE mutex
        let mut state = self.state.lock().unwrap();
        let bytes_read = state
            .read_from_cache(pos, buf)
            .expect("Cache still couldn't satisfy request event after reading beyond read pos");
        log::info!("Cache success after read");
        if reader_created {
            state.stats.num_http_streams += 1;
        }
        //     set read not in progress
        state.read_in_progress = false;
        //     release STATE mutex
        //     release READER mutex
        Ok(bytes_read)
    }

    /// The total length of the underlying resource.
    pub(crate) fn len(&self) -> u64 {
        self.len
    }

    /// Update the expected access pattern. You must not call this when
    /// any threads might be reading from any [`SeekableHttpReader`] created
    /// by this engine; that may panic.
    pub(crate) fn set_expected_access_pattern(&self, access_pattern: AccessPattern) {
        let mut state = self.state.lock().unwrap();
        let old_access_pattern = state.access_pattern;
        if old_access_pattern == access_pattern {
            return;
        }
        log::info!(
            "Changing access pattern - current stats are {:?}",
            state.stats
        );
        if matches!(access_pattern, AccessPattern::SequentialIsh) {
            if state.read_in_progress {
                panic!("Must not call set_expected_access_pattern while a read is in progress");
            }
            // If we're switching to a sequential pattern, recreate
            // the reader at position zero.
            log::info!("create_reader_at_zero");
            {
                let mut reading_materials = self.reader.lock().unwrap();
                let new_reader = reading_materials.range_fetcher.fetch_range(0);
                if let Ok(new_reader) = new_reader {
                    reading_materials.reader = Some((BufReader::new(new_reader), 0));
                }
            }
            state.stats.num_http_streams += 1;
        }
        state.access_pattern = access_pattern;
    }

    /// Return some statistics about the success (or otherwise) of this stream.
    pub(crate) fn get_stats(&self) -> SeekableHttpReaderStatistics {
        self.state.lock().unwrap().stats.clone()
    }
}

impl Drop for SeekableHttpReaderEngine {
    fn drop(&mut self) {
        log::info!("Dropping: stats are {:?}", self.state.lock().unwrap().stats)
    }
}

/// A [`Read`] which is also [`Seek`] to read from arbitrary places on an
/// HTTP stream. Cheap to clone. Create using [`SeekableHttpReader::create_reader`].
#[derive(Clone)]
pub(crate) struct SeekableHttpReader {
    engine: Arc<SeekableHttpReaderEngine>,
    pos: u64,
}

impl Seek for SeekableHttpReader {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        // TODO used checked arithmetic when stabilized
        self.pos = match pos {
            SeekFrom::Start(pos) => pos,
            SeekFrom::End(pos) => {
                if -pos > self.engine.len() as i64 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Unsupported,
                        "Rewind too far",
                    ));
                }
                self.engine.len() - ((-pos) as u64)
            }
            SeekFrom::Current(offset_from_pos) => {
                if offset_from_pos > 0 {
                    self.pos + (offset_from_pos as u64)
                } else {
                    self.pos - ((-offset_from_pos) as u64)
                }
            }
        };
        Ok(self.pos)
    }
}

impl Read for SeekableHttpReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let bytes_read = self.engine.read(buf, self.pos)?;
        self.pos += bytes_read as u64;
        Ok(bytes_read)
    }
}

impl HasLength for SeekableHttpReader {
    fn len(&self) -> u64 {
        self.engine.len()
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom};
    use test_log::test;

    use httptest::{matchers::*, responders::*, Expectation, Server};

    use super::{AccessPattern, SeekableHttpReaderEngine};

    #[test]
    fn test_unlimited_readahead() {
        do_test(None, AccessPattern::SequentialIsh)
    }

    #[test]
    fn test_big_readahead() {
        const ONE_HUNDRED_MB: usize = 1024usize * 1024usize * 100usize;
        do_test(Some(ONE_HUNDRED_MB), AccessPattern::SequentialIsh)
    }

    #[test]
    fn test_small_readahead() {
        do_test(Some(4), AccessPattern::SequentialIsh)
    }

    #[test]
    fn test_random_access() {
        do_test(None, AccessPattern::RandomAccess)
    }

    fn do_test(readahead_limit: Option<usize>, access_pattern: AccessPattern) {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("HEAD", "/foo")).respond_with(
                status_code(200)
                    .insert_header("Accept-Ranges", "bytes")
                    .insert_header("Content-Length", "12")
                    .body("0123456789AB"),
            ),
        );

        let mut seekable_http_reader = SeekableHttpReaderEngine::new(
            server.url("/foo").to_string(),
            readahead_limit,
            access_pattern,
        )
        .unwrap()
        .create_reader();
        let mut throwaway = [0u8; 4];

        server.expect(
            Expectation::matching(request::method_path("GET", "/foo"))
                .times(..)
                .respond_with(
                    status_code(200)
                        .insert_header("Accept-Ranges", "bytes")
                        .insert_header("Content-Length", "12")
                        .body("0123456789AB"),
                ),
        );
        seekable_http_reader.read_exact(&mut throwaway).unwrap();
        assert_eq!(std::str::from_utf8(&throwaway).unwrap(), "0123");
        seekable_http_reader.read_exact(&mut throwaway).unwrap();
        assert_eq!(std::str::from_utf8(&throwaway).unwrap(), "4567");
        seekable_http_reader.stream_position().unwrap();
        seekable_http_reader.read_exact(&mut throwaway).unwrap();
        assert_eq!(std::str::from_utf8(&throwaway).unwrap(), "89AB");
        seekable_http_reader.rewind().unwrap();
        seekable_http_reader.read_exact(&mut throwaway).unwrap();
        assert_eq!(std::str::from_utf8(&throwaway).unwrap(), "0123");
        seekable_http_reader.read_exact(&mut throwaway).unwrap();
        assert_eq!(std::str::from_utf8(&throwaway).unwrap(), "4567");

        server.expect(
            Expectation::matching(request::method_path("GET", "/foo"))
                .times(..)
                .respond_with(
                    status_code(200)
                        .insert_header("Accept-Ranges", "bytes")
                        .insert_header("Content-Length", "8")
                        .body("456789AB"),
                ),
        );

        seekable_http_reader.seek(SeekFrom::Start(4)).unwrap();
        seekable_http_reader.read_exact(&mut throwaway).unwrap();
        assert_eq!(std::str::from_utf8(&throwaway).unwrap(), "4567");
    }
}
