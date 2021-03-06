// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

// Code to handle a collection of block devices.

use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

use chrono::{DateTime, Duration, Utc};
use rand::{seq::IteratorRandom, thread_rng};
use serde_json::Value;

use devicemapper::{Bytes, Device, LinearDevTargetParams, LinearTargetParams, Sectors, TargetLine};

use crate::{
    engine::{
        strat_engine::{
            backstore::{
                blockdev::StratBlockDev,
                crypt::{interpret_clevis_config, CryptActivationHandle},
                devices::{initialize_devices, process_and_verify_devices, wipe_blockdevs},
            },
            keys::MemoryPrivateFilesystem,
            metadata::MDADataSize,
            names::KeyDescription,
            serde_structs::{BaseBlockDevSave, BaseDevSave, Recordable},
        },
        types::{DevUuid, EncryptionInfo, PoolUuid},
    },
    stratis::{ErrorEnum, StratisError, StratisResult},
};

const MAX_NUM_TO_WRITE: usize = 10;

/// struct to represent a continuous set of sectors on a disk
#[derive(Debug, Clone)]
pub struct Segment {
    /// The offset into the device where this segment starts.
    pub(super) start: Sectors,
    /// The length of the segment.
    pub(super) length: Sectors,
    /// The device the segment is within.
    pub(super) device: Device,
}

impl Segment {
    /// Create a new Segment with given attributes
    pub fn new(device: Device, start: Sectors, length: Sectors) -> Segment {
        Segment {
            device,
            start,
            length,
        }
    }
}

#[derive(Clone, Debug)]
pub struct BlkDevSegment {
    pub(super) uuid: DevUuid,
    pub(super) segment: Segment,
}

impl BlkDevSegment {
    pub fn new(uuid: DevUuid, segment: Segment) -> BlkDevSegment {
        BlkDevSegment { uuid, segment }
    }

    pub fn to_segment(&self) -> Segment {
        self.segment.clone()
    }
}

impl Recordable<Vec<BaseDevSave>> for Vec<BlkDevSegment> {
    fn record(&self) -> Vec<BaseDevSave> {
        self.iter()
            .map(|bseg| BaseDevSave {
                parent: bseg.uuid,
                start: bseg.segment.start,
                length: bseg.segment.length,
            })
            .collect::<Vec<_>>()
    }
}

/// Build a linear dev target table from BlkDevSegments. This is useful for
/// calls to the devicemapper library.
pub fn map_to_dm(bsegs: &[BlkDevSegment]) -> Vec<TargetLine<LinearDevTargetParams>> {
    let mut table = Vec::new();
    let mut logical_start_offset = Sectors(0);

    let segments = bsegs
        .iter()
        .map(|bseg| bseg.to_segment())
        .collect::<Vec<_>>();
    for segment in segments {
        let (physical_start_offset, length) = (segment.start, segment.length);
        let params = LinearTargetParams::new(segment.device, physical_start_offset);
        let line = TargetLine::new(
            logical_start_offset,
            length,
            LinearDevTargetParams::Linear(params),
        );
        table.push(line);
        logical_start_offset += length;
    }

    table
}

#[derive(Debug)]
pub struct BlockDevMgr {
    /// All the block devices that belong to this block dev manager.
    block_devs: Vec<StratBlockDev>,
    /// The most recent time that variable length metadata was saved to the
    /// devices managed by this block dev manager.
    last_update_time: Option<DateTime<Utc>>,
}

impl BlockDevMgr {
    /// Make a struct that represents an existing BlockDevMgr.
    pub fn new(
        block_devs: Vec<StratBlockDev>,
        last_update_time: Option<DateTime<Utc>>,
    ) -> BlockDevMgr {
        BlockDevMgr {
            block_devs,
            last_update_time,
        }
    }

    /// Initialize a new StratBlockDevMgr with specified pool and devices.
    pub fn initialize(
        pool_uuid: PoolUuid,
        paths: &[&Path],
        mda_data_size: MDADataSize,
        key_desc: Option<&KeyDescription>,
    ) -> StratisResult<BlockDevMgr> {
        let devices = process_and_verify_devices(pool_uuid, &HashSet::new(), paths)?;

        Ok(BlockDevMgr::new(
            initialize_devices(
                devices,
                pool_uuid,
                mda_data_size,
                key_desc.map(|k| EncryptionInfo {
                    key_description: k.clone(),
                    clevis_info: None,
                }),
            )?,
            None,
        ))
    }

