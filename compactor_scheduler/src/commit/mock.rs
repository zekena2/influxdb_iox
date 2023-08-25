use std::{
    fmt::Display,
    sync::atomic::{AtomicI64, Ordering},
};

use async_trait::async_trait;
use data_types::{CompactionLevel, ParquetFile, ParquetFileId, ParquetFileParams, PartitionId};
use parking_lot::Mutex;

use super::{Commit, Error};

#[derive(Debug, PartialEq, Eq, Clone)]
pub(crate) struct CommitHistoryEntry {
    pub(crate) partition_id: PartitionId,
    pub(crate) delete: Vec<ParquetFile>,
    pub(crate) upgrade: Vec<ParquetFile>,
    pub(crate) created: Vec<ParquetFile>,
    pub(crate) target_level: CompactionLevel,
}

#[derive(Debug, Default)]
pub(crate) struct MockCommit {
    history: Mutex<Vec<CommitHistoryEntry>>,
    id_counter: AtomicI64,
}

impl MockCommit {
    pub(crate) fn new() -> Self {
        Self {
            history: Default::default(),
            id_counter: AtomicI64::new(1000),
        }
    }

    #[cfg(test)]
    pub(crate) fn history(&self) -> Vec<CommitHistoryEntry> {
        self.history.lock().clone()
    }
}

impl Display for MockCommit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "mock")
    }
}

#[async_trait]
impl Commit for MockCommit {
    async fn commit(
        &self,
        partition_id: PartitionId,
        delete: &[ParquetFile],
        upgrade: &[ParquetFile],
        create: &[ParquetFileParams],
        target_level: CompactionLevel,
    ) -> Result<Vec<ParquetFileId>, Error> {
        let (created, ids): (Vec<_>, Vec<_>) = create
            .iter()
            .map(|params| {
                let id = ParquetFileId::new(self.id_counter.fetch_add(1, Ordering::SeqCst));
                let created = ParquetFile::from_params(params.clone(), id);
                (created, id)
            })
            .unzip();

        self.history.lock().push(CommitHistoryEntry {
            partition_id,
            delete: delete.to_vec(),
            upgrade: upgrade.to_vec(),
            created,
            target_level,
        });

        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use iox_tests::{partition_identifier, ParquetFileBuilder};

    #[test]
    fn test_display() {
        assert_eq!(MockCommit::new().to_string(), "mock");
    }

    #[tokio::test]
    async fn test_commit() {
        let commit = MockCommit::new();

        let partition_id_1 = PartitionId::new(1);
        let transition_partition_id_1 = partition_identifier(1);
        let partition_id_2 = PartitionId::new(2);
        let transition_partition_id_2 = partition_identifier(2);

        let existing_1 = ParquetFileBuilder::new(1).build();
        let existing_2 = ParquetFileBuilder::new(2).build();
        let existing_3 = ParquetFileBuilder::new(3).build();
        let existing_4 = ParquetFileBuilder::new(4).build();
        let existing_5 = ParquetFileBuilder::new(5).build();
        let existing_6 = ParquetFileBuilder::new(6).build();
        let existing_7 = ParquetFileBuilder::new(7).build();
        let existing_8 = ParquetFileBuilder::new(8).build();

        let created_1_1 = ParquetFileBuilder::new(1000)
            .with_partition(transition_partition_id_1.clone())
            .build();
        let created_1_2 = ParquetFileBuilder::new(1001)
            .with_partition(transition_partition_id_1.clone())
            .build();
        let created_1_3 = ParquetFileBuilder::new(1003)
            .with_partition(transition_partition_id_1)
            .build();
        let created_2_1 = ParquetFileBuilder::new(1002)
            .with_partition(transition_partition_id_2)
            .build();

        let ids = commit
            .commit(
                partition_id_1,
                &[existing_1.clone(), existing_2.clone()],
                &[existing_3.clone(), existing_4.clone()],
                &[created_1_1.clone().into(), created_1_2.clone().into()],
                CompactionLevel::FileNonOverlapped,
            )
            .await;
        assert_matches!(
            ids,
            Ok(res) if res == vec![ParquetFileId::new(1000), ParquetFileId::new(1001)]
        );

        let ids = commit
            .commit(
                partition_id_2,
                &[existing_3.clone()],
                &[],
                &[created_2_1.clone().into()],
                CompactionLevel::Final,
            )
            .await;
        assert_matches!(
            ids,
            Ok(res) if res == vec![ParquetFileId::new(1002)]
        );

        let ids = commit
            .commit(
                partition_id_1,
                &[existing_5.clone(), existing_6.clone(), existing_7.clone()],
                &[],
                &[created_1_3.clone().into()],
                CompactionLevel::FileNonOverlapped,
            )
            .await;
        assert_matches!(
            ids,
            Ok(res) if res == vec![ParquetFileId::new(1003)]
        );

        // simulate fill implosion of the file (this may happen w/ delete predicates)
        let ids = commit
            .commit(
                partition_id_1,
                &[existing_8.clone()],
                &[],
                &[],
                CompactionLevel::FileNonOverlapped,
            )
            .await;
        assert_matches!(
            ids,
            Ok(res) if res == vec![]
        );

        assert_eq!(
            commit.history(),
            vec![
                CommitHistoryEntry {
                    partition_id: partition_id_1,
                    delete: vec![existing_1, existing_2],
                    upgrade: vec![existing_3.clone(), existing_4.clone()],
                    created: vec![created_1_1, created_1_2],
                    target_level: CompactionLevel::FileNonOverlapped,
                },
                CommitHistoryEntry {
                    partition_id: partition_id_2,
                    delete: vec![existing_3],
                    upgrade: vec![],
                    created: vec![created_2_1],
                    target_level: CompactionLevel::Final,
                },
                CommitHistoryEntry {
                    partition_id: partition_id_1,
                    delete: vec![existing_5, existing_6, existing_7,],
                    upgrade: vec![],
                    created: vec![created_1_3],
                    target_level: CompactionLevel::FileNonOverlapped,
                },
                CommitHistoryEntry {
                    partition_id: partition_id_1,
                    delete: vec![existing_8],
                    upgrade: vec![],
                    created: vec![],
                    target_level: CompactionLevel::FileNonOverlapped,
                },
            ]
        )
    }
}
