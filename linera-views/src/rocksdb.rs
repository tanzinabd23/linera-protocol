// Copyright (c) Zefchain Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    hash::HashingContext,
    views::{
        AppendOnlyLogOperations, CollectionOperations, Context, MapOperations, QueueOperations,
        RegisterOperations, ScopedOperations, ViewError,
    },
};
use async_trait::async_trait;
use serde::{de::DeserializeOwned, Serialize};
use std::{ops::Range, sync::Arc};
use thiserror::Error;
use tokio::sync::OwnedMutexGuard;

#[derive(Debug, Clone)]
pub struct RocksdbContext<E> {
    db: Arc<rocksdb::DB>,
    lock: Arc<OwnedMutexGuard<()>>,
    base_key: Vec<u8>,
    extra: E,
}

impl<E> RocksdbContext<E> {
    pub fn new(
        db: Arc<rocksdb::DB>,
        lock: OwnedMutexGuard<()>,
        base_key: Vec<u8>,
        extra: E,
    ) -> Self {
        Self {
            db,
            lock: Arc::new(lock),
            base_key,
            extra,
        }
    }

    fn derive_key<I: Serialize>(&self, index: &I) -> Vec<u8> {
        let mut key = self.base_key.clone();
        bcs::serialize_into(&mut key, index).expect("serialization should not fail");
        assert!(
            key.len() > self.base_key.len(),
            "Empty indices are not allowed"
        );
        key
    }

    async fn read_key<V: DeserializeOwned>(
        &self,
        key: &[u8],
    ) -> Result<Option<V>, RocksdbViewError> {
        let db = self.db.clone();
        let key = key.to_vec();
        let value = tokio::task::spawn_blocking(move || db.get(&key)).await??;
        match &value {
            None => Ok(None),
            Some(bytes) => Ok(Some(bcs::from_bytes(bytes)?)),
        }
    }

    async fn write_key<V: Serialize>(&self, key: &[u8], value: &V) -> Result<(), RocksdbViewError> {
        let db = self.db.clone();
        let key = key.to_vec();
        let bytes = bcs::to_bytes(value)?;
        tokio::task::spawn_blocking(move || db.put(&key, bytes)).await??;
        Ok(())
    }

    async fn delete_key(&self, key: &[u8]) -> Result<(), RocksdbViewError> {
        let db = self.db.clone();
        let key = key.to_vec();
        tokio::task::spawn_blocking(move || db.delete(&key)).await??;
        Ok(())
    }

    async fn find_keys_with_strict_prefix(
        &self,
        key_prefix: &[u8],
    ) -> Result<Vec<Vec<u8>>, RocksdbViewError> {
        let db = self.db.clone();
        let key = key_prefix.to_vec();
        let keys = tokio::task::spawn_blocking(move || {
            let mut iter = db.raw_iterator();
            let mut keys = Vec::new();
            iter.seek(&key);
            if matches!(iter.key(), Some(k) if k == key) {
                // Skip the key it self
                iter.next();
            }
            while matches!(iter.key(), Some(k) if k.starts_with(&key)) {
                keys.push(iter.key().unwrap().to_vec());
                iter.next();
            }
            keys
        })
        .await?;
        Ok(keys)
    }
}

#[async_trait]
impl<E> Context for RocksdbContext<E>
where
    E: Clone + Send + Sync,
{
    type Extra = E;
    type Error = RocksdbViewError;

    async fn erase(&mut self) -> Result<(), Self::Error> {
        self.delete_key(&self.base_key).await?;
        Ok(())
    }

    fn extra(&self) -> &E {
        &self.extra
    }
}

#[async_trait]
impl<E> ScopedOperations for RocksdbContext<E>
where
    E: Clone + Send + Sync,
{
    fn clone_with_scope(&self, index: u64) -> Self {
        Self {
            db: self.db.clone(),
            lock: self.lock.clone(),
            base_key: self.derive_key(&index),
            extra: self.extra.clone(),
        }
    }
}

#[async_trait]
impl<E, T> RegisterOperations<T> for RocksdbContext<E>
where
    T: Default + Serialize + DeserializeOwned + Send + Sync + 'static,
    E: Clone + Send + Sync,
{
    async fn get(&mut self) -> Result<T, RocksdbViewError> {
        let value = self.read_key(&self.base_key).await?.unwrap_or_default();
        Ok(value)
    }

    async fn set(&mut self, value: T) -> Result<(), RocksdbViewError> {
        self.write_key(&self.base_key, &value).await?;
        Ok(())
    }
}

#[async_trait]
impl<E, T> AppendOnlyLogOperations<T> for RocksdbContext<E>
where
    T: Serialize + DeserializeOwned + Send + Sync + 'static,
    E: Clone + Send + Sync,
{
    async fn count(&mut self) -> Result<usize, RocksdbViewError> {
        let count = self.read_key(&self.base_key).await?.unwrap_or_default();
        Ok(count)
    }

    async fn get(&mut self, index: usize) -> Result<Option<T>, RocksdbViewError> {
        self.db.read_key(&self.derive_key(&index)).await
    }

    async fn read(&mut self, range: std::ops::Range<usize>) -> Result<Vec<T>, RocksdbViewError> {
        let mut values = Vec::new();
        for i in range {
            match self.read_key(&self.derive_key(&i)).await? {
                None => {
                    return Ok(values);
                }
                Some(value) => {
                    values.push(value);
                }
            }
        }
        Ok(values)
    }

    async fn append(&mut self, values: Vec<T>) -> Result<(), RocksdbViewError> {
        let mut i = AppendOnlyLogOperations::<T>::count(self).await?;
        for value in values {
            self.write_key(&self.derive_key(&i), &value).await?;
            i += 1;
        }
        self.write_key(&self.base_key, &i).await?;
        Ok(())
    }
}