    /// Get a hashmap that maps UUIDs to Devices.
    pub fn uuid_to_devno(&self) -> HashMap<DevUuid, Device> {
        self.block_devs
            .iter()
            .map(|bd| (bd.uuid(), *bd.device()))
            .collect()
    }

    /// Check that the registered key description for these block devices can
    /// unlock at least one of the existing block devices registered.
    /// Precondition: self.block_devs must have at least one device.
    pub fn has_valid_passphrase(&self) -> bool {
        CryptActivationHandle::can_unlock(
            self.block_devs
                .get(0)
                .expect("Must have at least one blockdev")
                .physical_path(),
        )
    }

    /// Add paths to self.
    /// Return the uuids of all blockdevs corresponding to paths that were
    /// added.
    pub fn add(&mut self, pool_uuid: PoolUuid, paths: &[&Path]) -> StratisResult<Vec<DevUuid>> {
        let this_pool_uuid = self.block_devs.get(0).map(|bd| bd.pool_uuid());
        if this_pool_uuid.is_some() && this_pool_uuid != Some(pool_uuid) {
            return Err(StratisError::Engine(
                ErrorEnum::Invalid,
                format!("block devices being managed have pool UUID {} but new devices are to be added with pool UUID {}",
                        this_pool_uuid.expect("guarded by if-expression").to_simple_ref(),
                        pool_uuid)
            ));
        }

        let current_uuids = self
            .block_devs
            .iter()
            .map(|bd| bd.uuid())
            .collect::<HashSet<_>>();
        let devices = process_and_verify_devices(pool_uuid, &current_uuids, paths)?;

        let encryption_info = self.encryption_info();
        if encryption_info.is_some() && !self.has_valid_passphrase() {
            return Err(StratisError::Engine(
                ErrorEnum::Invalid,
                "The key associated with the current registered key description \
                was not able to unlock an existing encrypted device; check that \
                the same key is in the keyring that was used to create the encrypted \
                pool"
                    .to_string(),
            ));
        }

        // FIXME: This is a bug. If new devices are added to a pool, and the
        // variable length metadata requires more than the minimum allocated,
        // then the necessary amount must be provided or the data can not be
        // saved.
        let bds = initialize_devices(
            devices,
            pool_uuid,
            MDADataSize::default(),
            encryption_info.cloned(),
        )?;
        let bdev_uuids = bds.iter().map(|bd| bd.uuid()).collect();
        self.block_devs.extend(bds);
        Ok(bdev_uuids)
    }

    pub fn destroy_all(&mut self) -> StratisResult<()> {
        wipe_blockdevs(&mut self.block_devs)
    }

    /// Remove the specified block devs and erase their metadata.
    ///
    /// Precondition: It is the responsibility of the caller to ensure that
    /// none of the blockdevs are in use, that is, have had any space allocated
    /// from them for upper layers.
    ///
    /// If a specified blockdev is not found, returns an error and does nothing.
    ///
    /// NOTE: This method traverses the block_devs Vec from the rear to the
    /// front, looking for blockdevs to remove. This is algorithmically
    /// inefficient, unless it is assumed that the blockdevs specified are very
    /// near the end of the Vec, which is expected to be the case. In that case,
    /// the algorithm is O(n).
    pub(super) fn remove_blockdevs(&mut self, uuids: &[DevUuid]) -> StratisResult<()> {
        let mut removed = Vec::new();
        for uuid in uuids {
            let mut found = false;
            let blockdevs_last_index = self.block_devs.len() - 1;
            for i in 0..blockdevs_last_index {
                let index = blockdevs_last_index - i;
                if self.block_devs[index].uuid() == *uuid {
                    removed.push(self.block_devs.swap_remove(index));
                    found = true;
                    break;
                }
            }
            if !found {
                return Err(StratisError::Engine(
                    ErrorEnum::Error,
                    format!(
                        "Blockdev corresponding to UUID: {} not found.",
                        uuid.to_simple_ref()
                    ),
                ));
            }
        }
        wipe_blockdevs(&mut removed)?;
        Ok(())
    }

