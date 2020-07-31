use crate::{pack, pack::index::util::Chunks};
use git_features::{parallel, parallel::in_parallel_if, progress::Progress};
use smallvec::alloc::collections::BTreeMap;
use std::{convert::TryInto, io};

mod encode;
mod error;
pub use error::Error;

mod types;
pub use types::*;

mod consume;
use consume::apply_deltas;

/// Various ways of writing an index file from pack entries
impl pack::index::File {
    /// Note that neither in-pack nor out-of-pack Ref Deltas are supported here, these must have been resolved beforehand.
    pub fn write_data_iter_to_stream<F>(
        kind: pack::index::Kind,
        mode: Mode<F>,
        entries: impl Iterator<Item = Result<pack::data::iter::Entry, pack::data::iter::Error>>,
        thread_limit: Option<usize>,
        _progress: impl Progress,
        out: impl io::Write,
    ) -> Result<Outcome, Error>
    where
        F: for<'r> Fn(ResolveContext, &'r mut Vec<u8>) -> bool + Send + Sync,
    {
        if kind != pack::index::Kind::default() {
            return Err(Error::Unsupported(kind));
        }
        let mut num_objects = 0;
        let mut bytes_to_process = 0u64;
        // This array starts out sorted by pack-offset
        let mut index_entries = Vec::with_capacity(entries.size_hint().0);
        if index_entries.capacity() == 0 {
            return Err(Error::IteratorInvariantNonEmpty);
        }
        let mut last_seen_trailer = None;
        let mut last_base_index = None;
        let mut last_pack_offset = 0;
        // TODO: This should soon become a dashmap (in fast mode, or a Mutex protected shared map) as this will be edited
        // by threads to remove now unused caches. Probably also a good moment to switch to parking lot mutexes everywhere.
        let mut cache_by_offset = BTreeMap::<_, CacheEntry>::new();
        for (eid, entry) in entries.enumerate() {
            use pack::data::Header::*;

            let pack::data::iter::Entry {
                header,
                pack_offset,
                header_size,
                compressed,
                decompressed,
                trailer,
            } = entry?;
            let compressed_len = compressed.len();
            if !(pack_offset > last_pack_offset) {
                return Err(Error::IteratorInvariantIncreasingPackOffset(
                    last_pack_offset,
                    pack_offset,
                ));
            }
            last_pack_offset = pack_offset;
            num_objects += 1;
            bytes_to_process += decompressed.len() as u64;
            let (cache, kind) = match header {
                Blob | Tree | Commit | Tag => {
                    last_base_index = Some(eid);
                    (
                        mode.base_cache(compressed, decompressed),
                        ObjectKind::Base(header.to_kind().expect("a base object")),
                    )
                }
                RefDelta { .. } => return Err(Error::IteratorInvariantNoRefDelta),
                OfsDelta {
                    pack_offset: base_pack_offset,
                } => {
                    cache_by_offset
                        .get_mut(&base_pack_offset)
                        .ok_or_else(|| {
                            Error::IteratorInvariantBasesBeforeDeltasNeedThem(pack_offset, base_pack_offset)
                        })?
                        .increment_child_count();
                    (
                        mode.delta_cache(compressed, decompressed),
                        ObjectKind::OfsDelta(base_pack_offset),
                    )
                }
            };

            cache_by_offset.insert(pack_offset, CacheEntry::new(cache));
            index_entries.push(Entry {
                pack_offset,
                entry_len: header_size as u64 + compressed_len as u64,
                kind,
                crc32: 0, // TBD, but can be done right here, needs header encoding
            });
            last_seen_trailer = trailer;
        }

        // Prevent us from trying to find bases for resolution past the point where they are
        let (chunk_size, thread_limit, _) = parallel::optimize_chunk_size_and_thread_limit(1, None, thread_limit, None);
        let last_base_index = last_base_index.ok_or(Error::IteratorInvariantBasesPresent)?;
        let num_objects: u32 = num_objects
            .try_into()
            .map_err(|_| Error::IteratorInvariantTooManyObjects(num_objects))?;
        let cache_by_offset = parking_lot::Mutex::new(cache_by_offset);
        let mut sorted_pack_offsets_by_oid = {
            let mut items = in_parallel_if(
                || bytes_to_process > 5_000_000,
                Chunks {
                    iter: index_entries.iter().take(last_base_index).filter(|e| e.kind.is_base()),
                    size: chunk_size,
                },
                thread_limit,
                |_thread_index| Vec::with_capacity(4096),
                |base_pack_offsets, state| {
                    apply_deltas(
                        base_pack_offsets,
                        state,
                        &index_entries,
                        &cache_by_offset,
                        &mode,
                        kind.hash(),
                    )
                },
                Reducer::new(num_objects),
            )?;
            items.sort_by_key(|e| e.1);
            items
        };

        // Bring crc32 back into our perfectly sorted oid which is sorted by oid
        for (pack_offset, _oid, crc32) in sorted_pack_offsets_by_oid.iter_mut() {
            let index = index_entries
                .binary_search_by_key(pack_offset, |e| e.pack_offset)
                .expect("both arrays to have the same pack-offsets");
            *crc32 = index_entries[index].crc32;
        }
        drop(cache_by_offset);

        let index_hash = encode::to_write(out, index_entries, kind)?;

        Ok(Outcome {
            index_kind: kind,
            index_hash,
            pack_hash: last_seen_trailer.ok_or(Error::IteratorInvariantTrailer)?,
            num_objects,
        })
    }
}