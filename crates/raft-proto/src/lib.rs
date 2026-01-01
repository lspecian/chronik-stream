// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.
// Vendored from tikv/raft-rs v0.7.0, modified for prost 0.12 compatibility.
//
// This module provides protobuf-style accessor methods (get_*, set_*, mut_*, take_*)
// on top of prost-generated types for compatibility with the raft crate.

#![allow(clippy::field_reassign_with_default)]

mod confchange;
mod confstate;

pub use crate::confchange::{
    new_conf_change_single, parse_conf_change, stringify_conf_change, ConfChangeI,
};
pub use crate::confstate::conf_state_eq;

/// The prost-generated protobuf types
#[allow(clippy::all)]
pub mod eraftpb {
    include!(concat!(env!("OUT_DIR"), "/eraftpb.rs"));

    use bytes::Bytes;

    // ============ Entry accessors ============
    impl Entry {
        #[inline]
        pub fn get_entry_type(&self) -> EntryType {
            EntryType::try_from(self.entry_type).unwrap_or(EntryType::EntryNormal)
        }

        #[inline]
        pub fn get_term(&self) -> u64 {
            self.term
        }

        #[inline]
        pub fn set_term(&mut self, v: u64) {
            self.term = v;
        }

        #[inline]
        pub fn get_index(&self) -> u64 {
            self.index
        }

        #[inline]
        pub fn set_index(&mut self, v: u64) {
            self.index = v;
        }

        #[inline]
        pub fn get_data(&self) -> &[u8] {
            &self.data
        }

        #[inline]
        pub fn set_data(&mut self, v: Bytes) {
            self.data = v;
        }

        #[inline]
        pub fn take_data(&mut self) -> Bytes {
            std::mem::take(&mut self.data)
        }

        #[inline]
        pub fn get_context(&self) -> &[u8] {
            &self.context
        }

        #[inline]
        pub fn set_context(&mut self, v: Bytes) {
            self.context = v;
        }

        #[inline]
        pub fn take_context(&mut self) -> Bytes {
            std::mem::take(&mut self.context)
        }

        #[inline]
        pub fn get_sync_log(&self) -> bool {
            self.sync_log
        }

        #[inline]
        pub fn set_sync_log(&mut self, v: bool) {
            self.sync_log = v;
        }

        /// Compute the encoded size of this entry (for size limiting)
        #[inline]
        pub fn compute_size(&self) -> u32 {
            prost::Message::encoded_len(self) as u32
        }
    }

    // ============ Snapshot accessors ============
    impl Snapshot {
        #[inline]
        pub fn get_data(&self) -> &[u8] {
            &self.data
        }

        #[inline]
        pub fn set_data(&mut self, v: Bytes) {
            self.data = v;
        }

        #[inline]
        pub fn take_data(&mut self) -> Bytes {
            std::mem::take(&mut self.data)
        }

        #[inline]
        pub fn has_metadata(&self) -> bool {
            self.metadata.is_some()
        }

        #[inline]
        pub fn get_metadata(&self) -> &SnapshotMetadata {
            self.metadata.as_ref().unwrap_or(&DEFAULT_SNAPSHOT_METADATA)
        }

        #[inline]
        pub fn mut_metadata(&mut self) -> &mut SnapshotMetadata {
            self.metadata.get_or_insert_with(Default::default)
        }

        #[inline]
        pub fn set_metadata(&mut self, v: SnapshotMetadata) {
            self.metadata = Some(v);
        }

        #[inline]
        pub fn take_metadata(&mut self) -> SnapshotMetadata {
            self.metadata.take().unwrap_or_default()
        }

        #[inline]
        pub fn is_empty(&self) -> bool {
            self.get_metadata().index == 0
        }
    }

    lazy_static::lazy_static! {
        static ref DEFAULT_SNAPSHOT_METADATA: SnapshotMetadata = SnapshotMetadata::default();
        static ref DEFAULT_SNAPSHOT: Snapshot = Snapshot::default();
        static ref DEFAULT_CONF_STATE: ConfState = ConfState::default();
    }

    // ============ SnapshotMetadata accessors ============
    impl SnapshotMetadata {
        #[inline]
        pub fn has_conf_state(&self) -> bool {
            self.conf_state.is_some()
        }

        #[inline]
        pub fn get_conf_state(&self) -> &ConfState {
            self.conf_state.as_ref().unwrap_or(&DEFAULT_CONF_STATE)
        }

        #[inline]
        pub fn mut_conf_state(&mut self) -> &mut ConfState {
            self.conf_state.get_or_insert_with(Default::default)
        }

        #[inline]
        pub fn set_conf_state(&mut self, v: ConfState) {
            self.conf_state = Some(v);
        }

        #[inline]
        pub fn take_conf_state(&mut self) -> ConfState {
            self.conf_state.take().unwrap_or_default()
        }

        #[inline]
        pub fn get_index(&self) -> u64 {
            self.index
        }