    /// Allocate space according to sizes vector request.
    /// Return the segments allocated for each request, or None if it was
    /// not possible to satisfy the request.
    /// This method is atomic, it either allocates all requested or allocates
    /// nothing.
    pub fn alloc_space(&mut self, sizes: &[Sectors]) -> Option<Vec<Vec<BlkDevSegment>>> {
        let total_needed: Sectors = sizes.iter().cloned().sum();
        if self.avail_space() < total_needed {
            return None;
        }

        let mut lists = Vec::new();
        for &needed in sizes {
            let mut alloc = Sectors(0);
            let mut segs = Vec::new();
            // TODO: Consider greater efficiency for allocation generally.
            // Over time, the blockdevs at the start will be exhausted. It
            // might be a good idea to keep an auxiliary structure, so that
            // only blockdevs with some space left to allocate are accessed.
            // In the context of this major inefficiency that ensues over time
            // the obvious but more minor inefficiency of this inner loop is
            // not worth worrying about.
            for bd in &mut self.block_devs {
                if alloc == needed {
                    break;
                }

                let r_segs = bd.request_space(needed - alloc);
                let blkdev_segs = r_segs.iter().map(|(&start, &length)| {
                    BlkDevSegment::new(bd.uuid(), Segment::new(*bd.device(), start, length))
                });
                segs.extend(blkdev_segs);
                alloc += r_segs.sum();
            }
            assert_eq!(alloc, needed);
            lists.push(segs);
        }

        Some(lists)
    }

    /// Write the given data to all blockdevs marking with current time.
    /// Return an error if data was not written to any blockdev.
    /// Omit blockdevs which do not have sufficient space in BDA to accommodate
    /// metadata. If current time is not more recent than previously written
    /// time, use a time that is one nanosecond greater than that previously
    /// written. Randomly select no more than MAX_NUM_TO_WRITE blockdevs to
    /// write to.
    pub fn save_state(&mut self, metadata: &[u8]) -> StratisResult<()> {
        let current_time = Utc::now();
        let stamp_time = if Some(current_time) <= self.last_update_time {
            self.last_update_time
                .expect("self.last_update_time >= Some(current_time")
                .checked_add_signed(Duration::nanoseconds(1))
                .expect("self.last_update_time << maximum representable DateTime")
        } else {
            current_time
        };

        let data_size = Bytes::from(metadata.len());
        let candidates = self
            .block_devs
            .iter_mut()
            .filter(|b| b.max_metadata_size().bytes() >= data_size);

        // TODO: consider making selection not entirely random, i.e, ensuring
        // distribution of metadata over different paths.
        let saved = candidates
            .choose_multiple(&mut thread_rng(), MAX_NUM_TO_WRITE)
            .iter_mut()
            .fold(false, |acc, b| {
                acc | b.save_state(&stamp_time, metadata).is_ok()
            });

        if saved {
            self.last_update_time = Some(stamp_time);
            Ok(())
        } else {
            let err_msg = "Failed to save metadata to even one device in pool";
            Err(StratisError::Engine(ErrorEnum::Error, err_msg.into()))
        }
    }

    /// Get references to managed blockdevs.
    pub fn blockdevs(&self) -> Vec<(DevUuid, &StratBlockDev)> {
        self.block_devs.iter().map(|bd| (bd.uuid(), bd)).collect()
    }

    pub fn blockdevs_mut(&mut self) -> Vec<(DevUuid, &mut StratBlockDev)> {
        self.block_devs
            .iter_mut()
            .map(|bd| (bd.uuid(), bd as &mut StratBlockDev))
            .collect()
    }

    pub fn get_blockdev_by_uuid(&self, uuid: DevUuid) -> Option<&StratBlockDev> {
        self.block_devs.iter().find(|bd| bd.uuid() == uuid)
    }

    pub fn get_mut_blockdev_by_uuid(&mut self, uuid: DevUuid) -> Option<&mut StratBlockDev> {
        self.block_devs.iter_mut().find(|bd| bd.uuid() == uuid)
    }

