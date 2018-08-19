// Copyright 2018 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use std::cmp::Ordering;

use kvproto::kvrpcpb::IsolationLevel;

use storage::engine::SEEK_BOUND;
use storage::mvcc::write::{Write, WriteType};
use storage::mvcc::Result;
use storage::{Cursor, CursorBuilder, Key, ScanMode, Snapshot, Statistics, Value};
use storage::{CF_DEFAULT, CF_LOCK, CF_WRITE};

use super::util::CheckLockResult;

// When there are many versions for the user key, after several tries,
// we will use seek to locate the right position. But this will turn around
// the write cf's iterator's direction inside RocksDB, and the next user key
// need to turn back the direction to backward. As we have tested, turn around
// iterator's direction from forward to backward is as expensive as seek in
// RocksDB, so don't set REVERSE_SEEK_BOUND too small.
const REVERSE_SEEK_BOUND: u64 = 16;

/// `BackwardScanner` factory.
pub struct BackwardScannerBuilder<S: Snapshot> {
    snapshot: S,
    fill_cache: bool,
    omit_value: bool,
    isolation_level: IsolationLevel,
    lower_bound: Option<Key>,
    upper_bound: Option<Key>,
    ts: u64,
}

impl<S: Snapshot> BackwardScannerBuilder<S> {
    /// Initialize a new `BackwardScanner`
    pub fn new(snapshot: S, ts: u64) -> Self {
        Self {
            snapshot,
            fill_cache: true,
            omit_value: false,
            isolation_level: IsolationLevel::SI,
            lower_bound: None,
            upper_bound: None,
            ts,
        }
    }

    /// Set whether or not read operations should fill the cache.
    ///
    /// Defaults to `true`.
    #[inline]
    pub fn fill_cache(mut self, fill_cache: bool) -> Self {
        self.fill_cache = fill_cache;
        self
    }

    /// Set whether values of the user key should be omitted. When `omit_value` is `true`, the
    /// length of returned value will be 0.
    ///
    /// Previously this option is called `key_only`.
    ///
    /// Defaults to `false`.
    #[inline]
    pub fn omit_value(mut self, omit_value: bool) -> Self {
        self.omit_value = omit_value;
        self
    }

    /// Set the isolation level.
    ///
    /// Defaults to `IsolationLevel::SI`.
    #[inline]
    pub fn isolation_level(mut self, isolation_level: IsolationLevel) -> Self {
        self.isolation_level = isolation_level;
        self
    }

    /// Limit the range to `[lower_bound, upper_bound)` in which the `BackwardScanner` should scan.
    /// `None` means unbounded.
    ///
    /// Default is `(None, None)`.
    #[inline]
    pub fn range(mut self, lower_bound: Option<Key>, upper_bound: Option<Key>) -> Self {
        self.lower_bound = lower_bound;
        self.upper_bound = upper_bound;
        self
    }

    /// Build `BackwardScanner` from the current configuration.
    pub fn build(self) -> Result<BackwardScanner<S>> {
        let lock_cursor = CursorBuilder::new(&self.snapshot, CF_LOCK)
            .range(self.lower_bound.clone(), self.upper_bound.clone())
            .fill_cache(self.fill_cache)
            .scan_mode(ScanMode::Backward)
            .build()?;

        let write_cursor = CursorBuilder::new(&self.snapshot, CF_WRITE)
            .range(self.lower_bound.clone(), self.upper_bound.clone())
            .fill_cache(self.fill_cache)
            .scan_mode(ScanMode::Backward)
            .build()?;

        Ok(BackwardScanner {
            snapshot: self.snapshot,
            fill_cache: self.fill_cache,
            omit_value: self.omit_value,
            isolation_level: self.isolation_level,
            lower_bound: self.lower_bound,
            upper_bound: self.upper_bound,
            ts: self.ts,
            lock_cursor,
            write_cursor,
            default_cursor: None,
            is_started: false,
            statistics: Statistics::default(),
        })
    }
}

/// This struct can be used to scan keys starting from the given user key in the reverse order
/// (less than).
///
/// Internally, for each key, rollbacks are ignored and smaller version will be tried. If the
/// isolation level is SI, locks will be checked first.
///
/// Use `BackwardScannerBuilder` to build `BackwardScanner`.
pub struct BackwardScanner<S: Snapshot> {
    snapshot: S,
    fill_cache: bool,
    omit_value: bool,
    isolation_level: IsolationLevel,

    /// `lower_bound` and `upper_bound` is used to create `default_cursor`. `upper_bound`
    /// is used in initial seek as well. They will be consumed after `default_cursor` is being
    /// created.
    lower_bound: Option<Key>,
    upper_bound: Option<Key>,

    ts: u64,

    lock_cursor: Cursor<S::Iter>,
    write_cursor: Cursor<S::Iter>,

    /// `default cursor` is lazy created only when it's needed.
    default_cursor: Option<Cursor<S::Iter>>,

    /// Is iteration started
    is_started: bool,

    statistics: Statistics,
}

impl<S: Snapshot> BackwardScanner<S> {
    /// Take out and reset the statistics collected so far.
    pub fn take_statistics(&mut self) -> Statistics {
        ::std::mem::replace(&mut self.statistics, Statistics::default())
    }

