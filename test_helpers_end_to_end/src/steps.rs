use crate::snapshot_comparison::Language;
use crate::{
    check_flight_error, run_influxql, run_sql, snapshot_comparison, try_run_influxql, try_run_sql,
    MiniCluster,
};
use arrow::record_batch::RecordBatch;
use arrow_util::assert_batches_sorted_eq;
use futures::future::BoxFuture;
use http::StatusCode;
use observability_deps::tracing::info;
use std::{path::PathBuf, time::Duration};
use test_helpers::assert_contains;

const MAX_QUERY_RETRY_TIME_SEC: u64 = 20;

/// Test harness for end to end tests that are comprised of several steps
pub struct StepTest<'a, S> {
    cluster: &'a mut MiniCluster,

    /// The test steps to perform
    steps: Box<dyn Iterator<Item = S> + Send + Sync + 'a>,
}

/// The test state that is passed to custom steps
pub struct StepTestState<'a> {
    /// The mini cluster
    cluster: &'a mut MiniCluster,

    /// How many Parquet files the catalog service knows about for the mini cluster's namespace,
    /// for tracking when persistence has happened. If this is `None`, we haven't ever checked with
    /// the catalog service.
    num_parquet_files: Option<usize>,
}

impl<'a> StepTestState<'a> {
    /// Get a reference to the step test state's cluster.
    #[must_use]
    pub fn cluster(&self) -> &&'a mut MiniCluster {
        &self.cluster
    }

    /// Get a reference to the step test state's cluster.
    #[must_use]
    pub fn cluster_mut(&mut self) -> &mut &'a mut MiniCluster {
        &mut self.cluster
    }

    /// Store the number of Parquet files the catalog has for the mini cluster's namespace.
    /// Call this before a write to be able to tell when a write has been persisted by checking for
    /// a change in this count.
    pub async fn record_num_parquet_files(&mut self) {
        let num_parquet_files = self.get_num_parquet_files().await;

        info!(
            "Recorded count of Parquet files for namespace {}: {num_parquet_files}",
            self.cluster.namespace()
        );
        self.num_parquet_files = Some(num_parquet_files);
    }

    /// Wait for a change (up to a timeout) in the number of Parquet files the catalog has for the
    /// mini cluster's namespacee since the last time the number of Parquet files was recorded,
    /// which indicates persistence has taken place.
    pub async fn wait_for_num_parquet_file_change(&mut self, expected_increase: usize) {
        let retry_duration = Duration::from_secs(MAX_QUERY_RETRY_TIME_SEC);
        let num_parquet_files = self.num_parquet_files.expect(
            "No previous number of Parquet files recorded! \
                Use `Step::RecordNumParquetFiles` before `Step::WaitForPersisted`.",
        );
        let expected_count = num_parquet_files + expected_increase;

        tokio::time::timeout(retry_duration, async move {
            let mut interval = tokio::time::interval(Duration::from_millis(1000));
            loop {
                let current_count = self.get_num_parquet_files().await;
                if current_count >= expected_count {
                    info!(
                        "Success; Parquet file count is now {current_count} \
                        which is at least {expected_count}"
                    );
                    // Reset the saved value to require recording before waiting again
                    self.num_parquet_files = None;
                    return;
                }
                info!(
                    "Retrying; Parquet file count is still {current_count} \
                    which is less than {expected_count}"
                );

                interval.tick().await;
            }
        })
        .await
        .expect("did not get additional Parquet files in the catalog");
    }

    /// Ask the catalog service how many Parquet files it has for the mini cluster's namespace.
    async fn get_num_parquet_files(&self) -> usize {
        let connection = self.cluster.router().router_grpc_connection();
        let mut catalog_client = influxdb_iox_client::catalog::Client::new(connection);

        catalog_client
            .get_parquet_files_by_namespace(self.cluster.namespace())
            .await
            .map(|parquet_files| parquet_files.len())
            .unwrap_or_default()
    }

    /// waits for `MAX_QUERY_RETRY_TIME_SEC` for the database to
    /// report exactly `expected` for its partition keys
    async fn wait_for_partition_keys(
        &self,
        table_name: &str,
        namespace_name: &Option<String>,
        expected: &[&str],
    ) {
        let retry_duration = Duration::from_secs(MAX_QUERY_RETRY_TIME_SEC);
        let partition_keys = tokio::time::timeout(retry_duration, async {
            loop {
                let mut partition_keys = self
                    .cluster()
                    .partition_keys(table_name, namespace_name.clone())
                    .await;
                partition_keys.sort();
                info!("====Read partition keys: {partition_keys:?}");

                if partition_keys == *expected {
                    return partition_keys;
                }
            }
        })
        .await
        .expect("did not get expected partition keys before timeout");

        assert_eq!(partition_keys, *expected);
    }
}