    // SIZE methods

    /// The number of sectors not allocated for any purpose.
    pub fn avail_space(&self) -> Sectors {
        self.block_devs.iter().map(|bd| bd.available()).sum()
    }

    /// The current size of all the blockdevs.
    /// self.size() > self.avail_space() because some sectors are certainly
    /// allocated for Stratis metadata
    pub fn size(&self) -> Sectors {
        self.block_devs
            .iter()
            .map(|b| b.total_size().sectors())
            .sum()
    }

    /// The number of sectors given over to Stratis metadata
    /// self.size() - self.metadata_size() >= self.avail_space()
    pub fn metadata_size(&self) -> Sectors {
        self.block_devs
            .iter()
            .map(|bd| bd.metadata_size().sectors())
            .sum()
    }

    pub fn encryption_info(&self) -> Option<&EncryptionInfo> {
        let mut iter = self.block_devs.iter().map(|bd| bd.encryption_info());
        let info = iter.next().and_then(|opt| opt);

        // Liminal device code will not set up a pool with devices with
        // different encryption information.
        assert!(iter.all(|elem| info == elem));

        info
    }

    pub fn is_encrypted(&self) -> bool {
        self.encryption_info().is_some()
    }

    #[cfg(test)]
    fn invariant(&self) {
        let pool_uuids = self
            .block_devs
            .iter()
            .map(|bd| bd.pool_uuid())
            .collect::<HashSet<_>>();
        assert!(pool_uuids.len() == 1);

        let encryption_infos = self
            .block_devs
            .iter()
            .filter_map(|bd| bd.encryption_info())
            .collect::<Vec<_>>();
        if encryption_infos.is_empty() {
            assert_eq!(self.encryption_info(), None);
        } else {
            assert_eq!(encryption_infos.len(), self.block_devs.len());

            let info_set = encryption_infos.iter().collect::<HashSet<_>>();
            assert!(info_set.len() == 1);
        }
    }

    /// Bind all devices in the given blockdev manager using the given clevis
    /// configuration.
    ///
    /// * Returns Ok(true) if the binding was performed.
    /// * Returns Ok(false) if the binding had already been previously performed and
    /// nothing was changed.
    /// * Returns Err(_) if an inconsistency was found in the metadata across pools
    /// or binding failed.
    pub fn bind_clevis(&mut self, pin: String, mut clevis_info: Value) -> StratisResult<bool> {
        fn bind_clevis_loop<'a, I>(
            key_fs: &MemoryPrivateFilesystem,
            blockdevs: I,
            pin: &str,
            clevis_info: &Value,
            yes: bool,
        ) -> StratisResult<()>
        where
            I: IntoIterator<Item = &'a mut StratBlockDev>,
        {
            let mut rollback_record = Vec::new();
            for blockdev_ref in blockdevs {
                if let Err(e) = blockdev_ref.bind_clevis(key_fs, pin, clevis_info, yes) {
                    rollback_loop(rollback_record);
                    return Err(e);
                } else {
                    rollback_record.push(blockdev_ref);
                }
            }
            Ok(())
        }

        fn rollback_loop(rollback_record: Vec<&mut StratBlockDev>) {
            rollback_record.into_iter().for_each(|blockdev| {
                if let Err(e) = blockdev.unbind_clevis() {
                    warn!(
                        "Failed to unbind device {} from clevis during \
                        rollback: {}",
                        blockdev.physical_path().display(),
                        e,
                    );
                }
            });
        }

        let encryption_info = match self.encryption_info() {
            Some(info) => info,
            None => {
                return Err(StratisError::Error(
                    "Requested pool does not appear to be encrypted".to_string(),
                ))
            }
        };

        let yes = interpret_clevis_config(&pin, &mut clevis_info)?;

        if let Some(info) = &encryption_info.clevis_info {
            let clevis_tuple = (pin, clevis_info);
            if info == &clevis_tuple {
                return Ok(false);
            } else {
                return Err(StratisError::Error(format!(
                    "Block devices have already been bound with pin {} and config {}; \
                    requested pin {} and config {} can't be applied",
                    info.0, info.1, clevis_tuple.0, clevis_tuple.1,
                )));
            }
        }