        #[inline]
        pub fn set_index(&mut self, v: u64) {
            self.index = v;
        }

        #[inline]
        pub fn get_term(&self) -> u64 {
            self.term
        }

        #[inline]
        pub fn set_term(&mut self, v: u64) {
            self.term = v;
        }
    }

    // ============ Message accessors ============
    impl Message {
        #[inline]
        pub fn get_msg_type(&self) -> MessageType {
            MessageType::try_from(self.msg_type).unwrap_or(MessageType::MsgHup)
        }

        #[inline]
        pub fn get_to(&self) -> u64 {
            self.to
        }

        #[inline]
        pub fn set_to(&mut self, v: u64) {
            self.to = v;
        }

        #[inline]
        pub fn get_from(&self) -> u64 {
            self.from
        }

        #[inline]
        pub fn set_from(&mut self, v: u64) {
            self.from = v;
        }

        #[inline]
        pub fn get_term(&self) -> u64 {
            self.term
        }

        #[inline]
        pub fn set_term(&mut self, v: u64) {
            self.term = v;
        }

        #[inline]
        pub fn get_log_term(&self) -> u64 {
            self.log_term
        }

        #[inline]
        pub fn set_log_term(&mut self, v: u64) {
            self.log_term = v;
        }

        #[inline]
        pub fn get_index(&self) -> u64 {
            self.index
        }

        #[inline]
        pub fn set_index(&mut self, v: u64) {
            self.index = v;
        }

        #[inline]
        pub fn get_entries(&self) -> &[Entry] {
            &self.entries
        }

        #[inline]
        pub fn mut_entries(&mut self) -> &mut Vec<Entry> {
            &mut self.entries
        }

        #[inline]
        pub fn set_entries(&mut self, v: Vec<Entry>) {
            self.entries = v;
        }

        #[inline]
        pub fn take_entries(&mut self) -> Vec<Entry> {
            std::mem::take(&mut self.entries)
        }

        #[inline]
        pub fn get_commit(&self) -> u64 {
            self.commit
        }

        #[inline]
        pub fn set_commit(&mut self, v: u64) {
            self.commit = v;
        }

        #[inline]
        pub fn get_commit_term(&self) -> u64 {
            self.commit_term
        }

        #[inline]
        pub fn set_commit_term(&mut self, v: u64) {
            self.commit_term = v;
        }

        #[inline]
        pub fn has_snapshot(&self) -> bool {
            self.snapshot.is_some()
        }

        #[inline]
        pub fn get_snapshot(&self) -> &Snapshot {
            self.snapshot.as_ref().unwrap_or(&DEFAULT_SNAPSHOT)
        }

        #[inline]
        pub fn mut_snapshot(&mut self) -> &mut Snapshot {
            self.snapshot.get_or_insert_with(Default::default)
        }

        #[inline]
        pub fn set_snapshot(&mut self, v: Snapshot) {
            self.snapshot = Some(v);
        }

        #[inline]
        pub fn take_snapshot(&mut self) -> Snapshot {
            self.snapshot.take().unwrap_or_default()
        }

        #[inline]
        pub fn get_request_snapshot(&self) -> u64 {
            self.request_snapshot
        }

        #[inline]
        pub fn set_request_snapshot(&mut self, v: u64) {
            self.request_snapshot = v;
        }

        #[inline]
        pub fn get_reject(&self) -> bool {
            self.reject
        }

        #[inline]
        pub fn set_reject(&mut self, v: bool) {
            self.reject = v;
        }

        #[inline]
        pub fn get_reject_hint(&self) -> u64 {
            self.reject_hint
        }

        #[inline]
        pub fn set_reject_hint(&mut self, v: u64) {
            self.reject_hint = v;
        }

        #[inline]
        pub fn get_context(&self) -> &[u8] {
            &self.context
        }

        #[inline]
        pub fn set_context(&mut self, v: Bytes) {
            self.context = v;
        }

        #[inline]
        pub fn take_context(&mut self) -> Bytes {
            std::mem::take(&mut self.context)
        }

        #[inline]
        pub fn get_priority(&self) -> i64 {
            self.priority
        }

        #[inline]
        pub fn set_priority(&mut self, v: i64) {
            self.priority = v;
        }
    }

    // ============ HardState accessors ============
    impl HardState {
        #[inline]
        pub fn get_term(&self) -> u64 {
            self.term
        }

        #[inline]
        pub fn set_term(&mut self, v: u64) {
            self.term = v;
        }

        #[inline]
        pub fn get_vote(&self) -> u64 {
            self.vote
        }

        #[inline]
        pub fn set_vote(&mut self, v: u64) {
            self.vote = v;
        }

        #[inline]
        pub fn get_commit(&self) -> u64 {
            self.commit
        }

        #[inline]
        pub fn set_commit(&mut self, v: u64) {
            self.commit = v;
        }
    }

    // ============ ConfState accessors ============
    impl ConfState {
        #[inline]
        pub fn get_voters(&self) -> &[u64] {
            &self.voters
        }

