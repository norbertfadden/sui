// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0
#![allow(dead_code)]

use crate::{
    analytics_metrics::AnalyticsMetrics,
    csv_writer::CSVWriter,
    errors::AnalyticsIndexerError,
    tables::{
        CheckpointEntry, EventEntry, InputObjectKind, ObjectEntry, ObjectStatus, OwnerType,
        TransactionEntry, TransactionObjectEntry,
    },
    writer::CheckpointWriter,
    AnalyticsIndexerConfig,
};
use fastcrypto::{
    encoding::{Base64, Encoding},
    traits::EncodeDecodeBase64,
};
use std::collections::BTreeSet;
use sui_indexer::framework::interface::Handler;
use sui_rest_api::{CheckpointData, CheckpointTransaction};
use sui_types::{
    base_types::ObjectID,
    digests::TransactionDigest,
    effects::{TransactionEffects, TransactionEffectsAPI, TransactionEvents},
    event::Event,
    messages_checkpoint::{CertifiedCheckpointSummary, CheckpointSummary},
    object::{Object, Owner},
    transaction::{TransactionData, TransactionDataAPI},
};
use tracing::info;

// The main processor for analytics indexer.
pub struct AnalyticsProcessor {
    config: AnalyticsIndexerConfig,
    metrics: AnalyticsMetrics,
    writer: CheckpointWriter,
    data_writer: Option<CSVWriter>,
    start_checkpoint_to_commit: u64,
}

// Main callback from the indexer framework.
// All processing starts here.
#[async_trait::async_trait]
impl Handler for AnalyticsProcessor {
    fn name(&self) -> &str {
        "checkpoint-analytics-processor"
    }

    async fn process_checkpoint(&mut self, checkpoint_data: &CheckpointData) -> anyhow::Result<()> {
        let CheckpointData {
            checkpoint_summary,
            transactions: checkpoint_transactions,
            ..
        } = checkpoint_data;
        // get epoch id, checkpoint sequence number and timestamp, those are important
        // indexes when operating on data
        let epoch: u64 = checkpoint_summary.epoch();
        let checkpoint_num: u64 = *checkpoint_summary.sequence_number();
        let timestamp: u64 = checkpoint_summary.data().timestamp_ms;
        info!("Processing checkpoint {checkpoint_num}, eqpoch {epoch}, timestamp {timestamp}");

        self.check_writer(checkpoint_num).unwrap_or_else(|e| {
            panic!(
                "Cannot create table writer when processing checkpoint: {:?}",
                e
            )
        });

        self.process_checkpoints(checkpoint_summary, checkpoint_transactions);
        for checkpoint_transaction in checkpoint_transactions {
            let digest = checkpoint_transaction.transaction.digest();
            self.process_transaction(
                epoch,
                checkpoint_num,
                timestamp,
                checkpoint_transaction,
                &checkpoint_transaction.effects,
            );
            if let Some(events) = &checkpoint_transaction.events {
                self.process_events(epoch, checkpoint_num, digest, timestamp, events);
            }
        }
        self.save_locally(checkpoint_num);
        self.upload_to_datastore(checkpoint_num);
        Ok(())
    }
}

impl AnalyticsProcessor {
    pub fn new(config: AnalyticsIndexerConfig, metrics: AnalyticsMetrics) -> Self {
        Self {
            config,
            metrics,
            writer: CheckpointWriter::new(),
            data_writer: None,
            start_checkpoint_to_commit: 0,
        }
    }

