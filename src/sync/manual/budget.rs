//! Whole-attempt resource and deadline accounting for encrypted vault synchronization.

use std::time::Duration;

use crate::sync::{
    EncryptedObject, ListCost, ListLimits, MAX_VAULT_PAYLOAD_BYTES, ObjectCondition,
    ObjectDeleteResult, ObjectKey, ObjectMetadata, ObjectWriteResult, VaultDeadline, VaultError,
    VaultTransport,
};

const MAX_LISTED_OBJECTS: usize = 10_000;
const MAX_READ_REQUESTS: usize = 20_000;
const MAX_LIST_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_SCANNED_COLLECTIONS: usize = 10_000;
const MAX_SCANNED_RESOURCES: usize = 10_000;
const MAX_SYNC_DURATION: Duration = Duration::from_secs(5 * 60);

pub struct ManualSyncBudget {
    consumed: usize,
    requests: usize,
    listed_objects: usize,
    list_response_bytes: usize,
    scanned_collections: usize,
    scanned_resources: usize,
    deadline: VaultDeadline,
}

impl Default for ManualSyncBudget {
    fn default() -> Self {
        Self::with_deadline(VaultDeadline::from_now(MAX_SYNC_DURATION))
    }
}

impl ManualSyncBudget {
    pub fn with_deadline(deadline: VaultDeadline) -> Self {
        Self {
            consumed: 0,
            requests: 0,
            listed_objects: 0,
            list_response_bytes: 0,
            scanned_collections: 0,
            scanned_resources: 0,
            deadline,
        }
    }

    pub fn check_deadline(&self) -> Result<(), VaultError> {
        self.deadline.check()
    }

    #[cfg(test)]
    pub(crate) fn consumed_requests(&self) -> usize {
        self.requests
    }

    pub fn get<T: VaultTransport + ?Sized>(
        &mut self,
        transport: &T,
        key: &ObjectKey,
        max_bytes: usize,
    ) -> Result<Option<(EncryptedObject, ObjectMetadata)>, VaultError> {
        self.check_deadline()?;
        self.consume_requests(1)?;
        let result = transport.get_with_deadline(key, max_bytes, self.deadline);
        self.check_deadline()?;
        let result = result?;
        if let Some((object, metadata)) = &result {
            let actual_length: u64 = object
                .as_bytes()
                .len()
                .try_into()
                .map_err(|_| VaultError::PayloadTooLarge)?;
            self.consume_ciphertext(metadata.content_length.max(actual_length))?;
        }
        Ok(result)
    }

    pub(super) fn list<T: VaultTransport + ?Sized>(
        &mut self,
        transport: &T,
        prefix: &ObjectKey,
    ) -> Result<Vec<ObjectMetadata>, VaultError> {
        self.check_deadline()?;
        let limits = self.list_limits()?;
        let outcome = transport.list_with_limits(prefix, limits, self.deadline);
        self.check_deadline()?;
        let outcome = outcome?;
        outcome.validate(limits)?;
        self.consume_list_cost(outcome.cost)?;
        Ok(outcome.objects)
    }

    pub(super) fn put<T: VaultTransport + ?Sized>(
        &mut self,
        transport: &T,
        key: &ObjectKey,
        object: &EncryptedObject,
        condition: ObjectCondition,
    ) -> Result<ObjectWriteResult, VaultError> {
        self.check_deadline()?;
        let result = transport.put_with_deadline(key, object, condition, self.deadline);
        self.check_deadline()?;
        result
    }

    pub(super) fn delete<T: VaultTransport + ?Sized>(
        &mut self,
        transport: &T,
        key: &ObjectKey,
        expected_etag: &str,
    ) -> Result<ObjectDeleteResult, VaultError> {
        self.check_deadline()?;
        let result = transport.delete_with_deadline(key, expected_etag, self.deadline);
        self.check_deadline()?;
        result
    }

    fn consume_ciphertext(&mut self, bytes: u64) -> Result<(), VaultError> {
        let bytes: usize = bytes.try_into().map_err(|_| VaultError::PayloadTooLarge)?;
        self.consume(bytes)
    }

    fn consume(&mut self, bytes: usize) -> Result<(), VaultError> {
        self.consumed = self
            .consumed
            .checked_add(bytes)
            .ok_or(VaultError::PayloadTooLarge)?;
        if self.consumed > MAX_VAULT_PAYLOAD_BYTES {
            return Err(VaultError::PayloadTooLarge);
        }
        Ok(())
    }

    fn consume_requests(&mut self, requests: usize) -> Result<(), VaultError> {
        self.requests = self
            .requests
            .checked_add(requests)
            .ok_or(VaultError::ResourceLimitExceeded)?;
        if self.requests > MAX_READ_REQUESTS {
            return Err(VaultError::ResourceLimitExceeded);
        }
        Ok(())
    }

    fn list_limits(&self) -> Result<ListLimits, VaultError> {
        Ok(ListLimits {
            requests: remaining(MAX_READ_REQUESTS, self.requests)?,
            response_bytes: remaining(MAX_LIST_RESPONSE_BYTES, self.list_response_bytes)?,
            scanned_collections: remaining(MAX_SCANNED_COLLECTIONS, self.scanned_collections)?,
            scanned_resources: remaining(MAX_SCANNED_RESOURCES, self.scanned_resources)?,
            returned_objects: remaining(MAX_LISTED_OBJECTS, self.listed_objects)?,
        })
    }

    fn consume_list_cost(&mut self, cost: ListCost) -> Result<(), VaultError> {
        self.consume_requests(cost.requests)?;
        self.list_response_bytes = checked_total(
            self.list_response_bytes,
            cost.response_bytes,
            MAX_LIST_RESPONSE_BYTES,
        )?;
        self.scanned_collections = checked_total(
            self.scanned_collections,
            cost.scanned_collections,
            MAX_SCANNED_COLLECTIONS,
        )?;
        self.scanned_resources = checked_total(
            self.scanned_resources,
            cost.scanned_resources,
            MAX_SCANNED_RESOURCES,
        )?;
        self.listed_objects = self
            .listed_objects
            .checked_add(cost.returned_objects)
            .ok_or(VaultError::ResourceLimitExceeded)?;
        if self.listed_objects > MAX_LISTED_OBJECTS {
            return Err(VaultError::ResourceLimitExceeded);
        }
        Ok(())
    }
}

fn remaining(maximum: usize, consumed: usize) -> Result<usize, VaultError> {
    maximum
        .checked_sub(consumed)
        .filter(|remaining| *remaining > 0)
        .ok_or(VaultError::ResourceLimitExceeded)
}

fn checked_total(current: usize, added: usize, maximum: usize) -> Result<usize, VaultError> {
    current
        .checked_add(added)
        .filter(|total| *total <= maximum)
        .ok_or(VaultError::ResourceLimitExceeded)
}
