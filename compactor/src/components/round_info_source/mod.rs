use std::{
    cmp::max,
    fmt::{Debug, Display},
    sync::Arc,
};

use crate::components::{
    split_or_compact::start_level_files_to_split::{
        linear_dist_ranges, merge_small_l0_chains, select_split_times, split_into_chains,
    },
    Components,
};
use async_trait::async_trait;
use data_types::{CompactionLevel, ParquetFile, Timestamp};
use itertools::Itertools;
use observability_deps::tracing::debug;

use crate::{error::DynError, PartitionInfo, RoundInfo};

/// Calculates information about what this compaction round does.
/// When we get deeper into the compaction decision making, there
/// may not be as much context information available.  It may not
/// be possible to reach the same conclusions about the intention
/// for this compaction round.  So RoundInfo must contain enough
/// information carry that intention through the compactions.
#[async_trait]
pub trait RoundInfoSource: Debug + Display + Send + Sync {
    async fn calculate(
        &self,
        components: Arc<Components>,
        partition_info: &PartitionInfo,
        files: Vec<ParquetFile>,
    ) -> Result<(RoundInfo, Vec<Vec<ParquetFile>>, Vec<ParquetFile>), DynError>;
}

#[derive(Debug)]
pub struct LoggingRoundInfoWrapper {
    inner: Arc<dyn RoundInfoSource>,
}

impl LoggingRoundInfoWrapper {
    pub fn new(inner: Arc<dyn RoundInfoSource>) -> Self {
        Self { inner }
    }
}

impl Display for LoggingRoundInfoWrapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LoggingRoundInfoWrapper({})", self.inner)
    }
}

#[async_trait]
impl RoundInfoSource for LoggingRoundInfoWrapper {
    async fn calculate(
        &self,
        components: Arc<Components>,
        partition_info: &PartitionInfo,
        files: Vec<ParquetFile>,
    ) -> Result<(RoundInfo, Vec<Vec<ParquetFile>>, Vec<ParquetFile>), DynError> {
        let res = self
            .inner
            .calculate(components, partition_info, files)
            .await;
        if let Ok((round_info, branches, files_later)) = &res {
            debug!(round_info_source=%self.inner, %round_info, branches=branches.len(), files_later=files_later.len(), "running round");
        }
        res
    }
}

/// Computes the type of round based on the levels of the input files
#[derive(Debug)]
pub struct LevelBasedRoundInfo {
    pub max_num_files_per_plan: usize,
    pub max_total_file_size_per_plan: usize,
}

impl Display for LevelBasedRoundInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LevelBasedRoundInfo {}", self.max_num_files_per_plan)
    }
}
impl LevelBasedRoundInfo {
    pub fn new(max_num_files_per_plan: usize, max_total_file_size_per_plan: usize) -> Self {
        Self {
            max_num_files_per_plan,
            max_total_file_size_per_plan,
        }
    }

    /// Returns true if the scenario looks like ManySmallFiles, but we can't group them well into branches.
    pub fn many_ungroupable_files(
        &self,
        files: &[ParquetFile],
        start_level: CompactionLevel,
        max_total_file_size_to_group: usize,
    ) -> bool {
        if self.too_many_small_files_to_compact(files, CompactionLevel::Initial) {
            let start_level_files = files
                .iter()
                .filter(|f| f.compaction_level == start_level)
                .collect::<Vec<_>>();
            let start_count = start_level_files.len();
            let mut chains = split_into_chains(start_level_files.into_iter().cloned().collect());
            chains = merge_small_l0_chains(chains, max_total_file_size_to_group);

            if chains.len() > 1 && chains.len() > start_count / 3 {
                return true;
            }
        }
        false
    }