    // Overall checkpoint data.
    fn process_checkpoints(
        &mut self,
        summary: &CertifiedCheckpointSummary,
        checkpoint_transactions: &Vec<CheckpointTransaction>,
    ) {
        self.metrics.total_checkpoint_received.inc();

        let CheckpointSummary {
            epoch,
            sequence_number,
            network_total_transactions,
            previous_digest,
            epoch_rolling_gas_cost_summary,
            timestamp_ms,
            end_of_epoch_data,
            ..
        } = summary.data();

        let total_gas_cost = epoch_rolling_gas_cost_summary.computation_cost as i64
            + epoch_rolling_gas_cost_summary.storage_cost as i64
            - epoch_rolling_gas_cost_summary.storage_rebate as i64;
        let total_transaction_blocks = checkpoint_transactions.len() as u64;
        let mut total_transactions: u64 = 0;
        let mut total_successful_transaction_blocks: u64 = 0;
        let mut total_successful_transactions: u64 = 0;
        for checkpoint_transaction in checkpoint_transactions {
            let txn_data = checkpoint_transaction.transaction.transaction_data();
            let cmds = txn_data.kind().num_commands() as u64;
            total_transactions += cmds;
            if checkpoint_transaction.effects.status().is_ok() {
                total_successful_transaction_blocks += 1;
                total_successful_transactions += cmds;
            }
        }

        let checkpoint_entry = CheckpointEntry {
            sequence_number: *sequence_number,
            checkpoint_digest: summary.digest().base58_encode(),
            previous_checkpoint_digest: previous_digest.map(|d| d.base58_encode()),
            epoch: *epoch,
            end_of_epoch: end_of_epoch_data.is_some(),
            total_gas_cost,
            total_computation_cost: epoch_rolling_gas_cost_summary.computation_cost,
            total_storage_cost: epoch_rolling_gas_cost_summary.storage_cost,
            total_storage_rebate: epoch_rolling_gas_cost_summary.storage_rebate,
            total_transaction_blocks,
            total_transactions,
            total_successful_transaction_blocks,
            total_successful_transactions,
            network_total_transaction: *network_total_transactions,
            timestamp_ms: *timestamp_ms,
            validator_signature: summary.auth_sig().signature.encode_base64(),
        };
        self.writer.write_checkpoint(checkpoint_entry);
    }

    // Transaction data. Also process transaction objects and objects.
    fn process_transaction(
        &mut self,
        epoch: u64,
        checkpoint: u64,
        timestamp_ms: u64,
        checkpoint_transaction: &CheckpointTransaction,
        effects: &TransactionEffects,
    ) {
        self.metrics.total_transaction_received.inc();

        // transaction
        let transaction = &checkpoint_transaction.transaction;
        let txn_data = transaction.transaction_data();
        let input_object_tracker = InputObjectTracker::new(txn_data);
        let object_status_tracker = ObjectStatusTracker::new(effects);
        let gas_object = effects.gas_object();
        let gas_summary = effects.gas_cost_summary();
        let move_calls = txn_data.move_calls();
        let packages: BTreeSet<_> = move_calls
            .iter()
            .map(|(package, _, _)| package.to_canonical_string())
            .collect();
        let packages = packages
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(",");
        let transaction_digest = transaction.digest().base58_encode();

        let transaction_entry = TransactionEntry {
            transaction_digest: transaction_digest.clone(),
            checkpoint,
            epoch,
            timestamp_ms,

            sender: txn_data.sender().to_string(),
            transaction_kind: txn_data.kind().name().to_owned(),
            transaction_count: txn_data.kind().num_commands() as u64,
            execution_success: effects.status().is_ok(),
            input: txn_data
                .input_objects()
                .expect("Input objects must be valid")
                .len() as u64,
            shared_input: txn_data.shared_input_objects().len() as u64,
            gas_coins: txn_data.gas().len() as u64,
            created: effects.created().len() as u64,
            mutated: (effects.mutated().len() + effects.unwrapped().len()) as u64,
            deleted: (effects.deleted().len()
                + effects.unwrapped_then_deleted().len()
                + effects.wrapped().len()) as u64,
            move_calls: move_calls.len() as u64,
            packages,
            gas_object_id: gas_object.0 .0.to_string(),
            gas_object_sequence: gas_object.0 .1.value(),
            gas_object_digest: gas_object.0 .2.to_string(),
            gas_budget: txn_data.gas_budget(),
            total_gas_cost: gas_summary.net_gas_usage(),
            computation_cost: gas_summary.computation_cost,
            storage_cost: gas_summary.storage_cost,
            storage_rebate: gas_summary.storage_rebate,
            non_refundable_storage_fee: gas_summary.non_refundable_storage_fee,

            gas_price: txn_data.gas_price(),

            raw_transaction: Base64::encode(bcs::to_bytes(&txn_data).unwrap()),
        };
        self.writer.write_transaction(transaction_entry);

        // transaction objects
        txn_data
            .input_objects()
            .expect("Input objects must be valid")
            .iter()
            .map(|object| (object.object_id(), object.version().map(|v| v.value())))
            .for_each(|(object_id, version)| {
                self.process_transaction_object(
                    epoch,
                    checkpoint,
                    timestamp_ms,
                    transaction_digest.clone(),
                    &object_id,
                    version,
                    &input_object_tracker,
                    &object_status_tracker,
                )
            });
        checkpoint_transaction
            .output_objects
            .iter()
            .map(|object| (object.id(), Some(object.version().value())))
            .for_each(|(object_id, version)| {
                self.process_transaction_object(
                    epoch,
                    checkpoint,
                    timestamp_ms,
                    transaction_digest.clone(),
                    &object_id,
                    version,
                    &input_object_tracker,
                    &object_status_tracker,
                )
            });

        // objects
        checkpoint_transaction
            .output_objects
            .iter()
            .for_each(|object| {
                self.process_object(
                    epoch,
                    checkpoint,
                    timestamp_ms,
                    object,
                    &object_status_tracker,
                )
            });
    }