        let key_fs = MemoryPrivateFilesystem::new()?;

        bind_clevis_loop(
            &key_fs,
            self.blockdevs_mut().into_iter().map(|(_, bd)| bd),
            pin.as_str(),
            &clevis_info,
            yes,
        )?;

        Ok(true)
    }

    pub fn unbind_clevis(&mut self) -> StratisResult<bool> {
        match self.encryption_info() {
            None => {
                return Err(StratisError::Error(
                    "Requested pool does not appear to be encrypted".to_string(),
                ));
            }
            Some(info) => {
                if info.clevis_info.is_none() {
                    return Ok(false);
                }
            }
        }

        for blockdev in self.blockdevs_mut().into_iter().map(|(_, bd)| bd) {
            let res = blockdev.unbind_clevis();
            if let Err(ref e) = res {
                warn!(
                    "Failed to unbind from the tang server using clevis: {}. \
                    This operation cannot be rolled back automatically.",
                    e,
                );
            }
            res?
        }
        Ok(true)
    }
}

impl Recordable<Vec<BaseBlockDevSave>> for BlockDevMgr {
    fn record(&self) -> Vec<BaseBlockDevSave> {
        self.block_devs.iter().map(|bd| bd.record()).collect()
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use crate::engine::strat_engine::{
        cmd,
        tests::{crypt, loopbacked, real},
    };

    use super::*;

    /// Verify that initially,
    /// size() - metadata_size() = avail_space().
    /// After 2 Sectors have been allocated, that amount must also be included
    /// in balance.
    fn test_blockdevmgr_used(paths: &[&Path]) {
        let mut mgr =
            BlockDevMgr::initialize(PoolUuid::new_v4(), paths, MDADataSize::default(), None)
                .unwrap();
        assert_eq!(mgr.avail_space() + mgr.metadata_size(), mgr.size());

        let allocated = Sectors(2);
        mgr.alloc_space(&[allocated]).unwrap();
        assert_eq!(
            mgr.avail_space() + allocated + mgr.metadata_size(),
            mgr.size()
        );
    }

    #[test]
    fn loop_test_blockdevmgr_used() {
        loopbacked::test_with_spec(
            &loopbacked::DeviceLimits::Range(1, 3, None),
            test_blockdevmgr_used,
        );
    }

    #[test]
    fn real_test_blockdevmgr_used() {
        real::test_with_spec(
            &real::DeviceLimits::AtLeast(1, None, None),
            test_blockdevmgr_used,
        );
    }

    #[test]
    fn travis_test_blockdevmgr_used() {
        loopbacked::test_with_spec(
            &loopbacked::DeviceLimits::Range(1, 3, None),
            test_blockdevmgr_used,
        );
    }

    /// Test that the `BlockDevMgr` will add devices if the same key
    /// is used to encrypted the existing devices and the added devices.
    fn test_blockdevmgr_same_key(paths: &[&Path]) {
        fn test_with_key(
            paths: &[&Path],
            key_desc: &KeyDescription,
            _: Option<()>,
        ) -> Result<(), Box<dyn Error>> {
            let pool_uuid = PoolUuid::new_v4();
            let mut bdm = BlockDevMgr::initialize(
                pool_uuid,
                &paths[..2],
                MDADataSize::default(),
                Some(key_desc),
            )?;

            if bdm.add(pool_uuid, &paths[2..3]).is_err() {
                Err(Box::new(StratisError::Error(
                    "Adding a blockdev with the same key to an encrypted pool should succeed"
                        .to_string(),
                )))
            } else {
                Ok(())
            }
        }

        crypt::insert_and_cleanup_key(paths, test_with_key);
    }

    #[test]
    fn loop_test_blockdevmgr_same_key() {
        loopbacked::test_with_spec(
            &loopbacked::DeviceLimits::Exactly(3, None),
            test_blockdevmgr_same_key,
        );
    }

    #[test]
    fn real_test_blockdevmgr_same_key() {
        real::test_with_spec(
            &real::DeviceLimits::Exactly(3, None, None),
            test_blockdevmgr_same_key,
        );
    }

    #[test]
    fn travis_test_blockdevmgr_same_key() {
        loopbacked::test_with_spec(
            &loopbacked::DeviceLimits::Exactly(3, None),
            test_blockdevmgr_same_key,
        );
    }

    /// Test that the `BlockDevMgr` will not add devices if a different key
    /// is present in the keyring than was used to encrypted the existing
    /// devices.
    fn test_blockdevmgr_changed_key(paths: &[&Path]) {
        fn test_with_first_key(
            paths: &[&Path],
            key_desc: &KeyDescription,
            _: Option<()>,
        ) -> Result<(PoolUuid, BlockDevMgr), Box<dyn Error>> {
            let pool_uuid = PoolUuid::new_v4();
            let bdm = BlockDevMgr::initialize(
                pool_uuid,
                &paths[..2],
                MDADataSize::default(),
                Some(key_desc),
            )?;
            Ok((pool_uuid, bdm))
        }

        fn test_with_second_key(
            paths: &[&Path],
            _: &KeyDescription,
            data: (PoolUuid, BlockDevMgr),
        ) -> Result<(), Box<dyn Error>> {
            let (pool_uuid, mut bdm) = data;
            if bdm.add(pool_uuid, &paths[2..3]).is_ok() {
                Err(Box::new(StratisError::Error(
                    "Adding a blockdev with a new key to an encrypted pool should fail".to_string(),
                )))
            } else {
                Ok(())
            }
        }

        crypt::insert_and_cleanup_two_keys(paths, test_with_first_key, test_with_second_key);
    }

    #[test]
    fn loop_test_blockdevmgr_changed_key() {
        loopbacked::test_with_spec(
            &loopbacked::DeviceLimits::Exactly(3, None),
            test_blockdevmgr_changed_key,
        );
    }

    #[test]
    fn real_test_blockdevmgr_changed_key() {
        real::test_with_spec(
            &real::DeviceLimits::Exactly(3, None, None),
            test_blockdevmgr_changed_key,
        );
    }

    #[test]
    fn travis_test_blockdevmgr_changed_key() {
        loopbacked::test_with_spec(
            &loopbacked::DeviceLimits::Exactly(3, None),
            test_blockdevmgr_changed_key,
        );
    }

    /// Verify that it is impossible to steal blockdevs from another Stratis
    /// pool.
    /// 1. Initialize devices with pool uuid.
    /// 2. Initializing again with different uuid must fail.
    /// 3. Adding the devices must succeed, because they already belong.
    fn test_initialization_add_stratis(paths: &[&Path]) {
        assert!(paths.len() > 1);
        let (paths1, paths2) = paths.split_at(paths.len() / 2);

        let uuid = PoolUuid::new_v4();
        let uuid2 = PoolUuid::new_v4();

        let mut bd_mgr =
            BlockDevMgr::initialize(uuid, paths1, MDADataSize::default(), None).unwrap();
        cmd::udev_settle().unwrap();

        assert_matches!(
            BlockDevMgr::initialize(uuid2, paths1, MDADataSize::default(), None),
            Err(_)
        );

        let original_length = bd_mgr.block_devs.len();
        assert_matches!(bd_mgr.add(uuid2, paths1), Err(_));
        assert_matches!(bd_mgr.add(uuid, paths1), Ok(_));
        assert_eq!(bd_mgr.block_devs.len(), original_length);

        BlockDevMgr::initialize(uuid, paths2, MDADataSize::default(), None).unwrap();
        cmd::udev_settle().unwrap();

        assert_matches!(bd_mgr.add(uuid, paths2), Err(_));

        bd_mgr.invariant()
    }

    #[test]
    fn loop_test_initialization_stratis() {
        loopbacked::test_with_spec(
            &loopbacked::DeviceLimits::Range(2, 3, None),
            test_initialization_add_stratis,
        );
    }

    #[test]
    fn real_test_initialization_stratis() {
        real::test_with_spec(
            &real::DeviceLimits::AtLeast(2, None, None),
            test_initialization_add_stratis,
        );
    }

    #[test]
    fn travis_test_initialization_stratis() {
        loopbacked::test_with_spec(
            &loopbacked::DeviceLimits::Range(2, 3, None),
            test_initialization_add_stratis,
        );
    }
}
