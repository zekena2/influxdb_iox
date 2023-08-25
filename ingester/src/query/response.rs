//! The response type returned from a query [`QueryExec::query_exec()`] call.
//!
//! [`QueryExec::query_exec()`]: super::QueryExec::query_exec()

use std::pin::Pin;

use futures::{Stream, StreamExt};

use super::partition_response::PartitionResponse;

/// Stream of partitions in this response.
pub(crate) struct PartitionStream(Pin<Box<dyn Stream<Item = PartitionResponse> + Send>>);

impl std::fmt::Debug for PartitionStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("PartitionStream").finish()
    }
}

impl PartitionStream {
    pub(crate) fn new<T>(s: T) -> Self
    where
        T: Stream<Item = PartitionResponse> + Send + 'static,
    {
        Self(s.boxed())
    }
}

/// A response stream wrapper for ingester query requests.
///
/// The data structure is constructed to allow lazy/streaming/pull-based data
/// sourcing.
#[derive(Debug)]
pub(crate) struct QueryResponse {
    /// Stream of partitions.
    partitions: PartitionStream,
}

impl QueryResponse {
    /// Make a response
    pub(crate) fn new(partitions: PartitionStream) -> Self {
        Self { partitions }
    }

    /// Return the stream of [`PartitionResponse`].
    pub(crate) fn into_partition_stream(self) -> impl Stream<Item = PartitionResponse> {
        self.partitions.0
    }
}