    // Events data. Only called if there are events in the transaction.
    fn process_events(
        &mut self,
        epoch: u64,
        checkpoint: u64,
        digest: &TransactionDigest,
        timestamp_ms: u64,
        events: &TransactionEvents,
    ) {
        for (idx, event) in events.data.iter().enumerate() {
            let Event {
                package_id,
                transaction_module,
                sender,
                type_,
                contents,
            } = event;
            let entry = EventEntry {
                transaction_digest: digest.base58_encode(),
                event_index: idx as u64,
                checkpoint,
                epoch,
                timestamp_ms,
                sender: sender.to_string(),
                package: package_id.to_string(),
                module: transaction_module.to_string(),
                event_type: type_.to_string(),
                bcs: Base64::encode(contents.clone()),
            };
            self.writer.write_events(entry);
        }
    }

    // Object data. Only called if there are objects in the transaction.
    // Responsible to build the live object table.
    fn process_object(
        &mut self,
        epoch: u64,
        checkpoint: u64,
        timestamp_ms: u64,
        object: &Object,
        object_status_tracker: &ObjectStatusTracker,
    ) {
        let move_obj_opt = object.data.try_as_move();
        let has_public_transfer = move_obj_opt
            .map(|o| o.has_public_transfer())
            .unwrap_or(false);
        let object_type = move_obj_opt.map(|o| o.type_().to_string());
        let object_id = object.id();
        let entry = ObjectEntry {
            object_id: object_id.to_string(),
            digest: object.digest().to_string(),
            version: object.version().value(),
            type_: object_type,
            checkpoint,
            epoch,
            timestamp_ms,
            owner_type: get_owner_type(object),
            owner_address: get_owner_address(object),
            object_status: object_status_tracker
                .get_object_status(&object_id)
                .expect("Object must be in output objects"),
            initial_shared_version: initial_shared_version(object),
            previous_transaction: object.previous_transaction.base58_encode(),
            has_public_transfer,
            storage_rebate: object.storage_rebate,
            bcs: Base64::encode(bcs::to_bytes(object).unwrap()),
        };
        self.writer.write_objects(entry);
    }

    // Transaction object data.
    // Builds a view of the object in input and output of a transaction.
    fn process_transaction_object(
        &mut self,
        epoch: u64,
        checkpoint: u64,
        timestamp_ms: u64,
        transaction_digest: String,
        object_id: &ObjectID,
        version: Option<u64>,
        input_object_tracker: &InputObjectTracker,
        object_status_tracker: &ObjectStatusTracker,
    ) {
        let entry = TransactionObjectEntry {
            object_id: object_id.to_string(),
            version,
            transaction_digest,
            checkpoint,
            epoch,
            timestamp_ms,
            input_kind: input_object_tracker.get_input_object_kind(object_id),
            object_status: object_status_tracker.get_object_status(object_id),
        };
        self.writer.write_transaction_object(entry);
    }

    // Ensure a writer is in place
    fn check_writer(&mut self, checkpoint: u64) -> Result<(), AnalyticsIndexerError> {
        if self.data_writer.is_none() {
            info!("Create csv writer at checkpoint {checkpoint}");
            self.start_checkpoint_to_commit = checkpoint;
            self.data_writer = Some(CSVWriter::new(&self.config, checkpoint)?);
        }
        Ok(())
    }

