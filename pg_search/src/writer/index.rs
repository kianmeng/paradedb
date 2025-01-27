// Copyright (c) 2023-2024 Retake, Inc.
//
// This file is part of ParadeDB - Postgres for Search and Analytics
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program. If not, see <http://www.gnu.org/licenses/>.

use super::{Handler, IndexError, SearchFs, WriterDirectory, WriterRequest};
use crate::{
    index::SearchIndex,
    schema::{
        SearchDocument, SearchFieldConfig, SearchFieldName, SearchFieldType, SearchIndexSchema,
    },
};
use anyhow::{Context, Result};
use std::collections::{
    hash_map::Entry::{Occupied, Vacant},
    HashMap, HashSet,
};
use tantivy::{schema::Field, Index, IndexWriter};
use tracing::warn;

/// The entity that interfaces with Tantivy indexes.
pub struct Writer {
    /// Map of index directory path to Tantivy writer instance.
    tantivy_writers: HashMap<WriterDirectory, IndexWriter>,
    drop_requested: HashSet<WriterDirectory>,
}

impl Writer {
    pub fn new() -> Self {
        Self {
            tantivy_writers: HashMap::new(),
            drop_requested: HashSet::new(),
        }
    }

    /// Check the writer server cache for an existing IndexWriter. If it does not exist,
    /// then retrieve the SearchIndex and use it to create a new IndexWriter, caching it.
    fn get_writer(&mut self, directory: WriterDirectory) -> Result<&mut IndexWriter, IndexError> {
        match self.tantivy_writers.entry(directory.clone()) {
            Vacant(entry) => {
                Ok(entry.insert(SearchIndex::writer(&directory).map_err(|err| {
                    IndexError::GetWriterFailed(directory.clone(), err.to_string())
                })?))
            }
            Occupied(entry) => Ok(entry.into_mut()),
        }
    }

    fn insert(
        &mut self,
        directory: WriterDirectory,
        document: SearchDocument,
    ) -> Result<(), IndexError> {
        let writer = self.get_writer(directory)?;
        // Add the Tantivy document to the index.
        writer.add_document(document.into())?;

        Ok(())
    }

    fn delete(
        &mut self,
        directory: WriterDirectory,
        ctid_field: &Field,
        ctid_values: &[u64],
    ) -> Result<(), IndexError> {
        let writer = self.get_writer(directory)?;
        for ctid in ctid_values {
            let ctid_term = tantivy::Term::from_field_u64(*ctid_field, *ctid);
            writer.delete_term(ctid_term);
        }
        Ok(())
    }

    fn commit(&mut self, directory: WriterDirectory) -> Result<()> {
        if directory.exists()? {
            let writer = self.get_writer(directory.clone())?;
            writer
                .prepare_commit()
                .context("error preparing commit to tantivy index")?;
            writer
                .commit()
                .context("error committing to tantivy index")?;
        } else {
            warn!(?directory, "index directory unexepectedly does not exist");
        }

        if self.drop_requested.contains(&directory) {
            // The directory has been dropped in this transaction. Now that we're
            // committing, we must physically delete it.
            self.drop_requested.remove(&directory);
            self.drop_index_on_commit(directory)?
        }
        Ok(())
    }

    fn abort(&mut self, directory: WriterDirectory) -> Result<(), IndexError> {
        // If the transaction was aborted, we should roll back the writer to the last commit.
        // Otherwise, partialy written data could stick around for the next transaction.
        if let Some(writer) = self.tantivy_writers.get_mut(&directory) {
            writer.rollback()?;
        }

        // If the index was dropped in this transaction, we will not actually physically
        // delete the files, as we've aborted. Remove the directory from the
        // drop_requested set.
        self.drop_requested.remove(&directory);

        Ok(())
    }

    fn vacuum(&mut self, directory: WriterDirectory) -> Result<(), IndexError> {
        let writer = self.get_writer(directory)?;
        writer.garbage_collect_files().wait()?;
        Ok(())
    }

    pub fn create_index(
        &mut self,
        directory: WriterDirectory,
        fields: Vec<(SearchFieldName, SearchFieldConfig, SearchFieldType)>,
        uuid: String,
        key_field_index: usize,
    ) -> Result<()> {
        let schema = SearchIndexSchema::new(fields, key_field_index)?;

        let tantivy_dir_path = directory.tantivy_dir_path(true)?;
        let mut underlying_index = Index::builder()
            .schema(schema.schema.clone())
            .create_in_dir(tantivy_dir_path)
            .expect("failed to create index");

        SearchIndex::setup_tokenizers(&mut underlying_index, &schema);

        let new_self = SearchIndex {
            reader: SearchIndex::reader(&underlying_index)?,
            underlying_index,
            directory: directory.clone(),
            schema,
            uuid,
            is_dirty: false,
            is_pending_drop: false,
            is_pending_create: true,
        };

        // Serialize SearchIndex to disk so it can be initialized by other connections.
        new_self.directory.save_index(&new_self)?;
        Ok(())
    }

    /// Physically delete the Tantivy directory. This should only be called on commit.
    fn drop_index_on_commit(&mut self, directory: WriterDirectory) -> Result<(), IndexError> {
        if let Some(writer) = self.tantivy_writers.remove(&directory) {
            std::mem::drop(writer);
        };

        directory.remove()?;
        Ok(())
    }

    /// Handle a request to drop an index. We don't physically delete the files in this
    /// function in case the transaction is aborted. Instead, we mark the directory for
    /// deletion upon commit.
    fn drop_index(&mut self, directory: WriterDirectory) -> Result<(), IndexError> {
        self.drop_requested.insert(directory);
        Ok(())
    }
}

impl Handler<WriterRequest> for Writer {
    fn handle(&mut self, request: WriterRequest) -> Result<()> {
        match request {
            WriterRequest::Insert {
                directory,
                document,
            } => Ok(self.insert(directory, document)?),
            WriterRequest::Delete {
                directory,
                field,
                ctids,
            } => Ok(self.delete(directory, &field, &ctids)?),
            WriterRequest::CreateIndex {
                directory,
                fields,
                uuid,
                key_field_index,
            } => {
                self.create_index(directory, fields, uuid, key_field_index)?;
                Ok(())
            }
            WriterRequest::DropIndex { directory } => Ok(self.drop_index(directory)?),
            WriterRequest::Commit { directory } => Ok(self.commit(directory)?),
            WriterRequest::Abort { directory } => Ok(self.abort(directory)?),
            WriterRequest::Vacuum { directory } => Ok(self.vacuum(directory)?),
        }
    }
}
