/*
 * Derived from cozo 0.7.6 `src/storage/sled.rs`,
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 *
 * Modifications for Knobyte:
 * - `del` records a real deletion marker. Upstream writes `PUT_MARKER` with an empty value,
 *   so every `:rm`, `::remove` and `::hnsw drop` left empty-valued keys behind, which broke
 *   the relation catalog ("Cannot deserialize relation") and resurrected removed rows.
 * - On open, empty-valued keys left by that bug are removed (cozo never stores empty values).
 * - A transaction's change set is an in-memory ordered map instead of a temporary Sled
 *   database: every read and write of a transaction (HNSW maintenance does many per row) went
 *   through a second on-disk Sled tree, which dominated graph synchronisation time.
 */

//! Persistent pure-Rust Sled storage engine for CozoDB (fixed deletions).

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::iter::Fuse;
use std::path::Path;

use cozo::{decode_tuple_from_kv, DataValue, Storage, StoreTx, ValidityTs};
use miette::{miette, IntoDiagnostic, Result};
use sled::{Batch, Db, IVec, Iter};

type Tuple = Vec<DataValue>;

/// Sled tree (outside cozo's keyspace) holding Knobyte's storage-maintenance markers.
const MAINTENANCE_TREE: &str = "knobyte_storage";
const SCRUBBED_MARKER: &str = "empty_tombstones_scrubbed_v1";

/// Open a Cozo database on Sled with the fixed storage engine.
pub fn new_cozo_sled_fixed(path: impl AsRef<Path>) -> Result<cozo::Db<FixedSledStorage>> {
    let db = sled::open(path).into_diagnostic()?;
    scrub_empty_tombstones(&db)?;
    let ret = cozo::Db::new(FixedSledStorage { db })?;
    ret.initialize()?;
    Ok(ret)
}

/// Remove empty-valued keys written by the upstream deletion bug (once per database).
fn scrub_empty_tombstones(db: &Db) -> Result<()> {
    let tree = db.open_tree(MAINTENANCE_TREE).into_diagnostic()?;
    if tree.contains_key(SCRUBBED_MARKER).into_diagnostic()? {
        return Ok(());
    }
    let mut batch = Batch::default();
    for pair in db.iter() {
        let (k, v) = pair.into_diagnostic()?;
        if v.is_empty() {
            batch.remove(k);
        }
    }
    db.apply_batch(batch).into_diagnostic()?;
    tree.insert(SCRUBBED_MARKER, &[1u8]).into_diagnostic()?;
    db.flush().into_diagnostic()?;
    Ok(())
}

/// Storage engine using Sled.
#[derive(Clone)]
pub struct FixedSledStorage {
    db: Db,
}

impl Storage<'_> for FixedSledStorage {
    type Tx = SledTx;

    fn storage_kind(&self) -> &'static str {
        "sled"
    }

    fn transact(&self, _write: bool) -> Result<Self::Tx> {
        Ok(SledTx {
            db: self.db.clone(),
            changes: BTreeMap::new(),
        })
    }

    fn range_compact(&self, _lower: &[u8], _upper: &[u8]) -> Result<()> {
        Ok(())
    }

    fn batch_put<'a>(
        &'a self,
        data: Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>,
    ) -> Result<()> {
        let mut tx = self.transact(true)?;
        for result in data {
            let (key, val) = result?;
            tx.put(&key, &val)?;
        }
        tx.commit()?;
        Ok(())
    }
}

/// A transaction: writes are buffered in memory (`None` = deletion) and applied to Sled as one
/// atomic batch on commit; reads see the buffered changes over the persisted data.
pub struct SledTx {
    db: Db,
    changes: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
}

impl<'s> StoreTx<'s> for SledTx {
    fn get(&self, key: &[u8], _for_update: bool) -> Result<Option<Vec<u8>>> {
        if let Some(val) = self.changes.get(key) {
            return Ok(val.clone());
        }
        let ret = self.db.get(key).into_diagnostic()?;
        Ok(ret.map(|v| v.to_vec()))
    }

    fn put(&mut self, key: &[u8], val: &[u8]) -> Result<()> {
        self.changes.insert(key.to_vec(), Some(val.to_vec()));
        Ok(())
    }

    fn supports_par_put(&self) -> bool {
        false
    }

    fn del(&mut self, key: &[u8]) -> Result<()> {
        // Upstream wrote a put marker here, turning deletions into empty values.
        self.changes.insert(key.to_vec(), None);
        Ok(())
    }

