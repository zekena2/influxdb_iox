use super::QuerierNamespace;
use crate::{
    cache::namespace::CachedNamespace, create_ingester_connection_for_testing, QuerierCatalogCache,
};
use data_types::TableId;
use datafusion_util::config::register_iox_object_store;
use iox_query::exec::ExecutorType;
use iox_tests::TestNamespace;
use std::sync::Arc;
use tokio::runtime::Handle;

/// Create [`QuerierNamespace`] for testing.
pub async fn querier_namespace(ns: &Arc<TestNamespace>) -> QuerierNamespace {
    let mut repos = ns.catalog.catalog.repositories().await;
    let tables = repos
        .tables()
        .list_by_namespace_id(ns.namespace.id)
        .await
        .unwrap();
    let columns = repos
        .columns()
        .list_by_namespace_id(ns.namespace.id)
        .await
        .unwrap();
    let cached_ns = Arc::new(CachedNamespace::new(ns.namespace.clone(), tables, columns));

    let catalog_cache = Arc::new(QuerierCatalogCache::new_testing(
        ns.catalog.catalog(),
        ns.catalog.time_provider(),
        ns.catalog.metric_registry(),
        ns.catalog.object_store(),
        &Handle::current(),
    ));

    // add cached store
    let parquet_store = catalog_cache.parquet_store();
    let runtime_env = ns
        .catalog
        .exec()
        .new_context(ExecutorType::Query)
        .inner()
        .runtime_env();
    register_iox_object_store(
        runtime_env,
        parquet_store.id(),
        Arc::clone(parquet_store.object_store()),
    );

    QuerierNamespace::new_testing(
        catalog_cache,
        ns.catalog.metric_registry(),
        ns.namespace.name.clone().into(),
        cached_ns,
        ns.catalog.exec(),
        Some(create_ingester_connection_for_testing()),
    )
}

/// Given some tests create parquet files without an ingester to
/// signal the need for a cache refresh, this function, explictly
/// trigger the "refresh cache logic"
pub fn clear_parquet_cache(querier_namespace: &QuerierNamespace, table_id: TableId) {
    querier_namespace
        .catalog_cache()
        .parquet_file()
        .expire(table_id);
}