/// Function used for custom [`Step`]s.
///
/// It is an async function that receives a mutable reference to [`StepTestState`].
///
/// Example of creating one (note the `boxed()` call):
/// ```
/// use futures::FutureExt;
/// use futures::future::BoxFuture;
/// use test_helpers_end_to_end::{FCustom, StepTestState};
///
/// let custom_function: FCustom = Box::new(|state: &mut StepTestState| {
///   async move {
///     // access the cluster:
///     let cluster = state.cluster();
///     // Your potentially async code here
///   }.boxed()
/// });
/// ```
pub type FCustom = Box<dyn for<'b> Fn(&'b mut StepTestState) -> BoxFuture<'b, ()> + Send + Sync>;

/// Function to do custom validation on metrics. Expected to panic on validation failure.
pub type MetricsValidationFn = Box<dyn Fn(&mut StepTestState, String) + Send + Sync>;

/// Possible test steps that a test can perform
pub enum Step {
    /// Writes the specified line protocol to the `/api/v2/write`
    /// endpoint, assert the data was written successfully
    WriteLineProtocol(String),

    /// Writes the specified line protocol to the `/api/v2/write` endpoint; assert the request
    /// returned an error with the given code
    WriteLineProtocolExpectingError {
        line_protocol: String,
        expected_error_code: StatusCode,
    },

    /// Writes the specified line protocol to the `/api/v2/write` endpoint
    /// using the specified authorization header, assert the data was
    /// written successfully.
    WriteLineProtocolWithAuthorization {
        line_protocol: String,
        authorization: String,
    },

    /// Ask the catalog service how many Parquet files it has for this cluster's namespace. Do this
    /// before a write where you're interested in when the write has been persisted to Parquet;
    /// then after the write use `WaitForPersisted` to observe the change in the number of Parquet
    /// files from the value this step recorded.
    RecordNumParquetFiles,

    /// Query the catalog service for how many parquet files it has for this
    /// cluster's namespace, asserting the value matches expected.
    AssertNumParquetFiles { expected: usize },

    /// Ask the ingester to persist immediately through the persist service gRPC API
    Persist,

    /// Wait for all previously written data to be persisted by observing an increase in the number
    /// of Parquet files in the catalog as specified for this cluster's namespace.
    WaitForPersisted { expected_increase: usize },

    /// Set the namespace retention interval to a retention period,
    /// specified in ns relative to `now()`.  `None` represents infinite retention
    /// (i.e. never drop data).
    SetRetention(Option<i64>),

    /// Run one compaction operation and wait for it to finish, expecting success.
    Compact,

    /// Run one compaction operation and wait for it to finish, expecting an error that matches
    /// the specified message.
    CompactExpectingError { expected_message: String },

    /// Run a SQL query using the FlightSQL interface and verify that the
    /// results match the expected results using the
    /// `assert_batches_eq!` macro
    Query {
        sql: String,
        expected: Vec<&'static str>,
    },

    /// Read the SQL queries in the specified file and verify that the results match the expected
    /// results in the corresponding expected file
    QueryAndCompare {
        input_path: PathBuf,
        setup_name: String,
        contents: String,
    },

    /// Run a SQL query that's expected to fail using the FlightSQL interface and verify that the
    /// request returns the expected error code and message
    QueryExpectingError {
        sql: String,
        expected_error_code: tonic::Code,
        expected_message: String,
    },

    /// Run a SQL query using the FlightSQL interface authorized by the
    /// authorization header. Verify that the
    /// results match the expected results using the `assert_batches_eq!`
    /// macro
    QueryWithAuthorization {
        sql: String,
        authorization: String,
        expected: Vec<&'static str>,
    },

