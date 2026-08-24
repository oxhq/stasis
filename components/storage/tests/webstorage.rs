/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use profile::mem as profile_mem;
use servo_base::generic_channel as base_channel;
use servo_base::generic_channel::GenericSend;
use servo_base::id::TEST_WEBVIEW_ID;
use servo_default_resources as _;
use servo_url::{ImmutableOrigin, ServoUrl};
use storage_traits::StorageThreads;
use storage_traits::webstorage_thread::{
    WEB_STORAGE_STATE_MAX_ENTRIES_PER_AREA_V1, WEB_STORAGE_STATE_MAX_KEY_BYTES_V1,
    WEB_STORAGE_STATE_MAX_ORIGINS_V1, WEB_STORAGE_STATE_MAX_PUBLIC_JSON_BYTES_V1,
    WEB_STORAGE_STATE_MAX_VALUE_BYTES_V1, WEB_STORAGE_STATE_SCHEMA_VERSION_V1,
    WebStorageMutationError, WebStorageMutationPolicy, WebStorageOriginStateV1,
    WebStorageStateEntryV1, WebStorageStateError, WebStorageStateSnapshotV1, WebStorageThreadMsg,
    WebStorageType,
};
use tempfile::TempDir;

pub(crate) struct WebStorageTest {
    tmp_dir: Option<TempDir>,
    threads: StorageThreads,
}

impl WebStorageTest {
    pub(crate) fn new() -> Self {
        let tmp_dir = tempfile::tempdir().unwrap();
        let config_dir = tmp_dir.path().to_path_buf();
        let mem_profiler_chan = profile_mem::Profiler::create();
        let threads = storage::new_storage_threads(mem_profiler_chan, Some(config_dir), false);

        Self {
            tmp_dir: Some(tmp_dir),
            threads: threads.0,
        }
    }

    pub(crate) fn new_in_memory() -> Self {
        let mem_profiler_chan = profile_mem::Profiler::create();
        let threads = storage::new_storage_threads(mem_profiler_chan, None, false);

        Self {
            tmp_dir: None,
            threads: threads.0,
        }
    }

    pub(crate) fn restart(mut self) -> Self {
        let tmp_dir = self.tmp_dir.take();
        let config_dir = tmp_dir.as_ref().map(|d| d.path().to_path_buf());
        let mem_profiler_chan = profile_mem::Profiler::create();
        let threads = storage::new_storage_threads(mem_profiler_chan, config_dir, false);

        Self {
            tmp_dir: tmp_dir,
            threads: threads.0,
        }
    }

    pub(crate) fn threads(&self) -> StorageThreads {
        self.threads.clone()
    }

    pub(crate) fn length(&self, storage_type: WebStorageType, origin: &ImmutableOrigin) -> usize {
        let (sender, receiver) = base_channel::channel().unwrap();
        self.threads
            .send(WebStorageThreadMsg::Length(
                sender,
                storage_type,
                TEST_WEBVIEW_ID,
                origin.clone(),
            ))
            .unwrap();
        receiver.recv().unwrap()
    }

    pub(crate) fn key(
        &self,
        storage_type: WebStorageType,
        origin: &ImmutableOrigin,
        index: u32,
    ) -> Option<String> {
        let (sender, receiver) = base_channel::channel().unwrap();
        self.threads
            .send(WebStorageThreadMsg::Key(
                sender,
                storage_type,
                TEST_WEBVIEW_ID,
                origin.clone(),
                index,
            ))
            .unwrap();
        receiver.recv().unwrap()
    }

    pub(crate) fn keys(
        &self,
        storage_type: WebStorageType,
        origin: &ImmutableOrigin,
    ) -> Vec<String> {
        let (sender, receiver) = base_channel::channel().unwrap();
        self.threads
            .send(WebStorageThreadMsg::Keys(
                sender,
                storage_type,
                TEST_WEBVIEW_ID,
                origin.clone(),
            ))
            .unwrap();
        receiver.recv().unwrap()
    }

    pub(crate) fn get_item(
        &self,
        storage_type: WebStorageType,
        origin: &ImmutableOrigin,
        key: &str,
    ) -> Option<String> {
        let (sender, receiver) = base_channel::channel().unwrap();
        self.threads
            .send(WebStorageThreadMsg::GetItem(
                sender,
                storage_type,
                TEST_WEBVIEW_ID,
                origin.clone(),
                key.into(),
            ))
            .unwrap();
        receiver.recv().unwrap()
    }

