//! Persistência durável com redb — transações ACID explícitas
//!
//! Diferenças fundamentais vs. sled:
//! - Transações: begin_write() → operações → commit() (atômico)
//! - Tables: definidas como constantes com tipos genéricos (key, value)
//! - No flush implícito: durabilidade garantida no commit()
//! - Uma única Database por processo (Arc<Database>)

use crate::types::*;
use redb::{Database, ReadableTable, TableDefinition};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

/// Erros de persistência
#[derive(Debug, thiserror::Error)]
pub enum PersistenceError {
    #[error("Redb database error: {0}")]
    RedbDatabase(#[from] redb::DatabaseError),
    #[error("Redb transaction error: {0}")]
    RedbTransaction(#[from] redb::TransactionError),
    #[error("Redb storage error: {0}")]
    RedbStorage(#[from] redb::StorageError),
    #[error("Redb table error: {0}")]
    RedbTable(#[from] redb::TableError),
    #[error("Redb commit error: {0}")]
    RedbCommit(#[from] redb::CommitError),
    #[error("Serialization error: {0}")]
    Serialization(#[from] bincode::Error),
    #[error("Key not found: {0}")]
    KeyNotFound(String),
    #[error("Corrupted data: {0}")]
    CorruptedData(String),
}

pub type PersistenceResult<T> = std::result::Result<T, PersistenceError>;

// ─── Table Definitions (redb requer constantes) ───────────────────────

const TABLE_ACQUISITION: TableDefinition<&str, &[u8]> = TableDefinition::new("acquisition");
const TABLE_EPOCHS: TableDefinition<&str, &[u8]> = TableDefinition::new("epochs");
const TABLE_LABELS: TableDefinition<&str, &[u8]> = TableDefinition::new("labels");
const TABLE_CONSENSUS: TableDefinition<&str, &[u8]> = TableDefinition::new("consensus");
const TABLE_AUDIT: TableDefinition<&str, &[u8]> = TableDefinition::new("audit");
const TABLE_META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");

/// Storage durável genérico sobre redb
#[derive(Debug)]
pub struct PersistentLog<T> {
    db: Arc<Database>,
    table: TableDefinition<&'static str, &'static [u8]>,
    _marker: std::marker::PhantomData<T>,
}

impl<T: Serialize + DeserializeOwned> PersistentLog<T> {
    pub fn new(db: Arc<Database>, table: TableDefinition<&'static str, &'static [u8]>) -> PersistenceResult<Self> {
        // Garante que a tabela existe (cria no primeiro write)
        let txn = db.begin_write()?;
        txn.open_table(table)?;
        txn.commit()?;

        info!(table = ?table.name(), "PersistentLog opened (redb)");
        Ok(Self {
            db,
            table,
            _marker: std::marker::PhantomData,
        })
    }

    /// Insere um valor em uma transação ACID
    pub fn insert(&self, key: &str, value: &T) -> PersistenceResult<()> {
        let bytes = bincode::serialize(value)?;
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(self.table)?;
            table.insert(key, bytes.as_slice())?;
        }
        txn.commit()?;
        debug!(key = key, table = ?self.table.name(), "Inserted and committed (ACID)");
        Ok(())
    }

    /// Batch insert em uma única transação
    pub fn insert_batch(&self, items: &[(&str, &T)]) -> PersistenceResult<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(self.table)?;
            for (key, value) in items {
                let bytes = bincode::serialize(value)?;
                table.insert(*key, bytes.as_slice())?;
            }
        }
        txn.commit()?;
        debug!(count = items.len(), table = ?self.table.name(), "Batch inserted and committed (ACID)");
        Ok(())
    }

    /// Recupera um valor
    pub fn get(&self, key: &str) -> PersistenceResult<Option<T>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(self.table)?;
        match table.get(key)? {
            Some(access) => {
                let bytes = access.value();
                let value = bincode::deserialize(bytes)?;
                Ok(Some(value))
            }
            None => Ok(None),
        }
    }

    /// Recupera todos os valores
    pub fn get_all(&self) -> PersistenceResult<Vec<(String, T)>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(self.table)?;
        let mut results = Vec::new();
        for item in table.iter()? {
            let (k, v) = item?;
            let key = k.value().to_string();
            let value = bincode::deserialize(v.value())?;
            results.push((key, value));
        }
        results.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(results)
    }

    /// Remove uma chave
    pub fn remove(&self, key: &str) -> PersistenceResult<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(self.table)?;
            table.remove(key)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Limpa a tabela
    pub fn clear(&self) -> PersistenceResult<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(self.table)?;
            table.retain(|_, _| false)?;
        }
        txn.commit()?;
        warn!(table = ?self.table.name(), "Table cleared");
        Ok(())
    }

    /// Número de entradas
    pub fn len(&self) -> PersistenceResult<usize> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(self.table)?;
        Ok(table.len()?.try_into().unwrap_or(usize::MAX))
    }

    pub fn is_empty(&self) -> PersistenceResult<bool> {
        Ok(self.len()? == 0)
    }
}

/// Metadados duráveis do protocolo
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolMeta {
    pub next_packet_id: PacketId,
    pub next_epoch_id: EpochId,
    pub hardware_active: bool,
    pub rem_window_start: Option<Timestamp>,
    pub rem_window_end: Option<Timestamp>,
    pub audit_chain_length: usize,
}

impl Default for ProtocolMeta {
    fn default() -> Self {
        Self {
            next_packet_id: 0,
            next_epoch_id: 0,
            hardware_active: true,
            rem_window_start: None,
            rem_window_end: None,
            audit_chain_length: 0,
        }
    }
}

/// Gerenciador de persistência unificado — UMA única Database
#[derive(Debug, Clone)]
pub struct PersistenceManager {
    db: Arc<Database>,
    pub acquisition: PersistentLog<DataPacket>,
    pub epochs: PersistentLog<Epoch>,
    pub labels: PersistentLog<RaterLabel>,
    pub consensus: PersistentLog<ConsensusResult>,
    pub audit: PersistentLog<AuditEntry>,
    pub meta: PersistentLog<ProtocolMeta>,
}

impl PersistenceManager {
    pub fn new<P: AsRef<Path>>(path: P) -> PersistenceResult<Self> {
        let db = Arc::new(Database::create(path)?);

        Ok(Self {
            db: db.clone(),
            acquisition: PersistentLog::new(db.clone(), TABLE_ACQUISITION)?,
            epochs: PersistentLog::new(db.clone(), TABLE_EPOCHS)?,
            labels: PersistentLog::new(db.clone(), TABLE_LABELS)?,
            consensus: PersistentLog::new(db.clone(), TABLE_CONSENSUS)?,
            audit: PersistentLog::new(db.clone(), TABLE_AUDIT)?,
            meta: PersistentLog::new(db, TABLE_META)?,
        })
    }

    pub fn save_meta(&self, meta: &ProtocolMeta) -> PersistenceResult<()> {
        self.meta.insert("state", meta)
    }

    pub fn load_meta(&self) -> PersistenceResult<ProtocolMeta> {
        match self.meta.get("state")? {
            Some(m) => Ok(m),
            None => Ok(ProtocolMeta::default()),
        }
    }
}