#[async_trait]
impl<E, T> QueueOperations<T> for RocksdbContext<E>
where
    T: Serialize + DeserializeOwned + Send + Sync + 'static,
    E: Clone + Send + Sync,
{
    async fn indices(&mut self) -> Result<Range<usize>, Self::Error> {
        let range = self.read_key(&self.base_key).await?.unwrap_or_default();
        Ok(range)
    }

    async fn get(&mut self, index: usize) -> Result<Option<T>, Self::Error> {
        Ok(self.read_key(&self.derive_key(&index)).await?)
    }

    async fn read(&mut self, range: Range<usize>) -> Result<Vec<T>, Self::Error> {
        let mut values = Vec::new();
        for i in range {
            match self.read_key(&self.derive_key(&i)).await? {
                None => {
                    return Ok(values);
                }
                Some(value) => {
                    values.push(value);
                }
            }
        }
        Ok(values)
    }

    async fn delete_front(&mut self, count: usize) -> Result<(), Self::Error> {
        let mut range: Range<usize> = self.read_key(&self.base_key).await?.unwrap_or_default();
        for i in 0..count {
            self.delete_key(&self.derive_key(&i)).await?;
        }
        range.start += count;
        self.write_key(&self.base_key, &range).await
    }

    async fn append_back(&mut self, values: Vec<T>) -> Result<(), Self::Error> {
        let mut range: Range<usize> = self.read_key(&self.base_key).await?.unwrap_or_default();
        for value in values {
            self.write_key(&self.derive_key(&range.end), &value).await?;
            range.end += 1;
        }
        self.write_key(&self.base_key, &range).await
    }
}

#[async_trait]
impl<E, I, V> MapOperations<I, V> for RocksdbContext<E>
where
    I: Eq + Ord + Send + Sync + Serialize + DeserializeOwned + Clone + 'static,
    V: Serialize + DeserializeOwned + Send + Sync + 'static,
    E: Clone + Send + Sync,
{
    async fn get(&mut self, index: &I) -> Result<Option<V>, RocksdbViewError> {
        Ok(self.read_key(&self.derive_key(index)).await?)
    }

    async fn insert(&mut self, index: I, value: V) -> Result<(), RocksdbViewError> {
        self.write_key(&self.derive_key(&index), &value).await?;
        Ok(())
    }

    async fn remove(&mut self, index: I) -> Result<(), RocksdbViewError> {
        self.delete_key(&self.derive_key(&index)).await?;
        Ok(())
    }

    async fn indices(&mut self) -> Result<Vec<I>, RocksdbViewError> {
        let len = self.base_key.len();
        let mut keys = Vec::new();
        for key in self.find_keys_with_strict_prefix(&self.base_key).await? {
            keys.push(bcs::from_bytes(&key[len..])?);
        }
        Ok(keys)
    }
}

#[async_trait]
impl<E, I> CollectionOperations<I> for RocksdbContext<E>
where
    I: serde::Serialize + serde::de::DeserializeOwned + Send + Sync,
    E: Clone + Send + Sync,
{
    fn clone_with_scope(&self, index: &I) -> Self {
        Self {
            db: self.db.clone(),
            lock: self.lock.clone(),
            base_key: self.derive_key(index),
            extra: self.extra.clone(),
        }
    }

    async fn indices(&mut self) -> Result<Vec<I>, Self::Error> {
        let len = self.base_key.len();
        let mut keys = Vec::new();
        for bytes in self.find_keys_with_strict_prefix(&self.base_key).await? {
            match bcs::from_bytes(&bytes[len..]) {
                Ok(key) => {
                    keys.push(key);
                }
                Err(bcs::Error::RemainingInput) => {
                    // Skip composite keys corresponding to objects in subviews.
                    // FIXME: this makes this function slower than linear time!
                }
                Err(e) => {
                    return Err(e.into());
                }
            }
        }
        Ok(keys)
    }
}

impl<E> HashingContext for RocksdbContext<E>
where
    E: Clone + Send + Sync,
{
    type Hasher = sha2::Sha512;
}

#[derive(Error, Debug)]
pub enum RocksdbViewError {
    #[error("View error: {0}")]
    ViewError(#[from] ViewError),

    #[error("tokio join error: {0}")]
    Io(#[from] tokio::task::JoinError),

    #[error("IO error: {0}")]
    TokioJoinError(#[from] std::io::Error),

    #[error("Rocksdb error: {0}")]
    Rocksdb(#[from] rocksdb::Error),

    #[error("BCS error: {0}")]
    Bcs(#[from] bcs::Error),
}