    /// Returns true if number of files of the given start_level and
    /// their overlapped files in next level is over limit, and if those
    /// files are sufficiently small.
    ///
    /// over the limit means that the maximum number of files that a subsequent compaction
    /// branch may choose to compact in a single plan would exceed `max_num_files_per_plan`
    pub fn too_many_small_files_to_compact(
        &self,
        files: &[ParquetFile],
        start_level: CompactionLevel,
    ) -> bool {
        let start_level_files = files
            .iter()
            .filter(|f| f.compaction_level == start_level)
            .collect::<Vec<_>>();
        let num_start_level = start_level_files.len();
        let size_start_level: usize = start_level_files
            .iter()
            .map(|f| f.file_size_bytes as usize)
            .sum();
        let start_max_l0_created_at = start_level_files
            .iter()
            .map(|f| f.max_l0_created_at)
            .unique()
            .count();

        let next_level_files = files
            .iter()
            .filter(|f| f.compaction_level == start_level.next())
            .collect::<Vec<_>>();

        // The compactor may compact all the target level and next level together in one
        // branch in the worst case, thus if that would result in too many files to compact in a single
        // plan, run a pre-phase to reduce the number of files first
        let num_overlapped_files = get_num_overlapped_files(start_level_files, next_level_files);
        if num_start_level > 1
            && num_start_level + num_overlapped_files > self.max_num_files_per_plan
        {
            // This scaenario meets the simple criteria of start level files + their overlaps are lots of files.
            // But ManySmallFiles implies we must compact only within the start level to reduce the quantity of
            // start level files. There are several reasons why that might be unhelpful.

            // Reason 1: if all the start level files have the same max_l0_created_at, then they were split from
            // the same file.  If we previously decided to split them, we should not undo that now.
            if start_max_l0_created_at == 1 {
                return false;
            }

            // Reason 2: Maybe its many LARGE files making reduction of file count in the start level impossible.
            if size_start_level / num_start_level
                > self.max_total_file_size_per_plan / self.max_num_files_per_plan
            {
                // Average start level file size is more than the average implied by max bytes & files per plan.
                // Even though there are "many files", this is not "many small files".
                // There isn't much (perhaps not any) file reduction to be done, attempting it can get us stuck
                // in a loop.
                return false;
            }

            // Reason 3: Maybe there are so many start level files because we did a bunch of splits.
            // Note that we'll do splits to ensure each start level file overlaps at most one target level file.
            // If the prior round did that, and now we declare this ManySmallFiles, which forces compactions
            // within the start level, we'll undo the splits performed in the prior round, which can get us
            // stuck in a loop.
            let chains = split_into_chains(files.to_vec());
            let mut max_target_level_files: usize = 0;
            let mut max_chain_len: usize = 0;
            for chain in chains {
                let target_file_cnt = chain
                    .iter()
                    .filter(|f| f.compaction_level == start_level.next())
                    .count();
                max_target_level_files = max(max_target_level_files, target_file_cnt);

                let chain_len = chain.len();
                max_chain_len = max(max_chain_len, chain_len);
            }
            if max_target_level_files <= 1 && max_chain_len <= self.max_num_files_per_plan {
                // All of our start level files overlap with at most one target level file.  If the prior round did
                // splits to cause this, declaring this a ManySmallFiles case can lead to an endless loop.
                // If we got lucky and this happened without splits, declaring this ManySmallFiles will waste
                // our good fortune.
                return false;
            }
            return true;
        }

        false
    }

    /// vertical_split_times evaluates the files to determine if vertical splitting is necessary.  If so,
    /// a vec of split times is returned.  The caller will use those split times in a VerticalSplit RoundInfo.
    /// If no split times are returned, that implies veritcal splitting is not appropriate, and the caller
    /// will identify another type of RoundInfo for this round of compaction.
    pub fn vertical_split_times(
        &self,
        files: Vec<ParquetFile>,
        max_compact_size: usize,
    ) -> Vec<i64> {
        let (start_level_files, target_level_files): (Vec<ParquetFile>, Vec<ParquetFile>) = files
            .into_iter()
            .filter(|f| f.compaction_level != CompactionLevel::Final)
            .partition(|f| f.compaction_level == CompactionLevel::Initial);

        let len = start_level_files.len();
        let mut split_times = Vec::with_capacity(len);

        // Break up the start level files into chains of files that overlap each other.
        // Then we'll determine if vertical splitting is needed within each chain.
        let chains = split_into_chains(start_level_files);

        for chain in chains {
            let chain_cap: usize = chain.iter().map(|f| f.file_size_bytes as usize).sum();

            // A single file over max size can just get upgraded to L1, then L2, unless it overlaps other L0s.
            // So multi file chains over the max compact size may need split
            if chain.len() > 1 && chain_cap > max_compact_size {
                // This chain is too big to compact on its own, so files will be split it into smaller, more manageable chains.
                // We can't know the data distribution within each file without reading the file (too expensive), but we can
                // still learn a lot about the data distribution accross the set of files by assuming even distribtuion within each
                // file and considering the distribution of files within the chain's time range.
                let linear_ranges = linear_dist_ranges(&chain, chain_cap, max_compact_size);

                for range in linear_ranges {
                    // split at every time range of linear distribution.
                    if !split_times.is_empty() {
                        split_times.push(range.min - 1);
                    }

                    // how many start level files are in this range?
                    let overlaps = chain
                        .iter()
                        .filter(|f| {
                            f.overlaps_time_range(
                                Timestamp::new(range.min),
                                Timestamp::new(range.max),
                            )
                        })
                        .count();

                    if overlaps > 1 && range.cap > max_compact_size {
                        // Since we'll be splitting the start level files within this range, it would be nice to align the split times to
                        // the min/max times of target level files.  select_split_times will use the min/max time of target level files
                        // as hints, and see what lines up to where the range needs split.
                        let mut split_hints: Vec<i64> =
                            Vec::with_capacity(range.cap * 2 / max_compact_size + 1);

                        // split time is the last time included in the 'left' side of the split.  Our goal with these hints is to avoid
                        // overlaps with L1 files, we'd like the 'left file' to end before this L1 file starts (split=min-1), or it can
                        // include up to the last ns of the L1 file (split=max).
                        for f in &target_level_files {
                            if f.min_time.get() - 1 > range.min && f.min_time.get() < range.max {
                                split_hints.push(f.min_time.get() - 1);
                            }
                            if f.max_time.get() > range.min && f.max_time.get() < range.max {
                                split_hints.push(f.max_time.get());
                            }
                        }

                        let splits = select_split_times(
                            range.cap,
                            max_compact_size,
                            range.min,
                            range.max,
                            split_hints.clone(),
                        );
                        split_times.extend(splits);
                    }
                }
            }
        }

        split_times.sort();
        split_times.dedup();
        split_times
    }
}