    fn del_range_from_persisted(&mut self, lower: &[u8], upper: &[u8]) -> Result<()> {
        let mut to_del = Vec::new();
        for pair in self.range_scan(lower, upper) {
            let (k, _) = pair?;
            to_del.push(k);
        }
        for k in to_del {
            self.db.remove(&k).into_diagnostic()?;
        }
        Ok(())
    }

    fn exists(&self, key: &[u8], _for_update: bool) -> Result<bool> {
        if let Some(val) = self.changes.get(key) {
            return Ok(val.is_some());
        }
        let ret = self.db.get(key).into_diagnostic()?;
        Ok(ret.is_some())
    }

    fn commit(&mut self) -> Result<()> {
        if self.changes.is_empty() {
            return Ok(());
        }
        let mut batch = Batch::default();
        for (k, v) in std::mem::take(&mut self.changes) {
            match v {
                Some(v) => batch.insert(k, v),
                None => batch.remove(k),
            }
        }
        self.db.apply_batch(batch).into_diagnostic()?;
        Ok(())
    }

    fn range_scan_tuple<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a>
    where
        's: 'a,
    {
        Box::new(
            self.range_scan(lower, upper)
                .map(|r| r.map(|(k, v)| decode_tuple_from_kv(&k, &v, None))),
        )
    }

    fn range_skip_scan_tuple<'a>(
        &'a self,
        _lower: &[u8],
        _upper: &[u8],
        _valid_at: ValidityTs,
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a> {
        Box::new(std::iter::once(Err(miette!(
            "Sled backend does not support time travelling."
        ))))
    }

    fn range_scan<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
    ) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>
    where
        's: 'a,
    {
        if lower >= upper {
            return Box::new(std::iter::empty());
        }
        if !self.changes.is_empty() {
            Box::new(MergedIter {
                change_iter: self.changes.range(lower.to_vec()..upper.to_vec()),
                db_iter: self.db.range(lower.to_vec()..upper.to_vec()).fuse(),
                change_cache: None,
                db_cache: None,
            })
        } else {
            Box::new(
                self.db
                    .range(lower.to_vec()..upper.to_vec())
                    .map(|d| d.into_diagnostic())
                    .map(|r| r.map(|(k, v)| (k.to_vec(), v.to_vec()))),
            )
        }
    }

    fn range_count<'a>(&'a self, lower: &[u8], upper: &[u8]) -> Result<usize>
    where
        's: 'a,
    {
        Ok(self.range_scan(lower, upper).count())
    }

    fn total_scan<'a>(&'a self) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>
    where
        's: 'a,
    {
        self.range_scan(&[], &[u8::MAX])
    }
}

/// Merges the transaction's change set over the persisted data (changes win; deletions hide
/// persisted keys).
struct MergedIter<'a> {
    change_iter: std::collections::btree_map::Range<'a, Vec<u8>, Option<Vec<u8>>>,
    db_iter: Fuse<Iter>,
    change_cache: Option<(&'a Vec<u8>, &'a Option<Vec<u8>>)>,
    db_cache: Option<(IVec, IVec)>,
}

impl MergedIter<'_> {
    fn fill_cache(&mut self) -> Result<()> {
        if self.change_cache.is_none() {
            self.change_cache = self.change_iter.next();
        }
        if self.db_cache.is_none() {
            if let Some(res) = self.db_iter.next() {
                self.db_cache = Some(res.into_diagnostic()?);
            }
        }
        Ok(())
    }

    fn next_inner(&mut self) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        loop {
            self.fill_cache()?;
            let take_change = match (&self.change_cache, &self.db_cache) {
                (None, None) => return Ok(None),
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (Some((ck, _)), Some((dk, _))) => match ck.as_slice().cmp(dk.as_ref()) {
                    Ordering::Less => true,
                    Ordering::Greater => false,
                    Ordering::Equal => {
                        // The change overrides the persisted value.
                        self.db_cache.take();
                        continue;
                    }
                },
            };
            if take_change {
                if let Some((k, cv)) = self.change_cache.take() {
                    match cv {
                        Some(v) => return Ok(Some((k.clone(), v.clone()))),
                        None => continue,
                    }
                }
            } else if let Some((k, v)) = self.db_cache.take() {
                return Ok(Some((k.to_vec(), v.to_vec())));
            }
        }
    }
}

impl Iterator for MergedIter<'_> {
    type Item = Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_inner().transpose()
    }
}
