/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::fmt;

use malloc_size_of_derive::MallocSizeOf;
use profile_traits::mem::ReportsChan;
use serde::{Deserialize, Serialize};
use servo_base::generic_channel::GenericSender;
use servo_base::id::WebViewId;
use servo_url::ImmutableOrigin;

pub const WEB_STORAGE_STATE_SCHEMA_VERSION_V1: u16 = 1;
pub const WEB_STORAGE_STATE_MAX_ORIGINS_V1: usize = 64;
pub const WEB_STORAGE_STATE_MAX_ENTRIES_PER_AREA_V1: usize = 1024;
pub const WEB_STORAGE_STATE_MAX_KEY_BYTES_V1: usize = 4096;
pub const WEB_STORAGE_STATE_MAX_VALUE_BYTES_V1: usize = 128 * 1024;
pub const WEB_STORAGE_STATE_MAX_ORIGIN_BYTES_V1: usize = 512 * 1024;
pub const WEB_STORAGE_STATE_MAX_TOTAL_BYTES_V1: usize = 512 * 1024;
/// Exact compact public-JSON budget for the `SessionState.origins` array.
///
/// Cookies receive the same independent budget. Together they consume 512,000 bytes and leave
/// 12,288 bytes below the 512 KiB whole-state cap for the fixed envelope and conservative
/// headroom.
pub const WEB_STORAGE_STATE_MAX_PUBLIC_JSON_BYTES_V1: usize = 250 * 1024;

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebStorageStateEntryV1 {
    pub key: String,
    pub value: String,
}

impl fmt::Debug for WebStorageStateEntryV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebStorageStateEntryV1")
            .field("key_bytes", &self.key.len())
            .field("value_bytes", &self.value.len())
            .finish()
    }
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WebStorageOriginStateV1 {
    pub origin: String,
    pub local_storage: Vec<WebStorageStateEntryV1>,
    pub session_storage: Vec<WebStorageStateEntryV1>,
}

impl fmt::Debug for WebStorageOriginStateV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebStorageOriginStateV1")
            .field("origin_bytes", &self.origin.len())
            .field("local_storage_entries", &self.local_storage.len())
            .field("session_storage_entries", &self.session_storage.len())
            .finish()
    }
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebStorageStateSnapshotV1 {
    pub schema_version: u16,
    pub revision: u64,
    pub origins: Vec<WebStorageOriginStateV1>,
}

impl fmt::Debug for WebStorageStateSnapshotV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebStorageStateSnapshotV1")
            .field("schema_version", &self.schema_version)
            .field("revision", &self.revision)
            .field("origin_count", &self.origins.len())
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebStorageStateError {
    UnsupportedSchemaVersion,
    StaleRevision,
    RevisionExhausted,
    PersistentBackendUnsupported,
    TooManyOrigins,
    TooManyEntries,
    KeyTooLarge,
    ValueTooLarge,
    OriginTooLarge,
    SnapshotTooLarge,
    InvalidOrigin,
    DuplicateOrigin,
    DuplicateKey,
    BackendFailure,
}

/// Mutation policy selected by the immutable document-control profile.
///
/// Ordinary Servo storage keeps its existing 5 MiB area quota. A controlled session additionally
/// requires every successful page mutation to preserve the portable session-state bounds.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebStorageMutationPolicy {
    Ordinary,
    ControlledSessionV1,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebStorageMutationError {
    QuotaExceeded,
    ControlledStateLimit,
    RevisionExhausted,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, MallocSizeOf, Serialize)]
pub enum WebStorageType {
    Session,
    Local,
}

#[derive(Clone, Debug, Deserialize, MallocSizeOf, Serialize)]
pub struct OriginDescriptor {
    pub name: String,
}

impl OriginDescriptor {
    pub fn new(name: String) -> Self {
        OriginDescriptor { name }
    }
}

/// Request operations on the storage data associated with a particular url
#[derive(Debug, Deserialize, Serialize)]
pub enum WebStorageThreadMsg {
    /// Return all local storage and the selected WebView's session storage as a bounded snapshot.
    ExportState {
        sender: GenericSender<Result<WebStorageStateSnapshotV1, WebStorageStateError>>,
        webview_id: WebViewId,
    },

    /// Atomically replace all local storage and the selected WebView's session storage.
    ReplaceState {
        sender: GenericSender<Result<u64, WebStorageStateError>>,
        webview_id: WebViewId,
        expected_revision: u64,
        snapshot: WebStorageStateSnapshotV1,
    },

    /// gets the number of key/value pairs present in the associated storage data
    Length(
        GenericSender<usize>,
        WebStorageType,
        WebViewId,
        ImmutableOrigin,
    ),

    /// gets the name of the key at the specified index in the associated storage data
    Key(
        GenericSender<Option<String>>,
        WebStorageType,
        WebViewId,
        ImmutableOrigin,
        u32,
    ),

    /// Gets the available keys in the associated storage data
    Keys(
        GenericSender<Vec<String>>,
        WebStorageType,
        WebViewId,
        ImmutableOrigin,
    ),

    /// gets the value associated with the given key in the associated storage data
    GetItem(
        GenericSender<Option<String>>,
        WebStorageType,
        WebViewId,
        ImmutableOrigin,
        String,
    ),

    /// sets the value of the given key in the associated storage data
    SetItem(
        GenericSender<Result<(bool, Option<String>), WebStorageMutationError>>,
        WebStorageType,
        WebStorageMutationPolicy,
        WebViewId,
        ImmutableOrigin,
        String,
        String,
    ),

    /// removes the key/value pair for the given key in the associated storage data
    RemoveItem(
        GenericSender<Option<String>>,
        WebStorageType,
        WebStorageMutationPolicy,
        WebViewId,
        ImmutableOrigin,
        String,
    ),

    /// clears the associated storage data by removing all the key/value pairs
    Clear(
        GenericSender<bool>,
        WebStorageType,
        WebStorageMutationPolicy,
        WebViewId,
        ImmutableOrigin,
    ),

    /// clones all storage data of the given top-level browsing context for a new browsing context.
    /// should only be used for sessionStorage.
    Clone {
        sender: GenericSender<()>,
        src: WebViewId,
        dest: WebViewId,
    },

    /// gets the list of origin descriptors for given storage type
    ///
    /// TODO: Consider returning `Vec<SiteDescriptor>`
    ListOrigins(GenericSender<Vec<OriginDescriptor>>, WebStorageType),

    /// clears storage data for given storage type and sites, affecting all matching origins
    ClearDataForSites(GenericSender<()>, WebStorageType, Vec<String>),

    /// send a reply when done cleaning up thread resources and then shut it down
    Exit(GenericSender<()>),

    /// Measure memory used by this thread and send the report over the provided channel.
    CollectMemoryReport(ReportsChan),
}