#[async_trait]
impl RoundInfoSource for LevelBasedRoundInfo {
    // The calculated RoundInfo is the most impactful decision for this round of compactions.
    // Later decisions should be just working out details to implement what RoundInfo dictates.
    async fn calculate(
        &self,
        components: Arc<Components>,
        _partition_info: &PartitionInfo,
        files: Vec<ParquetFile>,
    ) -> Result<(RoundInfo, Vec<Vec<ParquetFile>>, Vec<ParquetFile>), DynError> {
        // start_level is usually the lowest level we have files in, but occasionally we decide to
        // compact L1->L2 when L0s still exist.  If this comes back as L1, we'll ignore L0s for this
        // round and force an early L1-L2 compaction.
        let start_level = get_start_level(
            &files,
            self.max_num_files_per_plan,
            self.max_total_file_size_per_plan,
        );

        let round_info = if start_level == CompactionLevel::Initial {
            let split_times = self
                .vertical_split_times(files.clone().to_vec(), self.max_total_file_size_per_plan);
            if !split_times.is_empty() {
                RoundInfo::VerticalSplit { split_times }
            } else if self.many_ungroupable_files(&files, start_level, self.max_num_files_per_plan)
            {
                RoundInfo::SimulatedLeadingEdge {
                    max_num_files_to_group: self.max_num_files_per_plan,
                    max_total_file_size_to_group: self.max_total_file_size_per_plan,
                }
            } else if self.too_many_small_files_to_compact(&files, start_level) {
                RoundInfo::ManySmallFiles {
                    start_level,
                    max_num_files_to_group: self.max_num_files_per_plan,
                    max_total_file_size_to_group: self.max_total_file_size_per_plan,
                }
            } else {
                RoundInfo::TargetLevel {
                    target_level: CompactionLevel::FileNonOverlapped,
                    max_total_file_size_to_group: self.max_total_file_size_per_plan,
                }
            }
        } else {
            let target_level = start_level.next();
            RoundInfo::TargetLevel {
                target_level,
                max_total_file_size_to_group: self.max_total_file_size_per_plan,
            }
        };

        let (files_now, mut files_later) = components.round_split.split(files, round_info.clone());

        let (branches, more_for_later) = components
            .divide_initial
            .divide(files_now, round_info.clone());
        files_later.extend(more_for_later);

        Ok((round_info, branches, files_later))
    }
}