    pub(crate) fn set_item(
        &self,
        storage_type: WebStorageType,
        origin: &ImmutableOrigin,
        key: &str,
        value: &str,
    ) -> Result<(bool, Option<String>), WebStorageMutationError> {
        self.set_item_with_policy(
            storage_type,
            WebStorageMutationPolicy::Ordinary,
            origin,
            key,
            value,
        )
    }

    pub(crate) fn set_item_with_policy(
        &self,
        storage_type: WebStorageType,
        mutation_policy: WebStorageMutationPolicy,
        origin: &ImmutableOrigin,
        key: &str,
        value: &str,
    ) -> Result<(bool, Option<String>), WebStorageMutationError> {
        let (sender, receiver) = base_channel::channel().unwrap();
        self.threads
            .send(WebStorageThreadMsg::SetItem(
                sender,
                storage_type,
                mutation_policy,
                TEST_WEBVIEW_ID,
                origin.clone(),
                key.into(),
                value.into(),
            ))
            .unwrap();
        receiver.recv().unwrap()
    }

    pub(crate) fn remove_item(
        &self,
        storage_type: WebStorageType,
        origin: &ImmutableOrigin,
        key: &str,
    ) -> Option<String> {
        self.remove_item_with_policy(
            storage_type,
            WebStorageMutationPolicy::Ordinary,
            origin,
            key,
        )
    }

    pub(crate) fn remove_item_with_policy(
        &self,
        storage_type: WebStorageType,
        mutation_policy: WebStorageMutationPolicy,
        origin: &ImmutableOrigin,
        key: &str,
    ) -> Option<String> {
        let (sender, receiver) = base_channel::channel().unwrap();
        self.threads
            .send(WebStorageThreadMsg::RemoveItem(
                sender,
                storage_type,
                mutation_policy,
                TEST_WEBVIEW_ID,
                origin.clone(),
                key.into(),
            ))
            .unwrap();
        receiver.recv().unwrap()
    }

    pub(crate) fn clear(&self, storage_type: WebStorageType, origin: &ImmutableOrigin) -> bool {
        self.clear_with_policy(storage_type, WebStorageMutationPolicy::Ordinary, origin)
    }

    pub(crate) fn clear_with_policy(
        &self,
        storage_type: WebStorageType,
        mutation_policy: WebStorageMutationPolicy,
        origin: &ImmutableOrigin,
    ) -> bool {
        let (sender, receiver) = base_channel::channel().unwrap();
        self.threads
            .send(WebStorageThreadMsg::Clear(
                sender,
                storage_type,
                mutation_policy,
                TEST_WEBVIEW_ID,
                origin.clone(),
            ))
            .unwrap();
        receiver.recv().unwrap()
    }

    pub(crate) fn state(&self) -> Result<WebStorageStateSnapshotV1, WebStorageStateError> {
        self.threads.webstorage_state(TEST_WEBVIEW_ID)
    }

    pub(crate) fn replace_state(
        &self,
        expected_revision: u64,
        snapshot: WebStorageStateSnapshotV1,
    ) -> Result<u64, WebStorageStateError> {
        self.threads
            .replace_webstorage_state(TEST_WEBVIEW_ID, expected_revision, snapshot)
    }

    /// Gracefully shut down the webstorage thread to avoid dangling threads in tests.
    fn shutdown(&self) {
        let (sender, receiver) = base_channel::channel().unwrap();
        self.threads
            .send(WebStorageThreadMsg::Exit(sender))
            .expect("failed to send Exit");
        // Wait for acknowledgement so the thread terminates before the test ends.
        let _ = receiver.recv();
    }
}

