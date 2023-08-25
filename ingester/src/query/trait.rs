use std::{fmt::Debug, ops::Deref, sync::Arc};

use async_trait::async_trait;
use data_types::{NamespaceId, TableId};
use predicate::Predicate;
use thiserror::Error;
use trace::span::Span;

use super::projection::OwnedProjection;

#[derive(Debug, Error)]
#[allow(missing_copy_implementations)]
pub(crate) enum QueryError {
    #[error("namespace id {0} not found")]
    NamespaceNotFound(NamespaceId),

    #[error("table id {1} not found in namespace id {0}")]
    TableNotFound(NamespaceId, TableId),
}

#[async_trait]
pub(crate) trait QueryExec: Send + Sync + Debug {
    type Response: Send + Debug;

    async fn query_exec(
        &self,
        namespace_id: NamespaceId,
        table_id: TableId,
        projection: OwnedProjection,
        span: Option<Span>,
        predicate: Option<Predicate>,
    ) -> Result<Self::Response, QueryError>;
}

#[async_trait]
impl<T> QueryExec for Arc<T>
where
    T: QueryExec,
{
    type Response = T::Response;

    async fn query_exec(
        &self,
        namespace_id: NamespaceId,
        table_id: TableId,
        projection: OwnedProjection,
        span: Option<Span>,
        predicate: Option<Predicate>,
    ) -> Result<Self::Response, QueryError> {
        self.deref()
            .query_exec(namespace_id, table_id, projection, span, predicate)
            .await
    }
}