// get_start_level decides what level to start compaction from.  Often this is the lowest level
// we have ParquetFiles in, but occasionally we decide to compact L1->L2 when L0s still exist.
//
// If we ignore the invariants (where intra-level overlaps are allowed), this would be a math problem
// to optimize write amplification.
//
// However, allowing intra-level overlaps in L0 but not L1/L2 adds extra challenge to compacting L0s to L1.
// This is especially true when there are large quantitites of overlapping L0s and L1s, potentially resulting
// in many split/compact cycles to resolve the overlaps.
//
// Since L1 & L2 only have inter-level overlaps, they can be compacted with just a few splits to align the L1s
// with the L2s.  The relative ease of moving data from L1 to L2 provides additional motivation to compact the
// L1s to L2s when a backlog of L0s exist. The easily solvable L1->L2 compaction can give us a clean slate in
// L1, greatly simplifying the remaining L0->L1 compactions.
fn get_start_level(files: &[ParquetFile], max_files: usize, max_bytes: usize) -> CompactionLevel {
    // panic if the files are empty
    assert!(!files.is_empty());

    let mut l0_cnt: usize = 0;
    let mut l0_bytes: usize = 0;
    let mut l1_bytes: usize = 0;

    for f in files {
        match f.compaction_level {
            CompactionLevel::Initial => {
                l0_cnt += 1;
                l0_bytes += f.file_size_bytes as usize;
            }
            CompactionLevel::FileNonOverlapped => {
                l1_bytes += f.file_size_bytes as usize;
            }
            _ => {}
        }
    }

    if l1_bytes > 3 * max_bytes && (l0_cnt > max_files || l0_bytes > max_bytes) {
        // L1 is big enough to pose an overlap challenge compacting from L0, and there is quite a bit more coming from L0.
        // The criteria for this early L1->L2 compaction significanly impacts write amplification.  The above values optimize
        // existing test cases, but may be changed as additional test cases are added.
        CompactionLevel::FileNonOverlapped
    } else if l0_bytes > 0 {
        CompactionLevel::Initial
    } else if l1_bytes > 0 {
        CompactionLevel::FileNonOverlapped
    } else {
        CompactionLevel::Final
    }
}

fn get_num_overlapped_files(
    start_level_files: Vec<&ParquetFile>,
    next_level_files: Vec<&ParquetFile>,
) -> usize {
    // min_time and max_time of files in start_level
    let (min_time, max_time) =
        start_level_files
            .iter()
            .fold((None, None), |(min_time, max_time), f| {
                let min_time = min_time
                    .map(|v: Timestamp| v.min(f.min_time))
                    .unwrap_or(f.min_time);
                let max_time = max_time
                    .map(|v: Timestamp| v.max(f.max_time))
                    .unwrap_or(f.max_time);
                (Some(min_time), Some(max_time))
            });

    // There must be values, otherwise panic
    let min_time = min_time.unwrap();
    let max_time = max_time.unwrap();

    // number of files in next level that overlap with files in start_level
    let count_overlapped = next_level_files
        .iter()
        .filter(|f| f.min_time <= max_time && f.max_time >= min_time)
        .count();

    count_overlapped
}

#[cfg(test)]
mod tests {
    use data_types::CompactionLevel;
    use iox_tests::ParquetFileBuilder;

    use crate::components::round_info_source::LevelBasedRoundInfo;

    #[test]
    fn test_too_many_small_files_to_compact() {
        // L0 files
        let f1 = ParquetFileBuilder::new(1)
            .with_time_range(0, 100)
            .with_compaction_level(CompactionLevel::Initial)
            .with_max_l0_created_at(0)
            .build();
        let f2 = ParquetFileBuilder::new(2)
            .with_time_range(0, 100)
            .with_compaction_level(CompactionLevel::Initial)
            .with_max_l0_created_at(2)
            .build();
        // non overlapping L1 file
        let f3 = ParquetFileBuilder::new(3)
            .with_time_range(101, 200)
            .with_compaction_level(CompactionLevel::FileNonOverlapped)
            .build();
        // overlapping L1 file
        let f4 = ParquetFileBuilder::new(4)
            .with_time_range(50, 150)
            .with_compaction_level(CompactionLevel::FileNonOverlapped)
            .build();

        // max 2 files per plan
        let round_info = LevelBasedRoundInfo {
            max_num_files_per_plan: 2,
            max_total_file_size_per_plan: 1000,
        };

        // f1 and f2 are not over limit
        assert!(!round_info
            .too_many_small_files_to_compact(&[f1.clone(), f2.clone()], CompactionLevel::Initial));
        // f1, f2 and f3 are not over limit
        assert!(!round_info.too_many_small_files_to_compact(
            &[f1.clone(), f2.clone(), f3.clone()],
            CompactionLevel::Initial
        ));
        // f1, f2 and f4 are over limit
        assert!(round_info.too_many_small_files_to_compact(
            &[f1.clone(), f2.clone(), f4.clone()],
            CompactionLevel::Initial
        ));
        // f1, f2, f3 and f4 are over limit
        assert!(
            round_info.too_many_small_files_to_compact(&[f1, f2, f3, f4], CompactionLevel::Initial)
        );
    }
}
