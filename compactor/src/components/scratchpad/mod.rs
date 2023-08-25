use std::fmt::{Debug, Display};
use std::sync::Arc;

use async_trait::async_trait;
use parquet_file::ParquetFilePath;
use uuid::Uuid;

pub mod noop;
pub mod prod;
mod util;

#[cfg(test)]
mod test_util;

/// Create a [`Scratchpad`] for use as intermediate storage
pub trait ScratchpadGen: Debug + Display + Send + Sync {
    fn pad(&self) -> Arc<dyn Scratchpad>;
}

/// An intermediate in-memory store (can be a disk later if we want)
/// to stage all inputs and outputs of the compaction. The reasons
/// are:
///
/// **fewer IO ops:** DataFusion's streaming IO requires slightly more IO
/// requests (at least 2 per file) due to the way it is optimized to
/// read as little as possible. It first reads the metadata and then
/// decides which content to fetch. In the compaction case this is
/// (esp. w/o delete predicates) EVERYTHING. So in contrast to the
/// querier, there is no advantage of this approach. In contrary this
/// easily adds 100ms latency to every single input file.
///
/// **less traffic**: For divide&conquer partitions (i.e. when we need
/// to run multiple compaction steps to deal with them) it is kinda
/// pointless to upload an intermediate result just to download it
/// again. The scratchpad avoids that.
///
/// **higher throughput**: We want to limit the number of concurrent
/// DataFusion jobs because we don't wanna blow up the whole process
/// by having too much in-flight arrow data at the same time. However
/// while we perform the actual computation, we were waiting for
/// object store IO. This was limiting our throughput substantially.
///
/// **shadow mode**: De-coupling the stores in this way makes it easier
/// to implement compactor: shadow mode #6645.  Shadow mode relies on
/// leaving the compaction output in the scratchpad so
/// `clean_written_from_scratchpad` is a no-op for shadow mode.
///
/// Note that we assume here that the input parquet files are WAY
/// SMALLER than the uncompressed Arrow data during compaction itself.
#[async_trait]
pub trait Scratchpad: Debug + Send + Sync + 'static {
    fn uuids(&self, files: &[ParquetFilePath]) -> Vec<Uuid>;
    async fn load_to_scratchpad(&self, files: &[ParquetFilePath]) -> Vec<Uuid>;
    async fn make_public(&self, files: &[ParquetFilePath]) -> Vec<Uuid>;
    async fn clean_from_scratchpad(&self, files: &[ParquetFilePath]);
    async fn clean_written_from_scratchpad(&self, files: &[ParquetFilePath]);
    async fn clean(&self);
}