    /// Get the next key-value pair, in backward order.
    pub fn read_next(&mut self) -> Result<Option<(Key, Value)>> {
        if !self.is_started {
            if self.upper_bound.is_some() {
                // TODO: `seek_to_last` is better, however it has performance issues currently.
                // TODO: write_cursor only needs "seek_for_prev" because the given key should never
                // exist. However we don't have tests to cover now.
                self.write_cursor.reverse_seek(
                    self.upper_bound.as_ref().unwrap(),
                    &mut self.statistics.write,
                )?;
                self.lock_cursor.reverse_seek(
                    self.upper_bound.as_ref().unwrap(),
                    &mut self.statistics.lock,
                )?;
            } else {
                self.write_cursor.seek_to_last(&mut self.statistics.write);
                self.lock_cursor.seek_to_last(&mut self.statistics.lock);
            }
            self.is_started = true;
        }

        // Similar to forward scanner, the general idea is to simultaneously step write
        // cursor and lock cursor. Please refer to `ForwardScanner` for details.

        loop {
            let (current_user_key, has_write, has_lock) = {
                let w_key = if self.write_cursor.valid() {
                    Some(self.write_cursor.key(&mut self.statistics.write))
                } else {
                    None
                };
                let l_key = if self.lock_cursor.valid() {
                    Some(self.lock_cursor.key(&mut self.statistics.lock))
                } else {
                    None
                };

                // `res` is `(current_user_key_slice, has_write, has_lock)`
                let res = match (w_key, l_key) {
                    (None, None) => return Ok(None),
                    (None, Some(lk)) => (lk, false, true),
                    (Some(wk), None) => (Key::truncate_ts_for(wk)?, true, false),
                    (Some(wk), Some(lk)) => {
                        let write_user_key = Key::truncate_ts_for(wk)?;
                        match write_user_key.cmp(lk) {
                            Ordering::Less => {
                                // We are scanning from largest user key to smallest user key, so this
                                // indicate that we meet a lock first, thus its corresponding write
                                // does not exist.
                                (lk, false, true)
                            }
                            Ordering::Greater => {
                                // We meet write first, so the lock of the write key does not exist.
                                (write_user_key, true, false)
                            }
                            Ordering::Equal => (write_user_key, true, true),
                        }
                    }
                };

                // Use `from_encoded_slice` to reserve space for ts, so later we can append ts to
                // the key or its clones without reallocation.
                (Key::from_encoded_slice(res.0), res.1, res.2)
            };

            let mut result = Ok(None);
            let mut get_ts = self.ts;
            let mut met_prev_user_key = false;

            if has_lock {
                match self.isolation_level {
                    IsolationLevel::SI => match super::util::load_and_check_lock_from_cursor(
                        &mut self.lock_cursor,
                        &current_user_key,
                        self.ts,
                        &mut self.statistics,
                    )? {
                        CheckLockResult::NotLocked => {}
                        CheckLockResult::Locked(e) => result = Err(e),
                        CheckLockResult::Ignored(ts) => get_ts = ts,
                    },
                    IsolationLevel::RC => {}
                }
                self.lock_cursor.prev(&mut self.statistics.lock);
            }
            if has_write {
                if result.is_ok() {
                    result = self.reverse_get(&current_user_key, get_ts, &mut met_prev_user_key);
                }
                if !met_prev_user_key {
                    // Skip rest later versions and point to previous user key.
                    self.move_write_cursor_to_prev_user_key(&current_user_key)?;
                }
            }

            if let Some(v) = result? {
                return Ok(Some((current_user_key, v)));
            }
        }
    }

    /// Attempt to get the value of a key specified by `user_key` and `self.ts` in reverse order.
    /// This function requires that the write cursor is currently pointing to the earliest version
    /// of `user_key`.
    #[inline]
    fn reverse_get(
        &mut self,
        user_key: &Key,
        ts: u64,
        met_prev_user_key: &mut bool,
    ) -> Result<Option<Value>> {
        assert!(self.write_cursor.valid());

        // At first, we try to use several `prev()` to get the desired version.

        // We need to save last desired version, because when we may move to an unwanted version
        // at any time.
        let mut last_version = None;
        let mut last_checked_commit_ts = 0;

        for i in 0..REVERSE_SEEK_BOUND {
            if i > 0 {
                // We are already pointing at the smallest version, so we don't need to prev()
                // for the first iteration. So we will totally call `prev()` function
                // `REVERSE_SEEK_BOUND - 1` times.
                self.write_cursor.prev(&mut self.statistics.write);
                if !self.write_cursor.valid() {
                    // Key space ended. We use `last_version` as the return.
                    return Ok(self.handle_last_version(last_version, user_key)?);
                }
            }

            let mut is_done = false;
            {
                let current_key = self.write_cursor.key(&mut self.statistics.write);
                last_checked_commit_ts = Key::decode_ts_from(current_key)?;

                if !Key::is_user_key_eq(current_key, user_key.encoded().as_slice()) {
                    // Meet another key, use `last_version` as the return.
                    *met_prev_user_key = true;
                    is_done = true;
                } else if last_checked_commit_ts > ts {
                    // Meet an unwanted version, use `last_version` as the return as well.
                    is_done = true;
                }
            }
            if is_done {
                return Ok(self.handle_last_version(last_version, user_key)?);
            }

            let write = Write::parse(self.write_cursor.value(&mut self.statistics.write))?;
            self.statistics.write.processed += 1;

            match write.write_type {
                WriteType::Put | WriteType::Delete => last_version = Some(write),
                WriteType::Lock | WriteType::Rollback => {}
            }
        }

        // At this time, we must have current commit_ts <= ts. If commit_ts == ts,
        // we don't need to seek any more and we can just utilize `last_version`.
        if last_checked_commit_ts == ts {
            return Ok(self.handle_last_version(last_version, user_key)?);
        }
        assert!(ts > last_checked_commit_ts);

        // After several `prev()`, we still not get the latest version for the specified ts,
        // use seek to locate the latest version.
        // `user_key` must have reserved space here, so its clone has reserved space too. So no
        // reallocation happends in `append_ts`.
        let seek_key = user_key.clone().append_ts(ts);

        // TODO: Replace by cast + seek().
        self.write_cursor
            .internal_seek(&seek_key, &mut self.statistics.write)?;
        assert!(self.write_cursor.valid());

        loop {
            // After seek, or after some `next()`, we may reach `last_checked_commit_ts` again. It
            // means we have checked all versions for this user key. We use `last_version` as
            // return.
            let current_ts = {
                let current_key = self.write_cursor.key(&mut self.statistics.write);
                // We should never reach another user key.
                assert!(Key::is_user_key_eq(
                    current_key,
                    user_key.encoded().as_slice()
                ));
                Key::decode_ts_from(current_key)?
            };
            if current_ts <= last_checked_commit_ts {
                // We reach the last handled key
                return Ok(self.handle_last_version(last_version, user_key)?);
            }

            let write = Write::parse(self.write_cursor.value(&mut self.statistics.write))?;
            self.statistics.write.processed += 1;

            match write.write_type {
                WriteType::Put => {
                    return Ok(Some(self.reverse_load_data_by_write(write, user_key)?))
                }
                WriteType::Delete => return Ok(None),
                WriteType::Lock | WriteType::Rollback => {
                    // Continue iterate next `write`.
                    self.write_cursor.next(&mut self.statistics.write);
                    assert!(self.write_cursor.valid());
                }
            }
        }
    }