        #[inline]
        pub fn mut_voters(&mut self) -> &mut Vec<u64> {
            &mut self.voters
        }

        #[inline]
        pub fn set_voters(&mut self, v: Vec<u64>) {
            self.voters = v;
        }

        #[inline]
        pub fn get_learners(&self) -> &[u64] {
            &self.learners
        }

        #[inline]
        pub fn mut_learners(&mut self) -> &mut Vec<u64> {
            &mut self.learners
        }

        #[inline]
        pub fn set_learners(&mut self, v: Vec<u64>) {
            self.learners = v;
        }

        #[inline]
        pub fn get_voters_outgoing(&self) -> &[u64] {
            &self.voters_outgoing
        }

        #[inline]
        pub fn mut_voters_outgoing(&mut self) -> &mut Vec<u64> {
            &mut self.voters_outgoing
        }

        #[inline]
        pub fn set_voters_outgoing(&mut self, v: Vec<u64>) {
            self.voters_outgoing = v;
        }

        #[inline]
        pub fn get_learners_next(&self) -> &[u64] {
            &self.learners_next
        }

        #[inline]
        pub fn mut_learners_next(&mut self) -> &mut Vec<u64> {
            &mut self.learners_next
        }

        #[inline]
        pub fn set_learners_next(&mut self, v: Vec<u64>) {
            self.learners_next = v;
        }

        #[inline]
        pub fn get_auto_leave(&self) -> bool {
            self.auto_leave
        }

        #[inline]
        pub fn set_auto_leave(&mut self, v: bool) {
            self.auto_leave = v;
        }
    }

    // ============ ConfChange accessors ============
    impl ConfChange {
        #[inline]
        pub fn get_change_type(&self) -> ConfChangeType {
            ConfChangeType::try_from(self.change_type).unwrap_or(ConfChangeType::AddNode)
        }

        #[inline]
        pub fn get_node_id(&self) -> u64 {
            self.node_id
        }

        #[inline]
        pub fn set_node_id(&mut self, v: u64) {
            self.node_id = v;
        }

        #[inline]
        pub fn get_context(&self) -> &[u8] {
            &self.context
        }

        #[inline]
        pub fn set_context(&mut self, v: Bytes) {
            self.context = v;
        }

        #[inline]
        pub fn take_context(&mut self) -> Bytes {
            std::mem::take(&mut self.context)
        }

        #[inline]
        pub fn get_id(&self) -> u64 {
            self.id
        }

        #[inline]
        pub fn set_id(&mut self, v: u64) {
            self.id = v;
        }
    }

    // ============ ConfChangeSingle accessors ============
    impl ConfChangeSingle {
        #[inline]
        pub fn get_change_type(&self) -> ConfChangeType {
            ConfChangeType::try_from(self.change_type).unwrap_or(ConfChangeType::AddNode)
        }

        #[inline]
        pub fn get_node_id(&self) -> u64 {
            self.node_id
        }

        #[inline]
        pub fn set_node_id(&mut self, v: u64) {
            self.node_id = v;
        }
    }

    // ============ ConfChangeV2 accessors ============
    impl ConfChangeV2 {
        #[inline]
        pub fn get_transition(&self) -> ConfChangeTransition {
            ConfChangeTransition::try_from(self.transition).unwrap_or(ConfChangeTransition::Auto)
        }

        #[inline]
        pub fn get_changes(&self) -> &[ConfChangeSingle] {
            &self.changes
        }

        #[inline]
        pub fn mut_changes(&mut self) -> &mut Vec<ConfChangeSingle> {
            &mut self.changes
        }

        #[inline]
        pub fn set_changes(&mut self, v: Vec<ConfChangeSingle>) {
            self.changes = v;
        }

        #[inline]
        pub fn take_changes(&mut self) -> Vec<ConfChangeSingle> {
            std::mem::take(&mut self.changes)
        }

        #[inline]
        pub fn get_context(&self) -> &[u8] {
            &self.context
        }

        #[inline]
        pub fn set_context(&mut self, v: Bytes) {
            self.context = v;
        }

        #[inline]
        pub fn take_context(&mut self) -> Bytes {
            std::mem::take(&mut self.context)
        }
    }
}

pub mod prelude {
    pub use crate::eraftpb::{
        ConfChange, ConfChangeSingle, ConfChangeTransition, ConfChangeType, ConfChangeV2,
        ConfState, Entry, EntryType, HardState, Message, MessageType, Snapshot, SnapshotMetadata,
    };
}

pub mod util {
    use crate::eraftpb::ConfState;

    impl<Iter1, Iter2> From<(Iter1, Iter2)> for ConfState
    where
        Iter1: IntoIterator<Item = u64>,
        Iter2: IntoIterator<Item = u64>,
    {
        fn from((voters, learners): (Iter1, Iter2)) -> Self {
            let mut conf_state = ConfState::default();
            conf_state.voters.extend(voters);
            conf_state.learners.extend(learners);
            conf_state
        }
    }
}