    /// Run a SQL query using the FlightSQL interface with the `iox-debug` header set.
    /// Verify that the
    /// results match the expected results using the `assert_batches_eq!`
    /// macro
    QueryWithDebug {
        sql: String,
        expected: Vec<&'static str>,
    },

    /// Run a SQL query using the FlightSQL interface, and then verifies
    /// the results using the provided validation function on the
    /// results.
    ///
    /// The validation function is expected to panic on validation
    /// failure.
    VerifiedQuery {
        sql: String,
        verify: Box<dyn Fn(Vec<RecordBatch>) + Send + Sync>,
    },

    /// Run an InfluxQL query using the FlightSQL interface and verify that the
    /// results match the expected results using the
    /// `assert_batches_eq!` macro
    InfluxQLQuery {
        query: String,
        expected: Vec<&'static str>,
    },

    /// Read the InfluxQL queries in the specified file and verify that the results match the
    /// expected results in the corresponding expected file
    InfluxQLQueryAndCompare {
        input_path: PathBuf,
        setup_name: String,
        contents: String,
    },

    /// Run an InfluxQL query that's expected to fail using the FlightSQL interface and verify that
    /// the request returns the expected error code and message
    InfluxQLExpectingError {
        query: String,
        expected_error_code: tonic::Code,
        expected_message: String,
    },

    /// Run an InfluxQL query using the FlightSQL interface including an
    /// authorization header. Verify that the results match the expected
    /// results using the `assert_batches_eq!` macro.
    InfluxQLQueryWithAuthorization {
        query: String,
        authorization: String,
        expected: Vec<&'static str>,
    },

    /// Read and verify partition keys for a given table
    PartitionKeys {
        table_name: String,
        namespace_name: Option<String>,
        expected: Vec<&'static str>,
    },

    /// Attempt to gracefully shutdown all running ingester instances.
    ///
    /// This step blocks until all ingesters have gracefully stopped, or at
    /// least [`GRACEFUL_SERVER_STOP_TIMEOUT`] elapses before they are killed.
    ///
    /// [`GRACEFUL_SERVER_STOP_TIMEOUT`]:
    ///     crate::server_fixture::GRACEFUL_SERVER_STOP_TIMEOUT
    GracefulStopIngesters,

    /// Retrieve the metrics and verify the results using the provided
    /// validation function.
    ///
    /// The validation function is expected to panic on validation
    /// failure.
    VerifiedMetrics(MetricsValidationFn),

    /// A custom step that can be used to implement special cases that
    /// are only used once.
    Custom(FCustom),
}

impl AsRef<Step> for Step {
    fn as_ref(&self) -> &Step {
        self
    }
}