    /// Handle last version. Last version may be PUT or DELETE. If it is a PUT, value should be
    /// load.
    #[inline]
    fn handle_last_version(
        &mut self,
        some_write: Option<Write>,
        user_key: &Key,
    ) -> Result<Option<Value>> {
        match some_write {
            None => Ok(None),
            Some(write) => match write.write_type {
                WriteType::Put => Ok(Some(self.reverse_load_data_by_write(write, user_key)?)),
                WriteType::Delete => Ok(None),
                _ => unreachable!(),
            },
        }
    }

    /// Load the value by the given `some_write`. If value is carried in `some_write`, it will be
    /// returned directly. Otherwise there will be a default CF look up.
    ///
    /// The implementation is similar to `PointGetter::load_data_by_write`.
    #[inline]
    fn reverse_load_data_by_write(&mut self, write: Write, user_key: &Key) -> Result<Value> {
        if self.omit_value {
            return Ok(vec![]);
        }
        match write.short_value {
            Some(value) => {
                // Value is carried in `write`.
                Ok(value)
            }
            None => {
                // Value is in the default CF.
                self.ensure_default_cursor()?;
                let value = super::util::near_reverse_load_data_by_write(
                    &mut self.default_cursor.as_mut().unwrap(),
                    user_key,
                    write,
                    &mut self.statistics,
                )?;
                Ok(value)
            }
        }
    }

    /// After `self.reverse_get()`, our write cursor may be pointing to current user key (if we
    /// found a desired version), or previous user key (if there is no desired version), or
    /// out of bound.
    ///
    /// If it is pointing to current user key, we need to step it until we meet a new
    /// key. We first try to `prev()` a few times. If still not reaching another user
    /// key, we `seek_for_prev()`.
    #[inline]
    fn move_write_cursor_to_prev_user_key(&mut self, current_user_key: &Key) -> Result<()> {
        for i in 0..SEEK_BOUND {
            if i > 0 {
                self.write_cursor.prev(&mut self.statistics.write);
            }
            if !self.write_cursor.valid() {
                // Key space ended. We are done here.
                return Ok(());
            }
            {
                let current_key = self.write_cursor.key(&mut self.statistics.write);
                if !Key::is_user_key_eq(current_key, current_user_key.encoded().as_slice()) {
                    // Found another user key. We are done here.
                    return Ok(());
                }
            }
        }

        // We have not found another user key for now, so we directly `seek_for_prev()`.
        // After that, we must pointing to another key, or out of bound.
        self.write_cursor
            .internal_seek_for_prev(current_user_key, &mut self.statistics.write)?;

        Ok(())
    }