    // Write entries to files if so desired
    fn save_locally(&mut self, _checkpoint: u64) {
        info!("Save data locally");
        // check some condition if we do not want to flush every checkpoint
        self.writer.flush(self.data_writer.as_mut().unwrap());
    }

    // Upload to datastore if desired.
    // Rotate writer and notify upload thread.
    fn upload_to_datastore(&mut self, checkpoint: u64) {
        if checkpoint - self.start_checkpoint_to_commit > self.config.checkpoint_interval {
            info!("Upload to datastore and rotate writer");
            let _data_writer = self.data_writer.take().unwrap();
            // notify upload thread
        }
    }
}

fn get_owner_type(object: &Object) -> OwnerType {
    match object.owner {
        Owner::AddressOwner(_) => OwnerType::AddressOwner,
        Owner::ObjectOwner(_) => OwnerType::ObjectOwner,
        Owner::Shared { .. } => OwnerType::Shared,
        Owner::Immutable => OwnerType::Immutable,
    }
}

fn get_owner_address(object: &Object) -> Option<String> {
    match object.owner {
        Owner::AddressOwner(address) => Some(address.to_string()),
        Owner::ObjectOwner(address) => Some(address.to_string()),
        Owner::Shared { .. } => None,
        Owner::Immutable => None,
    }
}

fn initial_shared_version(object: &Object) -> Option<u64> {
    match object.owner {
        Owner::Shared {
            initial_shared_version,
        } => Some(initial_shared_version.value()),
        _ => None,
    }
}

// Helper class to track object status.
// Build sets of object ids for created, mutated and deleted objects as reported
// in the transaction effects.
struct ObjectStatusTracker {
    created: BTreeSet<ObjectID>,
    mutated: BTreeSet<ObjectID>,
    deleted: BTreeSet<ObjectID>,
}

impl ObjectStatusTracker {
    fn new(effects: &TransactionEffects) -> Self {
        let created: BTreeSet<ObjectID> = effects
            .created()
            .iter()
            .map(|(obj_ref, _)| obj_ref.0)
            .collect();
        let mutated: BTreeSet<ObjectID> = effects
            .mutated()
            .iter()
            .chain(effects.unwrapped().iter())
            .map(|(obj_ref, _)| obj_ref.0)
            .collect();
        let deleted: BTreeSet<ObjectID> = effects
            .deleted()
            .iter()
            .chain(effects.unwrapped_then_deleted().iter())
            .chain(effects.wrapped().iter())
            .map(|obj_ref| obj_ref.0)
            .collect();
        Self {
            created,
            mutated,
            deleted,
        }
    }

    fn get_object_status(&self, object_id: &ObjectID) -> Option<ObjectStatus> {
        if self.mutated.contains(object_id) {
            Some(ObjectStatus::Created)
        } else if self.deleted.contains(object_id) {
            Some(ObjectStatus::Mutated)
        } else if self.created.contains(object_id) {
            Some(ObjectStatus::Deleted)
        } else {
            None
        }
    }
}

// Helper class to track input object kind.
// Build sets of object ids for input, shared input and gas coin objects as defined
// in the transaction data.
// Input objects include coins and shared.
struct InputObjectTracker {
    shared: BTreeSet<ObjectID>,
    coins: BTreeSet<ObjectID>,
    input: BTreeSet<ObjectID>,
}

impl InputObjectTracker {
    fn new(txn_data: &TransactionData) -> Self {
        let shared: BTreeSet<ObjectID> = txn_data
            .shared_input_objects()
            .iter()
            .map(|shared_io| shared_io.id())
            .collect();
        let coins: BTreeSet<ObjectID> = txn_data.gas().iter().map(|obj_ref| obj_ref.0).collect();
        let input: BTreeSet<ObjectID> = txn_data
            .input_objects()
            .expect("Input objects must be valid")
            .iter()
            .map(|io_kind| io_kind.object_id())
            .collect();
        Self {
            shared,
            coins,
            input,
        }
    }

    fn get_input_object_kind(&self, object_id: &ObjectID) -> Option<InputObjectKind> {
        if self.coins.contains(object_id) {
            Some(InputObjectKind::GasCoin)
        } else if self.shared.contains(object_id) {
            Some(InputObjectKind::SharedInput)
        } else if self.input.contains(object_id) {
            Some(InputObjectKind::Input)
        } else {
            None
        }
    }
}