impl Drop for WebStorageTest {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[test]
fn set_and_get_item() {
    let test = WebStorageTest::new();
    let url = ServoUrl::parse("https://example.com").unwrap();

    // Set a value.
    let result = test.set_item(WebStorageType::Local, &url.origin(), "foo", "bar");
    assert_eq!(result, Ok((true, None)));

    // Retrieve the value.
    let result = test.get_item(WebStorageType::Local, &url.origin(), "foo");
    assert_eq!(result, Some("bar".into()));
}

#[test]
fn set_and_get_item_in_memory() {
    let test = WebStorageTest::new_in_memory();
    let url = ServoUrl::parse("https://example.com").unwrap();

    // Set a value.
    let result = test.set_item(WebStorageType::Local, &url.origin(), "foo", "bar");
    assert_eq!(result, Ok((true, None)));

    // Retrieve the value.
    let result = test.get_item(WebStorageType::Local, &url.origin(), "foo");
    assert_eq!(result, Some("bar".into()));
}

fn state_entry(key: &str, value: &str) -> WebStorageStateEntryV1 {
    WebStorageStateEntryV1 {
        key: key.into(),
        value: value.into(),
    }
}

#[test]
fn state_replace_round_trips_local_and_webview_scoped_session_storage() {
    let test = WebStorageTest::new_in_memory();
    let snapshot = WebStorageStateSnapshotV1 {
        schema_version: WEB_STORAGE_STATE_SCHEMA_VERSION_V1,
        revision: 99,
        origins: vec![WebStorageOriginStateV1 {
            origin: "https://example.com".into(),
            local_storage: vec![state_entry("z", "last"), state_entry("a", "first")],
            session_storage: vec![state_entry("auth", "scoped")],
        }],
    };

    assert_eq!(test.replace_state(0, snapshot), Ok(1));
    let origin = ServoUrl::parse("https://example.com").unwrap().origin();
    assert_eq!(
        test.get_item(WebStorageType::Local, &origin, "a"),
        Some("first".into())
    );
    assert_eq!(
        test.get_item(WebStorageType::Session, &origin, "auth"),
        Some("scoped".into())
    );

    let exported = test.state().unwrap();
    assert_eq!(exported.revision, 1);
    assert_eq!(exported.origins.len(), 1);
    assert_eq!(exported.origins[0].local_storage[0].key, "a");
    assert_eq!(exported.origins[0].local_storage[1].key, "z");
    let public_fragment = serde_json::to_value(&exported.origins).unwrap();
    assert!(public_fragment[0].get("localStorage").is_some());
    assert!(public_fragment[0].get("sessionStorage").is_some());
    assert!(public_fragment[0].get("local_storage").is_none());
    assert!(!format!("{exported:?}").contains("example.com"));
    assert!(!format!("{:?}", exported.origins[0]).contains("first"));
    let sensitive_entry = state_entry("private-key", "private-value");
    let debug_entry = format!("{sensitive_entry:?}");
    assert!(!debug_entry.contains("private-key"));
    assert!(!debug_entry.contains("private-value"));

    assert_eq!(
        test.set_item(WebStorageType::Session, &origin, "auth", "rotated"),
        Ok((true, Some("scoped".into())))
    );
    assert_eq!(test.state().unwrap().revision, 2);
}

#[test]
fn state_replace_rejects_stale_and_invalid_snapshots_atomically() {
    let test = WebStorageTest::new_in_memory();
    let initial = WebStorageStateSnapshotV1 {
        schema_version: WEB_STORAGE_STATE_SCHEMA_VERSION_V1,
        revision: 0,
        origins: vec![WebStorageOriginStateV1 {
            origin: "https://example.com".into(),
            local_storage: vec![state_entry("key", "value")],
            session_storage: vec![],
        }],
    };
    assert_eq!(test.replace_state(0, initial), Ok(1));
    let before = test.state().unwrap();

    assert_eq!(
        test.replace_state(0, before.clone()),
        Err(WebStorageStateError::StaleRevision)
    );
    assert_eq!(test.state().unwrap(), before);

    let duplicate = WebStorageStateSnapshotV1 {
        schema_version: WEB_STORAGE_STATE_SCHEMA_VERSION_V1,
        revision: 0,
        origins: vec![WebStorageOriginStateV1 {
            origin: "https://example.com".into(),
            local_storage: vec![state_entry("key", "one"), state_entry("key", "two")],
            session_storage: vec![],
        }],
    };
    assert_eq!(
        test.replace_state(1, duplicate),
        Err(WebStorageStateError::DuplicateKey)
    );
    assert_eq!(test.state().unwrap(), before);

    let invalid_origin = WebStorageStateSnapshotV1 {
        schema_version: WEB_STORAGE_STATE_SCHEMA_VERSION_V1,
        revision: 0,
        origins: vec![WebStorageOriginStateV1 {
            origin: "data:text/plain,opaque".into(),
            local_storage: vec![state_entry("key", "other")],
            session_storage: vec![],
        }],
    };
    assert_eq!(
        test.replace_state(1, invalid_origin),
        Err(WebStorageStateError::InvalidOrigin)
    );
    assert_eq!(test.state().unwrap(), before);
}

#[test]
fn state_transfer_rejects_persistent_webstorage_backend() {
    let test = WebStorageTest::new();
    assert_eq!(
        test.state(),
        Err(WebStorageStateError::PersistentBackendUnsupported)
    );
    assert_eq!(
        test.replace_state(
            0,
            WebStorageStateSnapshotV1 {
                schema_version: WEB_STORAGE_STATE_SCHEMA_VERSION_V1,
                revision: 0,
                origins: vec![],
            },
        ),
        Err(WebStorageStateError::PersistentBackendUnsupported)
    );
}

fn controlled_set(
    test: &WebStorageTest,
    storage_type: WebStorageType,
    origin: &ImmutableOrigin,
    key: &str,
    value: &str,
) -> Result<(bool, Option<String>), WebStorageMutationError> {
    test.set_item_with_policy(
        storage_type,
        WebStorageMutationPolicy::ControlledSessionV1,
        origin,
        key,
        value,
    )
}

fn public_origins_json_bytes(origins: &[WebStorageOriginStateV1]) -> usize {
    serde_json::to_vec(origins).unwrap().len()
}

#[test]
fn controlled_page_set_preserves_exact_public_json_budget_atomically() {
    let origin = ServoUrl::parse("https://example.com").unwrap().origin();
    let candidate = |value_bytes: usize| {
        vec![WebStorageOriginStateV1 {
            origin: "https://example.com".into(),
            local_storage: vec![state_entry("escaped", &"\0".repeat(value_bytes))],
            session_storage: Vec::new(),
        }]
    };
    let mut accepted = 0;
    let mut rejected = WEB_STORAGE_STATE_MAX_VALUE_BYTES_V1 + 1;
    while accepted + 1 < rejected {
        let middle = accepted + (rejected - accepted) / 2;
        if public_origins_json_bytes(&candidate(middle))
            <= WEB_STORAGE_STATE_MAX_PUBLIC_JSON_BYTES_V1
        {
            accepted = middle;
        } else {
            rejected = middle;
        }
    }
    assert!(accepted < WEB_STORAGE_STATE_MAX_VALUE_BYTES_V1);
    assert_eq!(rejected, accepted + 1);
    assert!(
        public_origins_json_bytes(&candidate(accepted))
            <= WEB_STORAGE_STATE_MAX_PUBLIC_JSON_BYTES_V1
    );
    assert!(
        public_origins_json_bytes(&candidate(rejected))
            > WEB_STORAGE_STATE_MAX_PUBLIC_JSON_BYTES_V1
    );

    let test = WebStorageTest::new_in_memory();
    assert_eq!(
        controlled_set(
            &test,
            WebStorageType::Local,
            &origin,
            "escaped",
            &"\0".repeat(accepted),
        ),
        Ok((true, None))
    );
    let before_rejection = test.state().unwrap();
    assert!(
        public_origins_json_bytes(&before_rejection.origins)
            <= WEB_STORAGE_STATE_MAX_PUBLIC_JSON_BYTES_V1
    );
    assert_eq!(
        controlled_set(
            &test,
            WebStorageType::Local,
            &origin,
            "escaped",
            &"\0".repeat(rejected),
        ),
        Err(WebStorageMutationError::ControlledStateLimit)
    );
    assert_eq!(test.state().unwrap(), before_rejection);
    assert_eq!(
        test.get_item(WebStorageType::Local, &origin, "escaped"),
        Some("\0".repeat(accepted))
    );
    assert_eq!(
        controlled_set(&test, WebStorageType::Session, &origin, "second-area", "x",),
        Err(WebStorageMutationError::ControlledStateLimit)
    );
    assert_eq!(test.state().unwrap(), before_rejection);

    // Ordinary Servo retains its existing area quota and is deliberately not narrowed by the
    // controlled-session state-transfer budget.
    let ordinary = WebStorageTest::new_in_memory();
    assert_eq!(
        ordinary.set_item(
            WebStorageType::Local,
            &origin,
            "escaped",
            &"\0".repeat(rejected),
        ),
        Ok((true, None))
    );
    assert_eq!(
        ordinary.state(),
        Err(WebStorageStateError::SnapshotTooLarge)
    );
}

#[test]
fn controlled_page_set_rejects_entry_and_origin_overflow_without_revision_change() {
    let field_test = WebStorageTest::new_in_memory();
    let field_origin = ServoUrl::parse("https://fields.example.com")
        .unwrap()
        .origin();
    let empty = field_test.state().unwrap();
    assert_eq!(
        controlled_set(
            &field_test,
            WebStorageType::Local,
            &field_origin,
            &"k".repeat(WEB_STORAGE_STATE_MAX_KEY_BYTES_V1 + 1),
            "value",
        ),
        Err(WebStorageMutationError::ControlledStateLimit)
    );
    assert_eq!(field_test.state().unwrap(), empty);
    assert_eq!(
        controlled_set(
            &field_test,
            WebStorageType::Session,
            &field_origin,
            "key",
            &"v".repeat(WEB_STORAGE_STATE_MAX_VALUE_BYTES_V1 + 1),
        ),
        Err(WebStorageMutationError::ControlledStateLimit)
    );
    assert_eq!(field_test.state().unwrap(), empty);

    let test = WebStorageTest::new_in_memory();
    let origin = ServoUrl::parse("https://example.com").unwrap().origin();
    let entries = (0..WEB_STORAGE_STATE_MAX_ENTRIES_PER_AREA_V1)
        .map(|index| state_entry(&format!("key-{index:04}"), "x"))
        .collect();
    assert_eq!(
        test.replace_state(
            0,
            WebStorageStateSnapshotV1 {
                schema_version: WEB_STORAGE_STATE_SCHEMA_VERSION_V1,
                revision: 0,
                origins: vec![WebStorageOriginStateV1 {
                    origin: "https://example.com".into(),
                    local_storage: entries,
                    session_storage: Vec::new(),
                }],
            },
        ),
        Ok(1)
    );
    let before_entry_overflow = test.state().unwrap();
    assert_eq!(
        controlled_set(&test, WebStorageType::Local, &origin, "one-too-many", "x",),
        Err(WebStorageMutationError::ControlledStateLimit)
    );
    assert_eq!(test.state().unwrap(), before_entry_overflow);

    let origins = (0..WEB_STORAGE_STATE_MAX_ORIGINS_V1)
        .map(|index| WebStorageOriginStateV1 {
            origin: format!("https://{index}.example.com"),
            local_storage: vec![state_entry("key", "value")],
            session_storage: Vec::new(),
        })
        .collect();
    assert_eq!(
        test.replace_state(
            before_entry_overflow.revision,
            WebStorageStateSnapshotV1 {
                schema_version: WEB_STORAGE_STATE_SCHEMA_VERSION_V1,
                revision: 0,
                origins,
            },
        ),
        Ok(before_entry_overflow.revision + 1)
    );
    let before_origin_overflow = test.state().unwrap();
    let extra_origin = ServoUrl::parse("https://extra.example.com")
        .unwrap()
        .origin();
    assert_eq!(
        controlled_set(
            &test,
            WebStorageType::Session,
            &extra_origin,
            "key",
            "value",
        ),
        Err(WebStorageMutationError::ControlledStateLimit)
    );
    assert_eq!(test.state().unwrap(), before_origin_overflow);
}

#[test]
fn controlled_remove_and_clear_rotate_revision_and_remain_exportable() {
    let test = WebStorageTest::new_in_memory();
    let origin = ServoUrl::parse("https://example.com").unwrap().origin();
    let empty = test.state().unwrap();
    controlled_set(&test, WebStorageType::Local, &origin, "a", "one").unwrap();
    let after_local_set = test.state().unwrap();
    assert_eq!(after_local_set.revision, empty.revision + 1);
    controlled_set(&test, WebStorageType::Session, &origin, "b", "two").unwrap();
    let after_sets = test.state().unwrap();
    assert_eq!(after_sets.revision, after_local_set.revision + 1);

    assert_eq!(
        controlled_set(&test, WebStorageType::Local, &origin, "a", "one"),
        Ok((false, None))
    );
    assert_eq!(test.state().unwrap(), after_sets);
    assert_eq!(
        test.remove_item_with_policy(
            WebStorageType::Local,
            WebStorageMutationPolicy::ControlledSessionV1,
            &origin,
            "missing",
        ),
        None
    );
    assert_eq!(test.state().unwrap(), after_sets);

    assert_eq!(
        test.remove_item_with_policy(
            WebStorageType::Local,
            WebStorageMutationPolicy::ControlledSessionV1,
            &origin,
            "a",
        ),
        Some("one".into())
    );
    let after_remove = test.state().unwrap();
    assert_eq!(after_remove.revision, after_sets.revision + 1);
    assert!(
        public_origins_json_bytes(&after_remove.origins)
            <= WEB_STORAGE_STATE_MAX_PUBLIC_JSON_BYTES_V1
    );

    assert!(test.clear_with_policy(
        WebStorageType::Session,
        WebStorageMutationPolicy::ControlledSessionV1,
        &origin,
    ));
    let after_clear = test.state().unwrap();
    assert_eq!(after_clear.revision, after_remove.revision + 1);
    assert!(after_clear.origins.is_empty());
    assert!(
        public_origins_json_bytes(&after_clear.origins)
            <= WEB_STORAGE_STATE_MAX_PUBLIC_JSON_BYTES_V1
    );
    assert!(!test.clear_with_policy(
        WebStorageType::Session,
        WebStorageMutationPolicy::ControlledSessionV1,
        &origin,
    ));
    assert_eq!(test.state().unwrap(), after_clear);
}

#[test]
fn length_key_and_keys() {
    let test = WebStorageTest::new();
    let url = ServoUrl::parse("https://example.com").unwrap();

    // Insert two items.
    for (k, v) in [("foo", "v1"), ("bar", "v2")] {
        let _ = test.set_item(WebStorageType::Local, &url.origin(), k, v);
    }

    // Verify length.
    let result = test.length(WebStorageType::Local, &url.origin());
    assert_eq!(result, 2);

    // Verify key(0) returns one of the inserted keys.
    let result = test.key(WebStorageType::Local, &url.origin(), 0);
    let key0 = result.unwrap();
    assert!(key0 == "foo" || key0 == "bar");

    // Verify keys vector contains both keys.
    let result = test.keys(WebStorageType::Local, &url.origin());
    assert_eq!(result.len(), 2);
    assert!(result.contains(&"foo".to_string()));
    assert!(result.contains(&"bar".to_string()));
}

#[test]
fn remove_item_and_clear() {
    let test = WebStorageTest::new();
    let url = ServoUrl::parse("https://example.com").unwrap();

    // Insert items.
    for (k, v) in [("foo", "v1"), ("bar", "v2")] {
        let _ = test.set_item(WebStorageType::Local, &url.origin(), k, v);
    }

    // Remove one item and verify old value is returned.
    let result = test.remove_item(WebStorageType::Local, &url.origin(), "foo");
    assert_eq!(result, Some("v1".into()));

    // Removing again should return None.
    let result = test.remove_item(WebStorageType::Local, &url.origin(), "foo");
    assert_eq!(result, None);

    // Clear storage and verify it reported change.
    let result = test.clear(WebStorageType::Local, &url.origin());
    assert!(result);

    // Length should now be zero.
    let result = test.length(WebStorageType::Local, &url.origin());
    assert_eq!(result, 0);
}

fn test_origin_descriptors(
    test: WebStorageTest,
    storage_type: WebStorageType,
    survives_restart: bool,
) {
    let threads = test.threads();
    let url = ServoUrl::parse("https://example.com").unwrap();

    // Set a value.
    let _ = test.set_item(storage_type, &url.origin(), "foo", "bar");

    // Verify descriptors.
    let descriptors = threads.webstorage_origins(storage_type);
    assert_eq!(descriptors.len(), 1);
    assert_eq!(descriptors[0].name, "https://example.com");

    // Restart storage threads.
    let test = test.restart();
    let threads = test.threads();

    // There should still be descriptors.
    let descriptors = threads.webstorage_origins(storage_type);
    if survives_restart {
        assert_eq!(descriptors.len(), 1);
        assert_eq!(descriptors[0].name, "https://example.com");
    } else {
        assert!(descriptors.is_empty());
    }
}

#[test]
fn origin_descriptors_session() {
    let test = WebStorageTest::new();
    test_origin_descriptors(
        test,
        WebStorageType::Session,
        /* survives_restart */ false,
    );
}

#[test]
fn origin_descriptors_local() {
    let test = WebStorageTest::new();
    test_origin_descriptors(
        test,
        WebStorageType::Local,
        /* survives_restart */ true,
    );
}

fn test_clear_data_for_sites(test: WebStorageTest, storage_type: WebStorageType) {
    let threads = test.threads();
    let url = ServoUrl::parse("https://example.com").unwrap();

    // Set a value.
    let _ = test.set_item(storage_type, &url.origin(), "foo", "bar");

    // Verify length.
    let result = test.length(storage_type, &url.origin());
    assert_eq!(result, 1);

    // Verify descriptors.
    let descriptors = threads.webstorage_origins(storage_type);
    assert_eq!(descriptors.len(), 1);

    // Clear site.
    threads.clear_webstorage_for_sites(storage_type, &["example.com"]);

    // Length should now be zero.
    let result = test.length(storage_type, &url.origin());
    assert_eq!(result, 0);

    // There should now be no descriptors.
    let descriptors = threads.webstorage_origins(storage_type);
    match storage_type {
        WebStorageType::Session => assert_eq!(descriptors.len(), 0),
        WebStorageType::Local =>
        // TODO: Fix localStorage to not create origin descriptors for
        // read only operations (the length check above).
        {
            assert_eq!(descriptors.len(), 1)
        },
    }

    // Restart storage threads.
    let test = test.restart();
    let threads = test.threads();

    // Length should still be zero.
    let result = test.length(storage_type, &url.origin());
    assert_eq!(result, 0);

    // There should still be no descriptors.
    let descriptors = threads.webstorage_origins(storage_type);
    match storage_type {
        WebStorageType::Session => assert_eq!(descriptors.len(), 0),
        WebStorageType::Local =>
        // TODO: Fix localStorage to not create origin descriptors for
        // read only operations (the length check above).
        {
            assert_eq!(descriptors.len(), 1)
        },
    }

    // Set a different value.
    let _ = test.set_item(storage_type, &url.origin(), "foo2", "bar2");

    // Verify the original value doesn't exist.
    let result = test.get_item(storage_type, &url.origin(), "foo");
    assert_eq!(result, None);
}

#[test]
fn clear_data_for_sites_session() {
    let test = WebStorageTest::new();
    test_clear_data_for_sites(test, WebStorageType::Session);
}

#[test]
fn clear_data_for_sites_local() {
    let test = WebStorageTest::new();
    test_clear_data_for_sites(test, WebStorageType::Local);
}

#[test]
fn clear_data_for_sites_local_in_memory() {
    let test = WebStorageTest::new_in_memory();
    test_clear_data_for_sites(test, WebStorageType::Local);
}

#[test]
fn no_storage_type_conflict() {
    // Ensures that editing session storage does not affect local storage and vice versa.
    let mut test = WebStorageTest::new();
    let url = ServoUrl::parse("https://example.com").unwrap();
    test.set_item(
        WebStorageType::Local,
        &url.origin(),
        "key".into(),
        "local_value".into(),
    )
    .unwrap();
    // Set session storage item.
    test.set_item(
        WebStorageType::Session,
        &url.origin(),
        "key".into(),
        "session_value".into(),
    )
    .unwrap();
    // Shutdown threads to ensure data is cleared from session storage and local storage is loaded from disk
    test = test.restart();
    let result = test.get_item(WebStorageType::Local, &url.origin(), "key".into());
    assert_eq!(result, Some("local_value".into()));
    // Get session storage item.
    let result = test.get_item(WebStorageType::Session, &url.origin(), "key".into());
    assert_eq!(result, None);
}