    /// Create the default cursor if it doesn't exist.
    fn ensure_default_cursor(&mut self) -> Result<()> {
        if self.default_cursor.is_some() {
            return Ok(());
        }
        let cursor = CursorBuilder::new(&self.snapshot, CF_DEFAULT)
            .range(self.lower_bound.take(), self.upper_bound.take())
            .fill_cache(self.fill_cache)
            .scan_mode(ScanMode::Backward)
            .build()?;
        self.default_cursor = Some(cursor);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use storage::engine::{self, TEMP_DIR};
    use storage::mvcc::tests::*;
    use storage::ALL_CFS;
    use storage::{Engine, Key};

    use kvproto::kvrpcpb::Context;

    #[test]
    fn test_basic() {
        let engine = engine::new_local_engine(TEMP_DIR, ALL_CFS).unwrap();

        // Generate REVERSE_SEEK_BOUND / 2 Put for key [10].
        let k = &[10 as u8];
        for ts in 0..REVERSE_SEEK_BOUND / 2 {
            must_prewrite_put(&engine, k, &[ts as u8], k, ts);
            must_commit(&engine, k, ts, ts);
        }

        // Generate REVERSE_SEEK_BOUND + 1 Put for key [9].
        let k = &[9 as u8];
        for ts in 0..REVERSE_SEEK_BOUND + 1 {
            must_prewrite_put(&engine, k, &[ts as u8], k, ts);
            must_commit(&engine, k, ts, ts);
        }

        // Generate REVERSE_SEEK_BOUND / 2 Put and REVERSE_SEEK_BOUND / 2 + 1 Rollback for key [8].
        let k = &[8 as u8];
        for ts in 0..REVERSE_SEEK_BOUND + 1 {
            must_prewrite_put(&engine, k, &[ts as u8], k, ts);
            if ts < REVERSE_SEEK_BOUND / 2 {
                must_commit(&engine, k, ts, ts);
            } else {
                must_rollback(&engine, k, ts);
            }
        }

        // Generate REVERSE_SEEK_BOUND / 2 Put, 1 Delete and REVERSE_SEEK_BOUND / 2 Rollback
        // for key [7].
        let k = &[7 as u8];
        for ts in 0..REVERSE_SEEK_BOUND / 2 {
            must_prewrite_put(&engine, k, &[ts as u8], k, ts);
            must_commit(&engine, k, ts, ts);
        }
        {
            let ts = REVERSE_SEEK_BOUND / 2;
            must_prewrite_delete(&engine, k, k, ts);
            must_commit(&engine, k, ts, ts);
        }
        for ts in REVERSE_SEEK_BOUND / 2 + 1..REVERSE_SEEK_BOUND + 1 {
            must_prewrite_put(&engine, k, &[ts as u8], k, ts);
            must_rollback(&engine, k, ts);
        }

        // Generate 1 PUT for key [6].
        let k = &[6 as u8];
        for ts in 0..1 {
            must_prewrite_put(&engine, k, &[ts as u8], k, ts);
            must_commit(&engine, k, ts, ts);
        }

        // Generate REVERSE_SEEK_BOUND + 1 Rollback for key [5].
        let k = &[5 as u8];
        for ts in 0..REVERSE_SEEK_BOUND + 1 {
            must_prewrite_put(&engine, k, &[ts as u8], k, ts);
            must_rollback(&engine, k, ts);
        }

        // Generate 1 PUT with ts = REVERSE_SEEK_BOUND and 1 PUT
        // with ts = REVERSE_SEEK_BOUND + 1 for key [4].
        let k = &[4 as u8];
        for ts in REVERSE_SEEK_BOUND..REVERSE_SEEK_BOUND + 2 {
            must_prewrite_put(&engine, k, &[ts as u8], k, ts);
            must_commit(&engine, k, ts, ts);
        }

        // Assume REVERSE_SEEK_BOUND == 4, we have keys:
        // 4 4 5 5 5 5 5 6 7 7 7 7 7 8 8 8 8 8 9 9 9 9 9 10 10

        let snapshot = engine.snapshot(&Context::new()).unwrap();
        let mut scanner = BackwardScannerBuilder::new(snapshot, REVERSE_SEEK_BOUND)
            .range(None, Some(Key::from_raw(&[11 as u8])))
            .build()
            .unwrap();

        // Initial position: 1 seek_for_prev
        // 4 4 5 5 5 5 5 6 7 7 7 7 7 8 8 8 8 8 9 9 9 9 9 10 10
        //                                                  ^cursor
        // When get key [10]: REVERSE_SEEK_BOUND / 2 prev
        // 4 4 5 5 5 5 5 6 7 7 7 7 7 8 8 8 8 8 9 9 9 9 9 10 10
        //                                             ^cursor
        //                                               ^last_version
        // After get key [10]: 0 prev
        // 4 4 5 5 5 5 5 6 7 7 7 7 7 8 8 8 8 8 9 9 9 9 9 10 10
        //                                             ^
        assert_eq!(
            scanner.read_next().unwrap(),
            Some((
                Key::from_raw(&[10 as u8]),
                vec![(REVERSE_SEEK_BOUND / 2 - 1) as u8]
            ))
        );
        let statistics = scanner.take_statistics();
        assert_eq!(statistics.write.prev, REVERSE_SEEK_BOUND as usize / 2);
        assert_eq!(statistics.write.seek, 0);
        assert_eq!(statistics.write.next, 0);
        assert_eq!(statistics.write.seek_for_prev, 1);

        // Before get key [9]:
        // 4 4 5 5 5 5 5 6 7 7 7 7 7 8 8 8 8 8 9 9 9 9 9 10 10
        //                                             ^cursor
        // When get key [9]:
        // First, REVERSE_SEEK_BOUND - 1 prev
        // 4 4 5 5 5 5 5 6 7 7 7 7 7 8 8 8 8 8 9 9 9 9 9 10 10
        //                                       ^cursor
        //                                       ^last_version
        //                                       ^last_handled_key
        // Bound reached, 1 extra seek.
        // After seek:
        // 4 4 5 5 5 5 5 6 7 7 7 7 7 8 8 8 8 8 9 9 9 9 9 10 10
        //                                     ^cursor
        //                                       ^last_handled_key
        // Now we got key[9].
        // After get key [9]: 1 prev
        // 4 4 5 5 5 5 5 6 7 7 7 7 7 8 8 8 8 8 9 9 9 9 9 10 10
        //                                   ^cursor
        assert_eq!(
            scanner.read_next().unwrap(),
            Some((Key::from_raw(&[9 as u8]), vec![REVERSE_SEEK_BOUND as u8]))
        );
        let statistics = scanner.take_statistics();
        assert_eq!(statistics.write.prev, REVERSE_SEEK_BOUND as usize);
        assert_eq!(statistics.write.seek, 1);
        assert_eq!(statistics.write.next, 0);
        assert_eq!(statistics.write.seek_for_prev, 0);

        // Before get key [8]:
        // 4 4 5 5 5 5 5 6 7 7 7 7 7 8 8 8 8 8 9 9 9 9 9 10 10
        //                                   ^cursor
        // When get key [8]:
        // First, REVERSE_SEEK_BOUND - 1 prev
        // 4 4 5 5 5 5 5 6 7 7 7 7 7 8 8 8 8 8 9 9 9 9 9 10 10
        //                             ^cursor
        //                                 ^last_version
        //                             ^last_handled_key
        // Bound reached, 1 extra seek.
        // After seek:
        // 4 4 5 5 5 5 5 6 7 7 7 7 7 8 8 8 8 8 9 9 9 9 9 10 10
        //                           ^cursor
        //                                 ^last_version
        //                             ^last_handled_key
        // Got ROLLBACK, so 1 next:
        // 4 4 5 5 5 5 5 6 7 7 7 7 7 8 8 8 8 8 9 9 9 9 9 10 10
        //                             ^cursor
        //                                 ^last_version
        //                             ^last_handled_key
        // Hit last_handled_key, so use last_version and we get key [8].
        // After get key [8]: 2 prev
        // 4 4 5 5 5 5 5 6 7 7 7 7 7 8 8 8 8 8 9 9 9 9 9 10 10
        //                         ^cursor
        assert_eq!(
            scanner.read_next().unwrap(),
            Some((
                Key::from_raw(&[8 as u8]),
                vec![(REVERSE_SEEK_BOUND / 2 - 1) as u8]
            ))
        );
        let statistics = scanner.take_statistics();
        assert_eq!(statistics.write.prev, REVERSE_SEEK_BOUND as usize + 1);
        assert_eq!(statistics.write.seek, 1);
        assert_eq!(statistics.write.next, 1);
        assert_eq!(statistics.write.seek_for_prev, 0);

        // Before get key [7]:
        // 4 4 5 5 5 5 5 6 7 7 7 7 7 8 8 8 8 8 9 9 9 9 9 10 10
        //                         ^cursor
        // First, REVERSE_SEEK_BOUND - 1 prev
        // 4 4 5 5 5 5 5 6 7 7 7 7 7 8 8 8 8 8 9 9 9 9 9 10 10
        //                   ^cursor
        // Bound reached, 1 extra seek.
        // After seek:
        // 4 4 5 5 5 5 5 6 7 7 7 7 7 8 8 8 8 8 9 9 9 9 9 10 10
        //                 ^cursor
        // Got ROLLBACK, so 1 next:
        // 4 4 5 5 5 5 5 6 7 7 7 7 7 8 8 8 8 8 9 9 9 9 9 10 10
        //                   ^cursor
        // Hit last_handled_key, so use last_version and we get None.
        // Skip this key's versions and go to next key: 2 prev
        // 4 4 5 5 5 5 5 6 7 7 7 7 7 8 8 8 8 8 9 9 9 9 9 10 10
        //               ^cursor
        // Current commit ts > ts is not satisfied, so 1 prev:
        // 4 4 5 5 5 5 5 6 7 7 7 7 7 8 8 8 8 8 9 9 9 9 9 10 10
        //             ^cursor
        //               ^last_version
        // We reached another key, use last_version and we get [6].
        // After get key [6]: 0 prev
        // 4 4 5 5 5 5 5 6 7 7 7 7 7 8 8 8 8 8 9 9 9 9 9 10 10
        //             ^cursor
        assert_eq!(
            scanner.read_next().unwrap(),
            Some((Key::from_raw(&[6 as u8]), vec![0 as u8]))
        );
        let statistics = scanner.take_statistics();
        assert_eq!(statistics.write.prev, REVERSE_SEEK_BOUND as usize + 2);
        assert_eq!(statistics.write.seek, 1);
        assert_eq!(statistics.write.next, 1);
        assert_eq!(statistics.write.seek_for_prev, 0);

        // Before get key [5]:
        // 4 4 5 5 5 5 5 6 7 7 7 7 7 8 8 8 8 8 9 9 9 9 9 10 10
        //             ^cursor
        // First, REVERSE_SEEK_BOUND - 1 prev
        // 4 4 5 5 5 5 5 6 7 7 7 7 7 8 8 8 8 8 9 9 9 9 9 10 10
        //       ^cursor (last_version == None)
        // Bound reached, 1 extra seek.
        // After seek:
        // 4 4 5 5 5 5 5 6 7 7 7 7 7 8 8 8 8 8 9 9 9 9 9 10 10
        //     ^cursor (last_version == None)
        // Got ROLLBACK, so 1 next:
        // 4 4 5 5 5 5 5 6 7 7 7 7 7 8 8 8 8 8 9 9 9 9 9 10 10
        //       ^cursor (last_version == None)
        // Hit last_handled_key, so use last_version and we get None.
        // Skip this key's versions and go to next key: 2 prev
        // 4 4 5 5 5 5 5 6 7 7 7 7 7 8 8 8 8 8 9 9 9 9 9 10 10
        //   ^cursor
        // Current commit ts > ts is not satisfied, so 1 prev:
        // 4 4 5 5 5 5 5 6 7 7 7 7 7 8 8 8 8 8 9 9 9 9 9 10 10
        // ^cursor
        //   ^last_version
        // Current commit ts > ts is satisfied, use last_version and we get [4].
        // After get key [4]: 1 prev
        //   4 4 5 5 5 5 5 6 7 7 7 7 7 8 8 8 8 8 9 9 9 9 9 10 10
        // ^cursor
        assert_eq!(
            scanner.read_next().unwrap(),
            Some((Key::from_raw(&[4 as u8]), vec![REVERSE_SEEK_BOUND as u8]))
        );
        let statistics = scanner.take_statistics();
        assert_eq!(statistics.write.prev, REVERSE_SEEK_BOUND as usize + 3);
        assert_eq!(statistics.write.seek, 1);
        assert_eq!(statistics.write.next, 1);
        assert_eq!(statistics.write.seek_for_prev, 0);

        // Scan end.
        assert_eq!(scanner.read_next().unwrap(), None);
        let statistics = scanner.take_statistics();
        assert_eq!(statistics.write.prev, 0);
        assert_eq!(statistics.write.seek, 0);
        assert_eq!(statistics.write.next, 0);
        assert_eq!(statistics.write.seek_for_prev, 0);
    }

    /// Check whether everything works as usual when `BackwardScanner::reverse_get()` goes
    /// out of bound.
    ///
    /// Case 1. prev out of bound, next_version is None.
    #[test]
    fn test_reverse_get_out_of_bound_1() {
        let engine = engine::new_local_engine(TEMP_DIR, ALL_CFS).unwrap();

        // Generate N/2 rollback for [b].
        for ts in 0..REVERSE_SEEK_BOUND / 2 {
            must_rollback(&engine, b"b", ts);
        }

        // Generate 1 put for [c].
        must_prewrite_put(&engine, b"c", b"value", b"c", REVERSE_SEEK_BOUND * 2);
        must_commit(
            &engine,
            b"c",
            REVERSE_SEEK_BOUND * 2,
            REVERSE_SEEK_BOUND * 2,
        );

        let snapshot = engine.snapshot(&Context::new()).unwrap();
        let mut scanner = BackwardScannerBuilder::new(snapshot, REVERSE_SEEK_BOUND * 2)
            .range(None, None)
            .build()
            .unwrap();

        // The following illustration comments assume that REVERSE_SEEK_BOUND = 4.

        // Initial position: 1 seek_to_last:
        //   b_1 b_0 c_8
        //           ^cursor
        // Got ts == specified ts, advance the cursor.
        //   b_1 b_0 c_8
        //       ^cursor
        // Now we get [c]. Trying to reach prev user key: no operation needed.
        //   b_1 b_0 c_8
        //       ^cursor
        assert_eq!(
            scanner.read_next().unwrap(),
            Some((Key::from_raw(b"c"), b"value".to_vec())),
        );
        let statistics = scanner.take_statistics();
        assert_eq!(statistics.write.seek, 1);
        assert_eq!(statistics.write.seek_for_prev, 0);
        assert_eq!(statistics.write.next, 0);
        assert_eq!(statistics.write.prev, 1);

        // Use N/2 prev and reach out of bound:
        //   b_1 b_0 c_8
        // ^cursor
        assert_eq!(scanner.read_next().unwrap(), None);
        let statistics = scanner.take_statistics();
        assert_eq!(statistics.write.seek, 0);
        assert_eq!(statistics.write.seek_for_prev, 0);
        assert_eq!(statistics.write.next, 0);
        assert_eq!(statistics.write.prev, (REVERSE_SEEK_BOUND / 2) as usize);

        // Cursor remains invalid, so nothing should happen.
        assert_eq!(scanner.read_next().unwrap(), None);
        let statistics = scanner.take_statistics();
        assert_eq!(statistics.write.seek, 0);
        assert_eq!(statistics.write.seek_for_prev, 0);
        assert_eq!(statistics.write.next, 0);
        assert_eq!(statistics.write.prev, 0);
    }

    /// Check whether everything works as usual when `BackwardScanner::reverse_get()` goes
    /// out of bound.
    ///
    /// Case 2. prev out of bound, next_version is Some.
    #[test]
    fn test_reverse_get_out_of_bound_2() {
        let engine = engine::new_local_engine(TEMP_DIR, ALL_CFS).unwrap();

        // Generate 1 put and N/2 rollback for [b].
        must_prewrite_put(&engine, b"b", b"value_b", b"b", 0);
        must_commit(&engine, b"b", 0, 0);
        for ts in 1..REVERSE_SEEK_BOUND / 2 + 1 {
            must_rollback(&engine, b"b", ts);
        }

        // Generate 1 put for [c].
        must_prewrite_put(&engine, b"c", b"value_c", b"c", REVERSE_SEEK_BOUND * 2);
        must_commit(
            &engine,
            b"c",
            REVERSE_SEEK_BOUND * 2,
            REVERSE_SEEK_BOUND * 2,
        );

        let snapshot = engine.snapshot(&Context::new()).unwrap();
        let mut scanner = BackwardScannerBuilder::new(snapshot, REVERSE_SEEK_BOUND * 2)
            .range(None, None)
            .build()
            .unwrap();

        // The following illustration comments assume that REVERSE_SEEK_BOUND = 4.

        // Initial position: 1 seek_to_last:
        //   b_2 b_1 b_0 c_8
        //               ^cursor
        // Got ts == specified ts, advance the cursor.
        //   b_2 b_1 b_0 c_8
        //           ^cursor
        // Now we get [c]. Trying to reach prev user key: no operation needed.
        //   b_2 b_1 b_0 c_8
        //           ^cursor
        assert_eq!(
            scanner.read_next().unwrap(),
            Some((Key::from_raw(b"c"), b"value_c".to_vec())),
        );
        let statistics = scanner.take_statistics();
        assert_eq!(statistics.write.seek, 1);
        assert_eq!(statistics.write.seek_for_prev, 0);
        assert_eq!(statistics.write.next, 0);
        assert_eq!(statistics.write.prev, 1);

        // Use N/2+1 prev and reach out of bound:
        //   b_2 b_1 b_0 c_8
        // ^cursor
        assert_eq!(
            scanner.read_next().unwrap(),
            Some((Key::from_raw(b"b"), b"value_b".to_vec())),
        );
        let statistics = scanner.take_statistics();
        assert_eq!(statistics.write.seek, 0);
        assert_eq!(statistics.write.seek_for_prev, 0);
        assert_eq!(statistics.write.next, 0);
        assert_eq!(statistics.write.prev, (REVERSE_SEEK_BOUND / 2 + 1) as usize);

        // Cursor remains invalid, so nothing should happen.
        assert_eq!(scanner.read_next().unwrap(), None);
        let statistics = scanner.take_statistics();
        assert_eq!(statistics.write.seek, 0);
        assert_eq!(statistics.write.seek_for_prev, 0);
        assert_eq!(statistics.write.next, 0);
        assert_eq!(statistics.write.prev, 0);
    }

    /// Check whether everything works as usual when
    /// `BackwardScanner::move_write_cursor_to_prev_user_key()` goes out of bound.
    ///
    /// Case 1. prev() out of bound
    #[test]
    fn test_move_prev_user_key_out_of_bound_1() {
        let engine = engine::new_local_engine(TEMP_DIR, ALL_CFS).unwrap();

        // Generate 1 put for [c].
        must_prewrite_put(&engine, b"c", b"value", b"c", 1);
        must_commit(&engine, b"c", 1, 1);

        // Generate N/2 put for [b] .
        for ts in 1..SEEK_BOUND / 2 + 1 {
            must_prewrite_put(&engine, b"b", &[ts as u8], b"b", ts);
            must_commit(&engine, b"b", ts, ts);
        }

        let snapshot = engine.snapshot(&Context::new()).unwrap();
        let mut scanner = BackwardScannerBuilder::new(snapshot, 1)
            .range(None, None)
            .build()
            .unwrap();

        // The following illustration comments assume that SEEK_BOUND = 4.

        // Initial position: 1 seek_to_last:
        //   b_2 b_1 c_1
        //           ^cursor
        // Got ts == specified ts, advance the cursor.
        //   b_2 b_1 c_1
        //       ^cursor
        // Now we get [c]. Trying to reach prev user key: no operation needed.
        //   b_2 b_1 c_1
        //       ^cursor
        assert_eq!(
            scanner.read_next().unwrap(),
            Some((Key::from_raw(b"c"), b"value".to_vec())),
        );
        let statistics = scanner.take_statistics();
        assert_eq!(statistics.write.seek, 1);
        assert_eq!(statistics.write.seek_for_prev, 0);
        assert_eq!(statistics.write.next, 0);
        assert_eq!(statistics.write.prev, 1);

        // Before:
        //   b_2 b_1 c_1
        //       ^cursor
        // We should be able to get wanted value without any operation.
        // After get the value, use N/2 prev to reach prev key and stop:
        //   b_2 b_1 c_1
        // ^cursor
        assert_eq!(
            scanner.read_next().unwrap(),
            Some((Key::from_raw(b"b"), vec![1u8].to_vec())),
        );
        let statistics = scanner.take_statistics();
        assert_eq!(statistics.write.seek, 0);
        assert_eq!(statistics.write.seek_for_prev, 0);
        assert_eq!(statistics.write.next, 0);
        assert_eq!(statistics.write.prev, (SEEK_BOUND / 2) as usize);

        // Next we should get nothing.
        assert_eq!(scanner.read_next().unwrap(), None);
        let statistics = scanner.take_statistics();
        assert_eq!(statistics.write.seek, 0);
        assert_eq!(statistics.write.seek_for_prev, 0);
        assert_eq!(statistics.write.next, 0);
        assert_eq!(statistics.write.prev, 0);
    }

    /// Check whether everything works as usual when
    /// `BackwardScanner::move_write_cursor_to_prev_user_key()` goes out of bound.
    ///
    /// Case 2. seek_for_prev() out of bound
    #[test]
    fn test_move_prev_user_key_out_of_bound_2() {
        let engine = engine::new_local_engine(TEMP_DIR, ALL_CFS).unwrap();

        // Generate 1 put for [c].
        must_prewrite_put(&engine, b"c", b"value", b"c", 1);
        must_commit(&engine, b"c", 1, 1);

        // Generate N+1 put for [b] .
        for ts in 1..SEEK_BOUND + 2 {
            must_prewrite_put(&engine, b"b", &[ts as u8], b"b", ts);
            must_commit(&engine, b"b", ts, ts);
        }

        let snapshot = engine.snapshot(&Context::new()).unwrap();
        let mut scanner = BackwardScannerBuilder::new(snapshot, 1)
            .range(None, None)
            .build()
            .unwrap();

        // The following illustration comments assume that SEEK_BOUND = 4.

        // Initial position: 1 seek_to_last:
        //   b_5 b_4 b_3 b_2 b_1 c_1
        //                       ^cursor
        // Got ts == specified ts, advance the cursor.
        //   b_5 b_4 b_3 b_2 b_1 c_1
        //                   ^cursor
        // Now we get [c]. Trying to reach prev user key: no operation needed.
        //   b_5 b_4 b_3 b_2 b_1 c_1
        //                   ^cursor
        assert_eq!(
            scanner.read_next().unwrap(),
            Some((Key::from_raw(b"c"), b"value".to_vec())),
        );
        let statistics = scanner.take_statistics();
        assert_eq!(statistics.write.seek, 1);
        assert_eq!(statistics.write.seek_for_prev, 0);
        assert_eq!(statistics.write.next, 0);
        assert_eq!(statistics.write.prev, 1);

        // Before:
        //   b_5 b_4 b_3 b_2 b_1 c_1
        //                   ^cursor
        // Got ts == specified ts, advance the cursor.
        //   b_5 b_4 b_3 b_2 b_1 c_1
        //               ^cursor
        // Now we get [b].
        // After get the value, use N-1 prev: (TODO: Should be SEEK_BOUND)
        //   b_5 b_4 b_3 b_2 b_1 c_1
        //   ^cursor
        // Then, use seek_for_prev:
        //   b_5 b_4 b_3 b_2 b_1 c_1
        // ^cursor
        assert_eq!(
            scanner.read_next().unwrap(),
            Some((Key::from_raw(b"b"), vec![1u8])),
        );
        let statistics = scanner.take_statistics();
        assert_eq!(statistics.write.seek, 0);
        assert_eq!(statistics.write.seek_for_prev, 1);
        assert_eq!(statistics.write.next, 0);
        assert_eq!(statistics.write.prev, SEEK_BOUND as usize);

        // Next we should get nothing.
        assert_eq!(scanner.read_next().unwrap(), None);
        let statistics = scanner.take_statistics();
        assert_eq!(statistics.write.seek, 0);
        assert_eq!(statistics.write.seek_for_prev, 0);
        assert_eq!(statistics.write.next, 0);
        assert_eq!(statistics.write.prev, 0);
    }

    /// Check whether everything works as usual when
    /// `BackwardScanner::move_write_cursor_to_prev_user_key()` goes out of bound.
    ///
    /// Case 3. a more complicated case
    #[test]
    fn test_move_prev_user_key_out_of_bound_3() {
        let engine = engine::new_local_engine(TEMP_DIR, ALL_CFS).unwrap();

        // N denotes for SEEK_BOUND, M denotes for REVERSE_SEEK_BOUND

        // Generate 1 put for [c].
        must_prewrite_put(&engine, b"c", b"value", b"c", 1);
        must_commit(&engine, b"c", 1, 1);

        // Generate N+M+1 put for [b] .
        for ts in 1..SEEK_BOUND + REVERSE_SEEK_BOUND + 2 {
            must_prewrite_put(&engine, b"b", &[ts as u8], b"b", ts);
            must_commit(&engine, b"b", ts, ts);
        }

        let snapshot = engine.snapshot(&Context::new()).unwrap();
        let mut scanner = BackwardScannerBuilder::new(snapshot, REVERSE_SEEK_BOUND + 1)
            .range(None, None)
            .build()
            .unwrap();

        // The following illustration comments assume that SEEK_BOUND = 4, REVERSE_SEEK_BOUND = 6.

        // Initial position: 1 seek_to_last:
        //   b_11 b_10 b_9 b_8 b_7 b_6 b_5 b_4 b_3 b_2 b_1 c_1
        //                                                 ^cursor
        // Got ts < specified ts, advance the cursor.
        //   b_11 b_10 b_9 b_8 b_7 b_6 b_5 b_4 b_3 b_2 b_1 c_1
        //                                             ^cursor
        // Now we get [c]. Trying to reach prev user key: no operation needed.
        //   b_11 b_10 b_9 b_8 b_7 b_6 b_5 b_4 b_3 b_2 b_1 c_1
        //                                             ^cursor
        assert_eq!(
            scanner.read_next().unwrap(),
            Some((Key::from_raw(b"c"), b"value".to_vec())),
        );
        let statistics = scanner.take_statistics();
        assert_eq!(statistics.write.seek, 1);
        assert_eq!(statistics.write.seek_for_prev, 0);
        assert_eq!(statistics.write.next, 0);
        assert_eq!(statistics.write.prev, 1);

        // Before:
        //   b_11 b_10 b_9 b_8 b_7 b_6 b_5 b_4 b_3 b_2 b_1 c_1
        //                                             ^cursor
        // Use M-1 prev trying to get specified version:
        //   b_11 b_10 b_9 b_8 b_7 b_6 b_5 b_4 b_3 b_2 b_1 c_1
        //                         ^cursor
        // Possible more version, so use seek:
        //   b_11 b_10 b_9 b_8 b_7 b_6 b_5 b_4 b_3 b_2 b_1 c_1
        //                     ^cursor
        // Now we got wanted value.
        // After get the value, use N-1 prev trying to reach prev user key:
        //   b_11 b_10 b_9 b_8 b_7 b_6 b_5 b_4 b_3 b_2 b_1 c_1
        //        ^cursor
        // Then, use seek_for_prev:
        //   b_11 b_10 b_9 b_8 b_7 b_6 b_5 b_4 b_3 b_2 b_1 c_1
        // ^cursor
        assert_eq!(
            scanner.read_next().unwrap(),
            Some((Key::from_raw(b"b"), vec![(REVERSE_SEEK_BOUND + 1) as u8])),
        );
        let statistics = scanner.take_statistics();
        assert_eq!(statistics.write.seek, 1);
        assert_eq!(statistics.write.seek_for_prev, 1);
        assert_eq!(statistics.write.next, 0);
        assert_eq!(
            statistics.write.prev,
            (REVERSE_SEEK_BOUND - 1 + SEEK_BOUND - 1) as usize
        );

        // Next we should get nothing.
        assert_eq!(scanner.read_next().unwrap(), None);
        let statistics = scanner.take_statistics();
        assert_eq!(statistics.write.seek, 0);
        assert_eq!(statistics.write.seek_for_prev, 0);
        assert_eq!(statistics.write.next, 0);
        assert_eq!(statistics.write.prev, 0);
    }

    /// Range is left open right closed.
    #[test]
    fn test_range() {
        let engine = engine::new_local_engine(TEMP_DIR, ALL_CFS).unwrap();

        // Generate 1 put for [1], [2] ... [6].
        for i in 1..7 {
            // ts = 1: value = []
            must_prewrite_put(&engine, &[i], &[], &[i], 1);
            must_commit(&engine, &[i], 1, 1);

            // ts = 7: value = [ts]
            must_prewrite_put(&engine, &[i], &[i], &[i], 7);
            must_commit(&engine, &[i], 7, 7);

            // ts = 14: value = []
            must_prewrite_put(&engine, &[i], &[], &[i], 14);
            must_commit(&engine, &[i], 14, 14);
        }

        let snapshot = engine.snapshot(&Context::new()).unwrap();

        // Test both bound specified.
        let mut scanner = BackwardScannerBuilder::new(snapshot.clone(), 10)
            .range(Some(Key::from_raw(&[3u8])), Some(Key::from_raw(&[5u8])))
            .build()
            .unwrap();
        assert_eq!(
            scanner.read_next().unwrap(),
            Some((Key::from_raw(&[4u8]), vec![4u8]))
        );
        assert_eq!(
            scanner.read_next().unwrap(),
            Some((Key::from_raw(&[3u8]), vec![3u8]))
        );
        assert_eq!(scanner.read_next().unwrap(), None);

        // Test left bound not specified.
        let mut scanner = BackwardScannerBuilder::new(snapshot.clone(), 10)
            .range(None, Some(Key::from_raw(&[3u8])))
            .build()
            .unwrap();
        assert_eq!(
            scanner.read_next().unwrap(),
            Some((Key::from_raw(&[2u8]), vec![2u8]))
        );
        assert_eq!(
            scanner.read_next().unwrap(),
            Some((Key::from_raw(&[1u8]), vec![1u8]))
        );
        assert_eq!(scanner.read_next().unwrap(), None);

        // Test right bound not specified.
        let mut scanner = BackwardScannerBuilder::new(snapshot.clone(), 10)
            .range(Some(Key::from_raw(&[5u8])), None)
            .build()
            .unwrap();
        assert_eq!(
            scanner.read_next().unwrap(),
            Some((Key::from_raw(&[6u8]), vec![6u8]))
        );
        assert_eq!(
            scanner.read_next().unwrap(),
            Some((Key::from_raw(&[5u8]), vec![5u8]))
        );
        assert_eq!(scanner.read_next().unwrap(), None);

        // Test both bound not specified.
        let mut scanner = BackwardScannerBuilder::new(snapshot.clone(), 10)
            .range(None, None)
            .build()
            .unwrap();
        assert_eq!(
            scanner.read_next().unwrap(),
            Some((Key::from_raw(&[6u8]), vec![6u8]))
        );
        assert_eq!(
            scanner.read_next().unwrap(),
            Some((Key::from_raw(&[5u8]), vec![5u8]))
        );
        assert_eq!(
            scanner.read_next().unwrap(),
            Some((Key::from_raw(&[4u8]), vec![4u8]))
        );
        assert_eq!(
            scanner.read_next().unwrap(),
            Some((Key::from_raw(&[3u8]), vec![3u8]))
        );
        assert_eq!(
            scanner.read_next().unwrap(),
            Some((Key::from_raw(&[2u8]), vec![2u8]))
        );
        assert_eq!(
            scanner.read_next().unwrap(),
            Some((Key::from_raw(&[1u8]), vec![1u8]))
        );
        assert_eq!(scanner.read_next().unwrap(), None);
    }

    #[test]
    fn test_many_tombstones() {
        let engine = engine::new_local_engine(TEMP_DIR, ALL_CFS).unwrap();

        // Generate RocksDB tombstones in write cf.
        let start_ts = 1;
        let safe_point = 2;
        for i in 0..256 {
            for y in 0..256 {
                let pk = &[i as u8, y as u8];
                must_prewrite_put(&engine, pk, b"", pk, start_ts);
                must_rollback(&engine, pk, start_ts);
                // Generate 65534 RocksDB tombstones between [0,0] and [255,255].
                if !((i == 0 && y == 0) || (i == 255 && y == 255)) {
                    must_gc(&engine, pk, safe_point);
                }
            }
        }

        // Generate 256 locks in lock cf.
        let start_ts = 3;
        for i in 0..256 {
            let pk = &[i as u8];
            must_prewrite_put(&engine, pk, b"", pk, start_ts);
        }

        let snapshot = engine.snapshot(&Context::new()).unwrap();
        let row = &[255 as u8];
        let k = Key::from_raw(row);

        // Call reverse scan
        let ts = 2;
        let mut scanner = BackwardScannerBuilder::new(snapshot, ts)
            .range(None, Some(k))
            .build()
            .unwrap();
        assert_eq!(scanner.read_next().unwrap(), None);
        let statistics = scanner.take_statistics();
        assert_eq!(statistics.lock.prev, 256);
        assert_eq!(statistics.write.prev, 1);
    }

}