impl<'a, S> StepTest<'a, S>
where
    S: AsRef<Step>,
{
    /// Create a new test that runs each `step`, in sequence, against
    /// `cluster` panic'ing if any step fails
    pub fn new<I>(cluster: &'a mut MiniCluster, steps: I) -> Self
    where
        I: IntoIterator<Item = S> + Send + Sync + 'a,
        <I as IntoIterator>::IntoIter: Send + Sync,
    {
        Self {
            cluster,
            steps: Box::new(steps.into_iter()),
        }
    }

    /// run the test.
    pub async fn run(self) {
        let Self { cluster, steps } = self;

        let mut state = StepTestState {
            cluster,
            num_parquet_files: Default::default(),
        };

        for (i, step) in steps.enumerate() {
            info!("**** Begin step {} *****", i);
            match step.as_ref() {
                Step::WriteLineProtocol(line_protocol) => {
                    info!(
                        "====Begin writing line protocol to v2 HTTP API:\n{}",
                        line_protocol
                    );
                    let response = state.cluster.write_to_router(line_protocol, None).await;
                    let status = response.status();
                    let body = hyper::body::to_bytes(response.into_body())
                        .await
                        .expect("reading response body");
                    assert!(
                        status == StatusCode::NO_CONTENT,
                        "Invalid response code while writing line protocol:\n\nLine Protocol:\n{}\n\nExpected Status: {}\nActual Status: {}\n\nBody:\n{:?}",
                        line_protocol,
                        StatusCode::NO_CONTENT,
                        status,
                        body,
                    );
                    info!("====Done writing line protocol");
                }
                Step::WriteLineProtocolExpectingError {
                    line_protocol,
                    expected_error_code,
                } => {
                    info!(
                        "====Begin writing line protocol expecting error to v2 HTTP API:\n{}",
                        line_protocol
                    );
                    let response = state.cluster.write_to_router(line_protocol, None).await;
                    assert_eq!(response.status(), *expected_error_code);
                    info!("====Done writing line protocol expecting error");
                }
                Step::WriteLineProtocolWithAuthorization {
                    line_protocol,
                    authorization,
                } => {
                    info!(
                        "====Begin writing line protocol (authenticated) to v2 HTTP API:\n{}",
                        line_protocol
                    );
                    let response = state
                        .cluster
                        .write_to_router(line_protocol, Some(authorization))
                        .await;
                    assert_eq!(response.status(), StatusCode::NO_CONTENT);
                    info!("====Done writing line protocol");
                }
                // Get the current number of Parquet files in the cluster's namespace before
                // starting a new write so we can observe a change when waiting for persistence.
                Step::RecordNumParquetFiles => {
                    state.record_num_parquet_files().await;
                }
                Step::AssertNumParquetFiles { expected } => {
                    let have_files = state.get_num_parquet_files().await;
                    assert_eq!(have_files, *expected);
                }
                // Ask the ingesters to persist immediately through the persist service gRPC API
                Step::Persist => {
                    state.cluster().persist_ingesters().await;
                }
                Step::WaitForPersisted { expected_increase } => {
                    info!("====Begin waiting for a change in the number of Parquet files");
                    state
                        .wait_for_num_parquet_file_change(*expected_increase)
                        .await;
                    info!("====Done waiting for a change in the number of Parquet files");
                }
                Step::Compact => {
                    info!("====Begin running compaction");
                    state.cluster.run_compaction().unwrap();
                    info!("====Done running compaction");
                }
                Step::CompactExpectingError { expected_message } => {
                    info!("====Begin running compaction expected to error");
                    let err = state.cluster.run_compaction().unwrap_err();

                    assert_contains!(err, expected_message);

                    info!("====Done running");
                }

                Step::SetRetention(retention_period_ns) => {
                    info!("====Begin setting retention period to {retention_period_ns:?}");
                    let namespace = state.cluster().namespace();
                    let router_connection = state.cluster().router().router_grpc_connection();
                    let mut client = influxdb_iox_client::namespace::Client::new(router_connection);
                    client
                        .update_namespace_retention(namespace, *retention_period_ns)
                        .await
                        .expect("Error updating retention period");
                    info!("====Done setting retention period");
                }
                Step::Query { sql, expected } => {
                    info!("====Begin running SQL query: {}", sql);
                    // run query
                    let (mut batches, schema) = run_sql(
                        sql,
                        state.cluster.namespace(),
                        state.cluster.querier().querier_grpc_connection(),
                        None,
                        false,
                    )
                    .await;
                    batches.push(RecordBatch::new_empty(schema));
                    assert_batches_sorted_eq!(expected, &batches);
                    info!("====Done running");
                }
                Step::QueryAndCompare {
                    input_path,
                    setup_name,
                    contents,
                } => {
                    info!(
                        "====Begin running SQL queries in file {}",
                        input_path.display()
                    );
                    snapshot_comparison::run(
                        state.cluster,
                        input_path.into(),
                        setup_name.into(),
                        contents.into(),
                        Language::Sql,
                    )
                    .await
                    .unwrap();
                    info!("====Done running SQL queries");
                }
                Step::QueryExpectingError {
                    sql,
                    expected_error_code,
                    expected_message,
                } => {
                    info!("====Begin running SQL query expected to error: {}", sql);

                    let err = try_run_sql(
                        sql,
                        state.cluster().namespace(),
                        state.cluster().querier().querier_grpc_connection(),
                        None,
                        false,
                    )
                    .await
                    .unwrap_err();

                    check_flight_error(err, *expected_error_code, Some(expected_message));

                    info!("====Done running");
                }
                Step::QueryWithAuthorization {
                    sql,
                    authorization,
                    expected,
                } => {
                    info!("====Begin running SQL query (authenticated): {}", sql);
                    // run query
                    let (mut batches, schema) = run_sql(
                        sql,
                        state.cluster.namespace(),
                        state.cluster().querier().querier_grpc_connection(),
                        Some(authorization.as_str()),
                        false,
                    )
                    .await;
                    batches.push(RecordBatch::new_empty(schema));
                    assert_batches_sorted_eq!(expected, &batches);
                    info!("====Done running");
                }
                Step::QueryWithDebug { sql, expected } => {
                    info!("====Begin running SQL query (w/ iox-debug): {}", sql);
                    // run query
                    let (mut batches, schema) = run_sql(
                        sql,
                        state.cluster.namespace(),
                        state.cluster().querier().querier_grpc_connection(),
                        None,
                        true,
                    )
                    .await;
                    batches.push(RecordBatch::new_empty(schema));
                    assert_batches_sorted_eq!(expected, &batches);
                    info!("====Done running");
                }
                Step::VerifiedQuery { sql, verify } => {
                    info!("====Begin running SQL verified query: {}", sql);
                    // run query
                    let (batches, _schema) = run_sql(
                        sql,
                        state.cluster.namespace(),
                        state.cluster.querier().querier_grpc_connection(),
                        None,
                        true,
                    )
                    .await;
                    verify(batches);
                    info!("====Done running");
                }
                Step::InfluxQLQuery { query, expected } => {
                    info!("====Begin running InfluxQL query: {}", query);
                    // run query
                    let (mut batches, schema) = run_influxql(
                        query,
                        state.cluster.namespace(),
                        state.cluster.querier().querier_grpc_connection(),
                        None,
                    )
                    .await;
                    batches.push(RecordBatch::new_empty(schema));
                    assert_batches_sorted_eq!(expected, &batches);
                    info!("====Done running");
                }
                Step::InfluxQLQueryAndCompare {
                    input_path,
                    setup_name,
                    contents,
                } => {
                    info!(
                        "====Begin running InfluxQL queries in file {}",
                        input_path.display()
                    );
                    snapshot_comparison::run(
                        state.cluster,
                        input_path.into(),
                        setup_name.into(),
                        contents.into(),
                        Language::InfluxQL,
                    )
                    .await
                    .unwrap();
                    info!("====Done running InfluxQL queries");
                }
                Step::InfluxQLExpectingError {
                    query,
                    expected_error_code,
                    expected_message,
                } => {
                    info!(
                        "====Begin running InfluxQL query expected to error: {}",
                        query
                    );

                    let err = try_run_influxql(
                        query,
                        state.cluster().namespace(),
                        state.cluster().querier().querier_grpc_connection(),
                        None,
                    )
                    .await
                    .unwrap_err();

                    check_flight_error(err, *expected_error_code, Some(expected_message));

                    info!("====Done running");
                }
                Step::InfluxQLQueryWithAuthorization {
                    query,
                    authorization,
                    expected,
                } => {
                    info!("====Begin running InfluxQL query: {}", query);
                    // run query
                    let (mut batches, schema) = run_influxql(
                        query,
                        state.cluster.namespace(),
                        state.cluster.querier().querier_grpc_connection(),
                        Some(authorization),
                    )
                    .await;
                    batches.push(RecordBatch::new_empty(schema));
                    assert_batches_sorted_eq!(expected, &batches);
                    info!("====Done running");
                }
                Step::PartitionKeys {
                    table_name,
                    namespace_name,
                    expected,
                } => {
                    info!("====Persist ingesters to ensure catalog partition records exist");
                    state
                        .cluster()
                        .persist_ingesters_by_namespace(namespace_name.clone())
                        .await;

                    info!("====Begin reading partition keys for table: {}", table_name);
                    state
                        .wait_for_partition_keys(table_name, namespace_name, expected)
                        .await;
                    info!("====Done reading partition keys");
                }
                Step::GracefulStopIngesters => {
                    info!("====Gracefully stop all ingesters");

                    state.cluster_mut().gracefully_stop_ingesters();
                }
                Step::VerifiedMetrics(verify) => {
                    info!("====Begin validating metrics");

                    let cluster = state.cluster();
                    let http_base = cluster.router().router_http_base();
                    let url = format!("{http_base}/metrics");

                    let client = reqwest::Client::new();
                    let metrics = client.get(&url).send().await.unwrap().text().await.unwrap();

                    verify(&mut state, metrics);

                    info!("====Done validating metrics");
                }
                Step::Custom(f) => {
                    info!("====Begin custom step");
                    f(&mut state).await;
                    info!("====Done custom step");
                }
            }
        }
    }
}
