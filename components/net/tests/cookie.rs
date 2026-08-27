/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::time::{Duration, SystemTime};

use embedder_traits::{ControlledCookieContext, ControlledCookiePolicy};
use http::Method;
use net::cookie::ServoCookie;
use net::cookie_storage::CookieStorage;
use net_traits::{
    CONTROLLED_COOKIE_MAX_BATCH_VALUES_V1, CONTROLLED_COOKIE_MAX_RAW_VALUE_BYTES_V1,
    COOKIE_STATE_MAX_COOKIE_BYTES_V1, COOKIE_STATE_MAX_COOKIES_PER_REGISTRABLE_HOST_V1,
    COOKIE_STATE_MAX_COOKIES_V1, COOKIE_STATE_MAX_ENCODED_PUBLIC_ARRAY_BYTES_V1,
    COOKIE_STATE_SCHEMA_VERSION_V1, ControlledCookiePolicyError, CookieSource, CookieStateError,
    CookieStateRecordV1, CookieStateSameSite, CookieStateSnapshotV1,
};
use serde_json::json;
use servo_url::ServoUrl;
use time::macros::datetime;

#[test]
fn test_domain_match() {
    assert!(ServoCookie::domain_match("foo.com", "foo.com"));
    assert!(ServoCookie::domain_match("bar.foo.com", "foo.com"));
    assert!(ServoCookie::domain_match("baz.bar.foo.com", "foo.com"));

    assert!(!ServoCookie::domain_match("bar.foo.com", "bar.com"));
    assert!(!ServoCookie::domain_match("bar.com", "baz.bar.com"));
    assert!(!ServoCookie::domain_match("foo.com", "bar.com"));

    assert!(!ServoCookie::domain_match("bar.com", "bbar.com"));
    assert!(ServoCookie::domain_match("235.132.2.3", "235.132.2.3"));
    assert!(!ServoCookie::domain_match("235.132.2.3", "1.1.1.1"));
    assert!(!ServoCookie::domain_match("235.132.2.3", ".2.3"));
}

#[test]
fn test_path_match() {
    assert!(ServoCookie::path_match("/", "/"));
    assert!(ServoCookie::path_match("/index.html", "/"));
    assert!(ServoCookie::path_match("/w/index.html", "/"));
    assert!(ServoCookie::path_match("/w/index.html", "/w/index.html"));
    assert!(ServoCookie::path_match("/w/index.html", "/w/"));
    assert!(ServoCookie::path_match("/w/index.html", "/w"));

    assert!(!ServoCookie::path_match("/", "/w/"));
    assert!(!ServoCookie::path_match("/a", "/w/"));
    assert!(!ServoCookie::path_match("/", "/w"));
    assert!(!ServoCookie::path_match("/w/index.html", "/w/index"));
    assert!(!ServoCookie::path_match("/windex.html", "/w/"));
    assert!(!ServoCookie::path_match("/windex.html", "/w"));
}

#[test]
fn test_default_path() {
    assert_eq!(&*ServoCookie::default_path("/foo/bar/baz/"), "/foo/bar/baz");
    assert_eq!(&*ServoCookie::default_path("/foo/bar/baz"), "/foo/bar");
    assert_eq!(&*ServoCookie::default_path("/foo/"), "/foo");
    assert_eq!(&*ServoCookie::default_path("/foo"), "/");
    assert_eq!(&*ServoCookie::default_path("/"), "/");
    assert_eq!(&*ServoCookie::default_path(""), "/");
    assert_eq!(&*ServoCookie::default_path("foo"), "/");
}

#[test]
fn fn_cookie_constructor() {
    use net_traits::CookieSource;

    let url = &ServoUrl::parse("http://example.com/foo").unwrap();

    let gov_url = &ServoUrl::parse("http://gov.ac/foo").unwrap();
    // cookie name/value test
    assert!(cookie::Cookie::parse(" baz ").is_err());
    assert!(cookie::Cookie::parse(" = bar  ").is_err());
    assert!(cookie::Cookie::parse(" baz = ").is_ok());

    // cookie domains test
    let cookie = cookie::Cookie::parse(" baz = bar; Domain =  ").unwrap();
    assert!(ServoCookie::new_wrapped(cookie.clone(), url, CookieSource::HTTP).is_some());
    let cookie = ServoCookie::new_wrapped(cookie, url, CookieSource::HTTP).unwrap();
    assert_eq!(&**cookie.cookie.domain().as_ref().unwrap(), "example.com");

    // cookie public domains test
    let cookie = cookie::Cookie::parse(" baz = bar; Domain =  gov.ac").unwrap();
    assert!(ServoCookie::new_wrapped(cookie.clone(), url, CookieSource::HTTP).is_none());
    assert!(ServoCookie::new_wrapped(cookie, gov_url, CookieSource::HTTP).is_some());

    // cookie domain matching test
    let cookie = cookie::Cookie::parse(" baz = bar ; Secure; Domain = bazample.com").unwrap();
    assert!(ServoCookie::new_wrapped(cookie, url, CookieSource::HTTP).is_none());

    let cookie = cookie::Cookie::parse(" baz = bar ; Secure; Path = /foo/bar/").unwrap();
    assert!(
        ServoCookie::new_wrapped(cookie, url, CookieSource::HTTP).is_none(),
        "Cookie with \"Secure\" attribute from non-secure source should be rejected"
    );

    let cookie = cookie::Cookie::parse(" baz = bar ; HttpOnly").unwrap();
    assert!(ServoCookie::new_wrapped(cookie, url, CookieSource::NonHTTP).is_none());

    let secure_url = &ServoUrl::parse("https://example.com/foo").unwrap();
    let cookie = cookie::Cookie::parse(" baz = bar ; Secure; Path = /foo/bar/").unwrap();
    let cookie = ServoCookie::new_wrapped(cookie, secure_url, CookieSource::HTTP).unwrap();
    assert_eq!(cookie.cookie.value(), "bar");
    assert_eq!(cookie.cookie.name(), "baz");
    assert!(cookie.cookie.secure().unwrap_or(false));
    assert_eq!(&cookie.cookie.path().as_ref().unwrap()[..], "/foo/bar/");
    assert_eq!(&cookie.cookie.domain().as_ref().unwrap()[..], "example.com");
    assert!(cookie.host_only);

    let u = &ServoUrl::parse("http://example.com/foobar").unwrap();
    let cookie = cookie::Cookie::parse("foobar=value;path=/").unwrap();
    assert!(ServoCookie::new_wrapped(cookie, u, CookieSource::HTTP).is_some());

    let cookie = cookie::Cookie::parse("foo=bar; max-age=99999999999999999999999999999").unwrap();
    let cookie = ServoCookie::new_wrapped(cookie, u, CookieSource::HTTP).unwrap();
    assert!(
        cookie
            .expiry_time
            .is_some_and(|exp| exp < SystemTime::now() + Duration::from_secs(401 * 24 * 60 * 60))
    );
}

#[test]
fn test_cookie_secure_prefix() {
    let url = &ServoUrl::parse("https://example.com").unwrap();
    let cookie = cookie::Cookie::parse("__Secure-SID=12345").unwrap();
    assert!(ServoCookie::new_wrapped(cookie, url, CookieSource::HTTP).is_none());

    let url = &ServoUrl::parse("http://example.com").unwrap();
    let cookie = cookie::Cookie::parse("__Secure-SID=12345; Secure").unwrap();
    assert!(ServoCookie::new_wrapped(cookie, url, CookieSource::HTTP).is_none());

    let url = &ServoUrl::parse("https://example.com").unwrap();
    let cookie = cookie::Cookie::parse("__Secure-SID=12345; Secure").unwrap();
    assert!(ServoCookie::new_wrapped(cookie, url, CookieSource::HTTP).is_some());

    let url = &ServoUrl::parse("https://example.com").unwrap();
    let cookie = cookie::Cookie::parse("__Secure-SID=12345; Domain=example.com").unwrap();
    assert!(ServoCookie::new_wrapped(cookie, url, CookieSource::HTTP).is_none());

    let url = &ServoUrl::parse("http://example.com").unwrap();
    let cookie = cookie::Cookie::parse("__Secure-SID=12345; Secure; Domain=example.com").unwrap();
    assert!(ServoCookie::new_wrapped(cookie, url, CookieSource::HTTP).is_none());

    let url = &ServoUrl::parse("https://example.com").unwrap();
    let cookie = cookie::Cookie::parse("__Secure-SID=12345; Secure; Domain=example.com").unwrap();
    assert!(ServoCookie::new_wrapped(cookie, url, CookieSource::HTTP).is_some());
}

#[test]
fn test_cookie_host_prefix() {
    let url = &ServoUrl::parse("https://example.com").unwrap();
    let cookie = cookie::Cookie::parse("__Host-SID=12345").unwrap();
    assert!(ServoCookie::new_wrapped(cookie, url, CookieSource::HTTP).is_none());

    let url = &ServoUrl::parse("http://example.com").unwrap();
    let cookie = cookie::Cookie::parse("__Host-SID=12345; Secure").unwrap();
    assert!(ServoCookie::new_wrapped(cookie, url, CookieSource::HTTP).is_none());

    let url = &ServoUrl::parse("https://example.com").unwrap();
    let cookie = cookie::Cookie::parse("__Host-SID=12345; Secure").unwrap();
    assert!(ServoCookie::new_wrapped(cookie, url, CookieSource::HTTP).is_none());

    let url = &ServoUrl::parse("https://example.com").unwrap();
    let cookie = cookie::Cookie::parse("__Host-SID=12345; Domain=example.com").unwrap();
    assert!(ServoCookie::new_wrapped(cookie, url, CookieSource::HTTP).is_none());

    let url = &ServoUrl::parse("https://example.com").unwrap();
    let cookie = cookie::Cookie::parse("__Host-SID=12345; Domain=example.com; Path=/").unwrap();
    assert!(ServoCookie::new_wrapped(cookie, url, CookieSource::HTTP).is_none());

    let url = &ServoUrl::parse("http://example.com").unwrap();
    let cookie = cookie::Cookie::parse("__Host-SID=12345; Secure; Domain=example.com").unwrap();
    assert!(ServoCookie::new_wrapped(cookie, url, CookieSource::HTTP).is_none());

    let url = &ServoUrl::parse("https://example.com").unwrap();
    let cookie = cookie::Cookie::parse("__Host-SID=12345; Secure; Domain=example.com").unwrap();
    assert!(ServoCookie::new_wrapped(cookie, url, CookieSource::HTTP).is_none());

    let url = &ServoUrl::parse("https://example.com").unwrap();
    let cookie =
        cookie::Cookie::parse("__Host-SID=12345; Secure; Domain=example.com; Path=/").unwrap();
    assert!(ServoCookie::new_wrapped(cookie, url, CookieSource::HTTP).is_none());

    let url = &ServoUrl::parse("https://example.com").unwrap();
    let cookie = cookie::Cookie::parse("__Host-SID=12345; Secure; Path=/").unwrap();
    assert!(ServoCookie::new_wrapped(cookie, url, CookieSource::HTTP).is_some());
}

fn delay_to_ensure_different_timestamp() {
    use std::thread;
    use std::time::Duration;

    // time::now()'s resolution on some platforms isn't granular enought to ensure
    // that two back-to-back calls to Cookie::new_wrapped generate different timestamps .
    thread::sleep(Duration::from_millis(500));
}

#[test]
fn test_sort_order() {
    use std::cmp::Ordering;

    let url = &ServoUrl::parse("http://example.com/foo").unwrap();
    let a_wrapped = cookie::Cookie::parse("baz=bar; Path=/foo/bar/").unwrap();
    let a = ServoCookie::new_wrapped(a_wrapped.clone(), url, CookieSource::HTTP).unwrap();
    delay_to_ensure_different_timestamp();
    let a_prime = ServoCookie::new_wrapped(a_wrapped, url, CookieSource::HTTP).unwrap();
    let b = cookie::Cookie::parse("baz=bar;Path=/foo/bar/baz/").unwrap();
    let b = ServoCookie::new_wrapped(b, url, CookieSource::HTTP).unwrap();

    assert!(b.cookie.path().as_ref().unwrap().len() > a.cookie.path().as_ref().unwrap().len());
    assert_eq!(CookieStorage::cookie_comparator(&a, &b), Ordering::Greater);
    assert_eq!(CookieStorage::cookie_comparator(&b, &a), Ordering::Less);
    assert_eq!(
        CookieStorage::cookie_comparator(&a, &a_prime),
        Ordering::Less
    );
    assert_eq!(
        CookieStorage::cookie_comparator(&a_prime, &a),
        Ordering::Greater
    );
    assert_eq!(CookieStorage::cookie_comparator(&a, &a), Ordering::Equal);
}

fn add_cookie_to_storage(storage: &mut CookieStorage, url: &ServoUrl, cookie_str: &str) {
    let source = CookieSource::HTTP;
    let cookie = cookie::Cookie::parse(cookie_str.to_owned()).unwrap();
    let cookie = ServoCookie::new_wrapped(cookie, url, source).unwrap();
    storage.push(cookie, url, source);
}

fn controlled_cookie_context(
    policy: ControlledCookiePolicy,
    site_for_cookies: Option<&str>,
    top_level_navigation: bool,
) -> ControlledCookieContext {
    ControlledCookieContext {
        policy,
        site_for_cookies: site_for_cookies
            .map(|url| ServoUrl::parse(url).unwrap().as_url().clone()),
        top_level_navigation,
    }
}

fn state_cookie(
    name: &str,
    domain: &str,
    path: &str,
    creation_sequence: u64,
) -> CookieStateRecordV1 {
    CookieStateRecordV1 {
        name: name.into(),
        value: format!("{name}-secret"),
        domain: domain.into(),
        path: path.into(),
        host_only: true,
        secure: true,
        http_only: true,
        same_site: CookieStateSameSite::Lax,
        expires_unix_time_ns: None,
        partitioned: false,
        creation_sequence,
        last_access_sequence: creation_sequence + 10,
    }
}

fn cookie_backend_encoded_array_bytes(cookies: &[CookieStateRecordV1]) -> usize {
    serde_json::to_vec(cookies).unwrap().len()
}

fn cookie_public_encoded_array_bytes(cookies: &[CookieStateRecordV1]) -> usize {
    let projected: Vec<_> = cookies
        .iter()
        .map(|cookie| {
            json!({
                "name": cookie.name,
                "value": cookie.value,
                "domain": cookie.domain,
                "path": cookie.path,
                "hostOnly": cookie.host_only,
                "secure": cookie.secure,
                "httpOnly": cookie.http_only,
                "sameSite": cookie.same_site,
                "expiresUnixTimeNs": cookie.expires_unix_time_ns.map(|value| value.to_string()),
                "partitioned": cookie.partitioned,
                "creationSequence": cookie.creation_sequence.to_string(),
                "lastAccessSequence": cookie.last_access_sequence.to_string(),
            })
        })
        .collect();
    serde_json::to_vec(&projected).unwrap().len()
}

fn maximum_admitted_cookie_fragment() -> Vec<CookieStateRecordV1> {
    let mut cookies = Vec::new();
    while cookies.len() < COOKIE_STATE_MAX_COOKIES_V1 {
        let index = cookies.len();
        let name = format!("budget-{index:03}");
        let mut cookie = state_cookie(&name, "example.com", "/", index as u64);
        cookie.http_only = false;
        cookie.last_access_sequence = index as u64;
        let fixed_bytes = cookie.name.len() + cookie.domain.len() + cookie.path.len();
        cookie.value = "x".repeat(COOKIE_STATE_MAX_COOKIE_BYTES_V1 - fixed_bytes);
        cookies.push(cookie);
        if cookie_public_encoded_array_bytes(&cookies)
            <= COOKIE_STATE_MAX_ENCODED_PUBLIC_ARRAY_BYTES_V1
        {
            continue;
        }

        let mut cookie = cookies.pop().unwrap();
        cookie.value.clear();
        cookies.push(cookie);
        let minimum_bytes = cookie_public_encoded_array_bytes(&cookies);
        if minimum_bytes > COOKIE_STATE_MAX_ENCODED_PUBLIC_ARRAY_BYTES_V1 {
            cookies.pop();
            break;
        }
        let fill_bytes = (COOKIE_STATE_MAX_ENCODED_PUBLIC_ARRAY_BYTES_V1 - minimum_bytes)
            .min(COOKIE_STATE_MAX_COOKIE_BYTES_V1 - fixed_bytes);
        cookies.last_mut().unwrap().value = "x".repeat(fill_bytes);
        break;
    }
    cookies
}

#[test]
fn cookie_state_replace_is_canonical_and_preserves_metadata() {
    let mut storage = CookieStorage::new(150);
    let snapshot = CookieStateSnapshotV1 {
        schema_version: COOKIE_STATE_SCHEMA_VERSION_V1,
        revision: 75,
        cookies: vec![
            state_cookie("second", "example.com", "/account", 2),
            state_cookie("first", "example.com", "/", 1),
        ],
    };

    assert_eq!(storage.replace_state(0, snapshot), Ok(1));
    let exported = storage.export_state().unwrap();
    assert_eq!(exported.revision, 1);
    assert_eq!(exported.cookies[0].name, "first");
    assert_eq!(exported.cookies[1].name, "second");
    assert!(exported.cookies.iter().all(|cookie| {
        cookie.host_only
            && cookie.secure
            && cookie.http_only
            && cookie.same_site == CookieStateSameSite::Lax
            && cookie.expires_unix_time_ns.is_none()
            && !cookie.partitioned
    }));
    assert_eq!(exported.cookies[0].creation_sequence, 0);
    assert_eq!(exported.cookies[1].creation_sequence, 1);
    assert_eq!(exported.cookies[0].last_access_sequence, 0);
    assert_eq!(exported.cookies[1].last_access_sequence, 1);
    assert!(!format!("{exported:?}").contains("secret"));
    assert!(!format!("{:?}", exported.cookies[0]).contains("first"));
}

#[test]
fn cookie_state_rejects_stale_persistent_and_partitioned_atomically() {
    let mut storage = CookieStorage::new(150);
    let initial = CookieStateSnapshotV1 {
        schema_version: COOKIE_STATE_SCHEMA_VERSION_V1,
        revision: 0,
        cookies: vec![state_cookie("session", "example.com", "/", 1)],
    };
    assert_eq!(storage.replace_state(0, initial), Ok(1));
    let before = storage.export_state().unwrap();

    assert_eq!(
        storage.replace_state(0, before.clone()),
        Err(CookieStateError::StaleRevision)
    );
    assert_eq!(storage.export_state().unwrap(), before);

    let mut persistent = before.clone();
    persistent.cookies[0].expires_unix_time_ns = Some(1);
    assert_eq!(
        storage.replace_state(1, persistent),
        Err(CookieStateError::PersistentCookieUnsupported)
    );
    assert_eq!(storage.export_state().unwrap(), before);

    let mut partitioned = before.clone();
    partitioned.cookies[0].partitioned = true;
    assert_eq!(
        storage.replace_state(1, partitioned),
        Err(CookieStateError::PartitionedCookieUnsupported)
    );
    assert_eq!(storage.export_state().unwrap(), before);
}

#[test]
fn cookie_state_rejects_invalid_request_wire_pairs_atomically() {
    let mut storage = CookieStorage::new(150);
    let initial = CookieStateSnapshotV1 {
        schema_version: COOKIE_STATE_SCHEMA_VERSION_V1,
        revision: 0,
        cookies: vec![state_cookie("session", "example.com", "/", 1)],
    };
    assert_eq!(storage.replace_state(0, initial), Ok(1));
    let before = storage.export_state().unwrap();

    for (name, value) in [
        ("bad name", "value"),
        ("bad=name", "value"),
        ("bad;name", "value"),
        ("bad\r\nname", "value"),
        ("valid", "bad;value"),
        ("valid", "bad,value"),
        ("valid", "bad\\value"),
        ("valid", "bad\r\nvalue"),
    ] {
        let mut invalid = before.clone();
        invalid.cookies[0].name = name.into();
        invalid.cookies[0].value = value.into();
        assert_eq!(
            storage.replace_state(1, invalid),
            Err(CookieStateError::InvalidCookie),
            "accepted invalid cookie pair {name:?}={value:?}"
        );
        assert_eq!(storage.export_state().unwrap(), before);
    }
}

#[test]
fn cookie_state_ipv6_domain_projection_is_canonical_and_round_trips() {
    assert!(net_traits::is_canonical_cookie_state_domain("2001:db8::1"));
    let mut storage = CookieStorage::new(150);
    let mut host_only = state_cookie("host-only", "2001:db8::1", "/", 0);
    host_only.http_only = false;
    let mut domain_cookie = state_cookie("domain", "2001:db8::1", "/", 1);
    domain_cookie.host_only = false;
    domain_cookie.http_only = false;
    domain_cookie.last_access_sequence = 11;

    assert_eq!(
        storage.replace_state(
            0,
            CookieStateSnapshotV1 {
                schema_version: COOKIE_STATE_SCHEMA_VERSION_V1,
                revision: 0,
                cookies: vec![host_only, domain_cookie],
            },
        ),
        Ok(1),
    );
    let request = ServoUrl::parse("https://[2001:db8::1]/").unwrap();
    let header = storage
        .cookies_for_url(&request, CookieSource::NonHTTP)
        .unwrap();
    assert!(header.contains("host-only=host-only-secret"));
    assert!(header.contains("domain=domain-secret"));
    let exported = storage.export_state().unwrap();
    assert!(
        exported
            .cookies
            .iter()
            .all(|cookie| cookie.domain == "2001:db8::1")
    );

    let mut bracketed = exported.clone();
    bracketed.cookies[0].domain = "[2001:db8::1]".into();
    assert_eq!(
        storage.replace_state(exported.revision, bracketed),
        Err(CookieStateError::InvalidCookie),
    );
    assert_eq!(storage.export_state().unwrap(), exported);
}

#[test]
fn controlled_cookie_reads_preserve_the_exact_public_fragment_budget() {
    let cookies = maximum_admitted_cookie_fragment();
    assert_eq!(
        cookie_public_encoded_array_bytes(&cookies),
        COOKIE_STATE_MAX_ENCODED_PUBLIC_ARRAY_BYTES_V1,
    );
    let mut storage = CookieStorage::new(150);
    storage
        .replace_state(
            0,
            CookieStateSnapshotV1 {
                schema_version: COOKIE_STATE_SCHEMA_VERSION_V1,
                revision: 0,
                cookies,
            },
        )
        .unwrap();
    let request = ServoUrl::parse("https://example.com/").unwrap();

    assert!(
        storage
            .controlled_session_cookies_for_url(&request, &request, CookieSource::HTTP)
            .unwrap()
            .is_some(),
    );
    let after_request = storage.export_state().unwrap();
    assert_eq!(
        cookie_public_encoded_array_bytes(&after_request.cookies),
        COOKIE_STATE_MAX_ENCODED_PUBLIC_ARRAY_BYTES_V1,
    );

    assert!(
        storage
            .controlled_session_cookies_for_url(&request, &request, CookieSource::NonHTTP)
            .unwrap()
            .is_some(),
    );
    let after_document_cookie = storage.export_state().unwrap();
    assert_eq!(
        cookie_public_encoded_array_bytes(&after_document_cookie.cookies),
        COOKIE_STATE_MAX_ENCODED_PUBLIC_ARRAY_BYTES_V1,
    );
}

fn assert_ip_cookie_bucket_collision_eviction(ip_a: &str, ip_b: &str) {
    let mut storage = CookieStorage::new(5);
    let ip_a = ServoUrl::parse(ip_a).unwrap();
    let ip_b = ServoUrl::parse(ip_b).unwrap();
    let source = CookieSource::HTTP;

    for i in 1..=3 {
        add_cookie_to_storage(&mut storage, &ip_a, &format!("a{i}=val{i}"));
    }

    for i in 1..=5 {
        add_cookie_to_storage(&mut storage, &ip_b, &format!("b{i}=val{i}"));
    }

    let cookies_a = storage.cookies_for_url(&ip_a, source).unwrap();
    assert_eq!(cookies_a.split("; ").count(), 3);
    for i in 1..=3 {
        assert!(cookies_a.contains(&format!("a{i}=val{i}")));
    }
}

#[test]
fn test_ip_cookie_bucket_collision_eviction() {
    assert_ip_cookie_bucket_collision_eviction("http://192.168.0.1/path", "http://10.0.0.1/path");
    assert_ip_cookie_bucket_collision_eviction(
        "http://[2001:db8::1]/path",
        "http://[2001:db8::2]/path",
    );
}

#[test]
fn test_insecure_cookies_cannot_evict_secure_cookie() {
    let mut storage = CookieStorage::new(5);
    let secure_url = ServoUrl::parse("https://home.example.org:8888/cookie-parser?0001").unwrap();
    let source = CookieSource::HTTP;
    let mut cookies = Vec::new();

    cookies.push(cookie::Cookie::parse("foo=bar; Secure; Domain=home.example.org").unwrap());
    cookies.push(cookie::Cookie::parse("foo2=bar; Secure; Domain=.example.org").unwrap());
    cookies.push(cookie::Cookie::parse("foo3=bar; Secure; Path=/foo").unwrap());
    cookies.push(cookie::Cookie::parse("foo4=bar; Secure; Path=/foo/bar").unwrap());

    for bare_cookie in cookies {
        let cookie = ServoCookie::new_wrapped(bare_cookie, &secure_url, source).unwrap();
        storage.push(cookie, &secure_url, source);
    }

    let insecure_url = ServoUrl::parse("http://home.example.org:8888/cookie-parser?0001").unwrap();

    add_cookie_to_storage(
        &mut storage,
        &insecure_url,
        "foo=value; Domain=home.example.org",
    );
    add_cookie_to_storage(
        &mut storage,
        &insecure_url,
        "foo2=value; Domain=.example.org",
    );
    add_cookie_to_storage(&mut storage, &insecure_url, "foo3=value; Path=/foo/bar");
    add_cookie_to_storage(&mut storage, &insecure_url, "foo4=value; Path=/foo");

    let source = CookieSource::HTTP;
    assert_eq!(
        storage.cookies_for_url(&secure_url, source).unwrap(),
        "foo=bar; foo2=bar"
    );

    let url =
        ServoUrl::parse("https://home.example.org:8888/foo/cookie-parser-result?0001").unwrap();
    let source = CookieSource::HTTP;
    assert_eq!(
        storage.cookies_for_url(&url, source).unwrap(),
        "foo3=bar; foo4=value; foo=bar; foo2=bar"
    );

    let url =
        ServoUrl::parse("https://home.example.org:8888/foo/bar/cookie-parser-result?0001").unwrap();
    let source = CookieSource::HTTP;
    assert_eq!(
        storage.cookies_for_url(&url, source).unwrap(),
        "foo4=bar; foo3=bar; foo4=value; foo=bar; foo2=bar"
    );
}

#[test]
fn test_secure_cookies_eviction() {
    let mut storage = CookieStorage::new(5);
    let url = ServoUrl::parse("https://home.example.org:8888/cookie-parser?0001").unwrap();
    let source = CookieSource::HTTP;
    let mut cookies = Vec::new();

    cookies.push(cookie::Cookie::parse("foo=bar; Secure; Domain=home.example.org").unwrap());
    cookies.push(cookie::Cookie::parse("foo2=bar; Secure; Domain=.example.org").unwrap());
    cookies.push(cookie::Cookie::parse("foo3=bar; Secure; Path=/foo").unwrap());
    cookies.push(cookie::Cookie::parse("foo4=bar; Secure; Path=/foo/bar").unwrap());

    for bare_cookie in cookies {
        let cookie = ServoCookie::new_wrapped(bare_cookie, &url, source).unwrap();
        storage.push(cookie, &url, source);
    }

    add_cookie_to_storage(&mut storage, &url, "foo=value; Domain=home.example.org");
    add_cookie_to_storage(&mut storage, &url, "foo2=value; Domain=.example.org");
    add_cookie_to_storage(&mut storage, &url, "foo3=value; Path=/foo/bar");
    add_cookie_to_storage(&mut storage, &url, "foo4=value; Path=/foo");

    let source = CookieSource::HTTP;
    assert_eq!(storage.cookies_for_url(&url, source).unwrap(), "foo2=value");

    let url =
        ServoUrl::parse("https://home.example.org:8888/foo/cookie-parser-result?0001").unwrap();
    let source = CookieSource::HTTP;
    assert_eq!(
        storage.cookies_for_url(&url, source).unwrap(),
        "foo3=bar; foo4=value; foo2=value"
    );

    let url =
        ServoUrl::parse("https://home.example.org:8888/foo/bar/cookie-parser-result?0001").unwrap();
    let source = CookieSource::HTTP;
    assert_eq!(
        storage.cookies_for_url(&url, source).unwrap(),
        "foo4=bar; foo3=value; foo3=bar; foo4=value; foo2=value"
    );
}

#[test]
fn test_secure_cookies_eviction_non_http_source() {
    let mut storage = CookieStorage::new(5);
    let url = ServoUrl::parse("https://home.example.org:8888/cookie-parser?0001").unwrap();
    let source = CookieSource::NonHTTP;
    let mut cookies = Vec::new();

    cookies.push(cookie::Cookie::parse("foo=bar; Secure; Domain=home.example.org").unwrap());
    cookies.push(cookie::Cookie::parse("foo2=bar; Secure; Domain=.example.org").unwrap());
    cookies.push(cookie::Cookie::parse("foo3=bar; Secure; Path=/foo").unwrap());
    cookies.push(cookie::Cookie::parse("foo4=bar; Secure; Path=/foo/bar").unwrap());

    for bare_cookie in cookies {
        let cookie = ServoCookie::new_wrapped(bare_cookie, &url, source).unwrap();
        storage.push(cookie, &url, source);
    }

    add_cookie_to_storage(&mut storage, &url, "foo=value; Domain=home.example.org");
    add_cookie_to_storage(&mut storage, &url, "foo2=value; Domain=.example.org");
    add_cookie_to_storage(&mut storage, &url, "foo3=value; Path=/foo/bar");
    add_cookie_to_storage(&mut storage, &url, "foo4=value; Path=/foo");

    let source = CookieSource::HTTP;
    assert_eq!(storage.cookies_for_url(&url, source).unwrap(), "foo2=value");

    let url =
        ServoUrl::parse("https://home.example.org:8888/foo/cookie-parser-result?0001").unwrap();
    let source = CookieSource::HTTP;
    assert_eq!(
        storage.cookies_for_url(&url, source).unwrap(),
        "foo3=bar; foo4=value; foo2=value"
    );

    let url =
        ServoUrl::parse("https://home.example.org:8888/foo/bar/cookie-parser-result?0001").unwrap();
    let source = CookieSource::HTTP;
    assert_eq!(
        storage.cookies_for_url(&url, source).unwrap(),
        "foo4=bar; foo3=value; foo3=bar; foo4=value; foo2=value"
    );
}

fn add_retrieve_cookies(
    set_location: &str,
    set_cookies: &[String],
    final_location: &str,
) -> String {
    let mut storage = CookieStorage::new(5);
    let url = ServoUrl::parse(set_location).unwrap();
    let source = CookieSource::HTTP;

    // Add all cookies to the store
    for str_cookie in set_cookies {
        let cookie = ServoCookie::from_cookie_string(str_cookie, &url, source).unwrap();
        storage.push(cookie, &url, source);
    }

    // Get cookies for the test location
    let url = ServoUrl::parse(final_location).unwrap();
    storage
        .cookies_for_url(&url, source)
        .unwrap_or("".to_string())
}

#[test]
fn test_cookie_eviction_expired() {
    let mut vec = Vec::new();
    for i in 1..6 {
        let st = format!(
            "extra{}=bar; Secure; expires=Sun, 18-Apr-2000 21:06:29 GMT",
            i
        );
        vec.push(st);
    }
    vec.push("foo=bar; Secure; expires=Sun, 18-Apr-2127 21:06:29 GMT".to_owned());
    let r = add_retrieve_cookies(
        "https://home.example.org:8888/cookie-parser?0001",
        &vec,
        "https://home.example.org:8888/cookie-parser-result?0001",
    );
    assert_eq!(&r, "foo=bar");
}

#[test]
fn test_cookie_eviction_all_secure_one_nonsecure() {
    let mut vec = Vec::new();
    for i in 1..5 {
        let st = format!(
            "extra{}=bar; Secure; expires=Sun, 18-Apr-2126 21:06:29 GMT",
            i
        );
        vec.push(st);
    }
    vec.push("foo=bar; expires=Sun, 18-Apr-2126 21:06:29 GMT".to_owned());
    vec.push("foo2=bar; Secure; expires=Sun, 18-Apr-2128 21:06:29 GMT".to_owned());
    let r = add_retrieve_cookies(
        "https://home.example.org:8888/cookie-parser?0001",
        &vec,
        "https://home.example.org:8888/cookie-parser-result?0001",
    );
    assert_eq!(
        &r,
        "extra1=bar; extra2=bar; extra3=bar; extra4=bar; foo2=bar"
    );
}

#[test]
fn test_cookie_eviction_all_secure_new_nonsecure() {
    let mut vec = Vec::new();
    for i in 1..6 {
        let st = format!(
            "extra{}=bar; Secure; expires=Sun, 18-Apr-2126 21:06:29 GMT",
            i
        );
        vec.push(st);
    }
    vec.push("foo=bar; expires=Sun, 18-Apr-2177 21:06:29 GMT".to_owned());
    let r = add_retrieve_cookies(
        "https://home.example.org:8888/cookie-parser?0001",
        &vec,
        "https://home.example.org:8888/cookie-parser-result?0001",
    );
    assert_eq!(
        &r,
        "extra1=bar; extra2=bar; extra3=bar; extra4=bar; extra5=bar"
    );
}

#[test]
fn test_cookie_eviction_all_nonsecure_new_secure() {
    let mut vec = Vec::new();
    for i in 1..6 {
        let st = format!("extra{}=bar; expires=Sun, 18-Apr-2126 21:06:29 GMT", i);
        vec.push(st);
    }
    vec.push("foo=bar; Secure; expires=Sun, 18-Apr-2177 21:06:29 GMT".to_owned());
    let r = add_retrieve_cookies(
        "https://home.example.org:8888/cookie-parser?0001",
        &vec,
        "https://home.example.org:8888/cookie-parser-result?0001",
    );
    assert_eq!(
        &r,
        "extra2=bar; extra3=bar; extra4=bar; extra5=bar; foo=bar"
    );
}

#[test]
fn test_cookie_eviction_all_nonsecure_new_nonsecure() {
    let mut vec = Vec::new();
    for i in 1..6 {
        let st = format!("extra{}=bar; expires=Sun, 18-Apr-2126 21:06:29 GMT", i);
        vec.push(st);
    }
    vec.push("foo=bar; expires=Sun, 18-Apr-2177 21:06:29 GMT".to_owned());
    let r = add_retrieve_cookies(
        "https://home.example.org:8888/cookie-parser?0001",
        &vec,
        "https://home.example.org:8888/cookie-parser-result?0001",
    );
    assert_eq!(
        &r,
        "extra2=bar; extra3=bar; extra4=bar; extra5=bar; foo=bar"
    );
}

#[test]
fn test_parse_date() {
    assert_eq!(
        ServoCookie::parse_date("26 Jun 2024 15:35:10 GMT"), // without day of week
        Some(datetime!(2024-06-26 15:35:10).assume_utc())
    );
    assert_eq!(
        ServoCookie::parse_date("26-Jun-2024 15:35:10 GMT"), // dashed
        Some(datetime!(2024-06-26 15:35:10).assume_utc())
    );
    assert_eq!(
        ServoCookie::parse_date("26 Jun 2024 15:35:10"), // no GMT
        Some(datetime!(2024-06-26 15:35:10).assume_utc())
    );
    assert_eq!(
        ServoCookie::parse_date("26 Jun 24 15:35:10 GMT"), // 2-digit year
        Some(datetime!(2024-06-26 15:35:10).assume_utc())
    );
    assert_eq!(
        ServoCookie::parse_date("26 jun 2024 15:35:10 gmt"), // Lowercase
        Some(datetime!(2024-06-26 15:35:10).assume_utc())
    );
}

#[test]
fn test_clear_storage_for_url_expires_matching_cookies() {
    let mut storage = CookieStorage::new(5);
    let source = CookieSource::HTTP;
    let url = ServoUrl::parse("http://example.com/").unwrap();

    add_cookie_to_storage(&mut storage, &url, "foo=bar");
    assert_eq!(
        storage.cookies_for_url(&url, source).as_deref(),
        Some("foo=bar")
    );

    storage.clear_storage(Some(&url));

    storage.remove_expired_cookies_for_url(&url);
    assert_eq!(storage.cookies_for_url(&url, source), None);
}

#[test]
fn test_clear_storage_without_url_clears_everything() {
    let mut storage = CookieStorage::new(5);
    let source = CookieSource::HTTP;
    let url = ServoUrl::parse("http://example.com/").unwrap();
    let other_url = ServoUrl::parse("http://example.org/").unwrap();

    add_cookie_to_storage(&mut storage, &url, "foo=bar");
    add_cookie_to_storage(&mut storage, &other_url, "baz=qux");

    storage.clear_storage(None);

    assert!(storage.cookie_site_descriptors().is_empty());
    assert_eq!(storage.cookies_for_url(&url, source), None);
    assert_eq!(storage.cookies_for_url(&other_url, source), None);
}

#[test]
fn test_delete_cookie_with_name_expires_only_matching_cookie() {
    let mut storage = CookieStorage::new(5);
    let source = CookieSource::HTTP;
    let url = ServoUrl::parse("http://example.com/").unwrap();

    add_cookie_to_storage(&mut storage, &url, "foo=bar");
    add_cookie_to_storage(&mut storage, &url, "baz=qux");

    storage.delete_cookie_with_name(&url, "foo".to_owned());

    storage.remove_expired_cookies_for_url(&url);
    assert_eq!(
        storage.cookies_for_url(&url, source).as_deref(),
        Some("baz=qux")
    );
}

#[test]
fn test_delete_cookie_with_name_does_not_affect_other_domains() {
    let mut storage = CookieStorage::new(5);
    let source = CookieSource::HTTP;
    let url = ServoUrl::parse("http://example.com/").unwrap();
    let other_url = ServoUrl::parse("http://example.org/").unwrap();

    add_cookie_to_storage(&mut storage, &url, "foo=bar");
    add_cookie_to_storage(&mut storage, &other_url, "foo=bar");

    storage.delete_cookie_with_name(&url, "foo".to_owned());

    storage.remove_all_expired_cookies();
    assert_eq!(storage.cookies_for_url(&url, source), None);
    assert_eq!(
        storage.cookies_for_url(&other_url, source).as_deref(),
        Some("foo=bar")
    );
}

#[test]
fn controlled_session_cookie_policy_is_schemeful_and_same_site() {
    let mut storage = CookieStorage::new(5);
    let request = ServoUrl::parse("https://api.example.com/account").unwrap();
    let same_site = ServoUrl::parse("https://www.example.com/").unwrap();
    let cross_scheme = ServoUrl::parse("http://www.example.com/").unwrap();
    let cross_site = ServoUrl::parse("https://example.org/").unwrap();

    storage
        .set_controlled_session_cookie_from_header(
            &request,
            &same_site,
            "strict=value; Secure; SameSite=Strict",
        )
        .unwrap();
    assert_eq!(
        storage
            .controlled_session_cookies_for_url(&request, &same_site, CookieSource::HTTP)
            .unwrap()
            .as_deref(),
        Some("strict=value")
    );
    assert_eq!(
        storage.controlled_session_cookies_for_url(&request, &cross_scheme, CookieSource::HTTP,),
        Err(ControlledCookiePolicyError::SameSiteContextUnsupported)
    );
    assert_eq!(
        storage.set_controlled_session_cookie_from_header(
            &request,
            &cross_site,
            "never=stored; Secure",
        ),
        Err(ControlledCookiePolicyError::SameSiteContextUnsupported)
    );
}

#[test]
fn controlled_session_rejects_uncontrolled_attributes_before_jar_mutation() {
    let mut storage = CookieStorage::new(5);
    let request = ServoUrl::parse("https://example.com/").unwrap();
    let initial_revision = storage.export_state().unwrap().revision;

    assert_eq!(
        storage.set_controlled_session_cookies_from_headers(
            &request,
            &request,
            &[
                "would-have-been-stored=value; Secure",
                "persistent=value; Max-Age=60",
            ],
        ),
        Err(ControlledCookiePolicyError::PersistentCookieUnsupported)
    );

    for cookie in [
        "persistent=value; Expires=Wed, 21 Oct 2037 07:28:00 GMT",
        "persistent=value; mAx-AgE=60",
    ] {
        assert_eq!(
            storage.set_controlled_session_cookie_from_header(&request, &request, cookie),
            Err(ControlledCookiePolicyError::PersistentCookieUnsupported)
        );
    }
    assert_eq!(
        storage.set_controlled_session_cookie_from_header(
            &request,
            &request,
            "partitioned=value; Secure; PARTITIONED",
        ),
        Err(ControlledCookiePolicyError::PartitionedCookieUnsupported)
    );
    let final_state = storage.export_state().unwrap();
    assert_eq!(final_state.revision, initial_revision);
    assert!(final_state.cookies.is_empty());
}

#[test]
fn controlled_non_http_cookie_writes_cannot_poison_session_export() {
    let mut storage = CookieStorage::new(5);
    let document_url = ServoUrl::parse("https://example.com/account").unwrap();

    storage
        .set_controlled_session_cookie_from_non_http(
            &document_url,
            &document_url,
            "page-session=valid; Path=/; Secure; SameSite=Lax",
        )
        .unwrap();
    assert_eq!(
        storage
            .controlled_session_cookies_for_url(
                &document_url,
                &document_url,
                CookieSource::NonHTTP,
            )
            .unwrap()
            .as_deref(),
        Some("page-session=valid")
    );
    storage
        .set_controlled_session_cookie_from_non_http(
            &document_url,
            &document_url,
            "page-session=updated; Path=/; Secure; SameSite=Lax",
        )
        .unwrap();
    assert_eq!(
        storage
            .controlled_session_cookies_for_url(
                &document_url,
                &document_url,
                CookieSource::NonHTTP,
            )
            .unwrap()
            .as_deref(),
        Some("page-session=updated")
    );
    let valid_state = storage.export_state().unwrap();
    assert_eq!(valid_state.cookies.len(), 1);
    assert_eq!(valid_state.cookies[0].name, "page-session");
    assert_eq!(valid_state.cookies[0].value, "updated");

    assert_eq!(
        storage.set_controlled_session_cookie_from_non_http(
            &document_url,
            &document_url,
            "persistent=blocked; Max-Age=60; Secure",
        ),
        Err(ControlledCookiePolicyError::PersistentCookieUnsupported)
    );
    assert_eq!(storage.export_state().unwrap(), valid_state);

    assert_eq!(
        storage.set_controlled_session_cookie_from_non_http(
            &document_url,
            &document_url,
            "partitioned=blocked; Partitioned; Secure; SameSite=None",
        ),
        Err(ControlledCookiePolicyError::PartitionedCookieUnsupported)
    );
    assert_eq!(storage.export_state().unwrap(), valid_state);

    let oversized_path = format!(
        "oversized=blocked; Path=/{}; Secure",
        "x".repeat(COOKIE_STATE_MAX_COOKIE_BYTES_V1),
    );
    assert_eq!(
        storage.set_controlled_session_cookie_from_non_http(
            &document_url,
            &document_url,
            &oversized_path,
        ),
        Err(ControlledCookiePolicyError::InvalidCookie)
    );
    assert_eq!(storage.export_state().unwrap(), valid_state);
}

#[test]
fn controlled_cookie_mutations_enforce_global_snapshot_bounds_atomically() {
    let document_url = ServoUrl::parse("https://example.com/account").unwrap();

    let mut count_limited = CookieStorage::new(COOKIE_STATE_MAX_COOKIES_V1 + 1);
    let mut count_records: Vec<_> = (0..COOKIE_STATE_MAX_COOKIES_V1)
        .map(|index| state_cookie(&format!("count-{index}"), "example.com", "/", index as u64))
        .collect();
    count_records[0].http_only = false;
    let count_snapshot = CookieStateSnapshotV1 {
        schema_version: COOKIE_STATE_SCHEMA_VERSION_V1,
        revision: 0,
        cookies: count_records,
    };
    count_limited.replace_state(0, count_snapshot).unwrap();
    count_limited
        .set_controlled_session_cookie_from_non_http(
            &document_url,
            &document_url,
            "count-0=replaced-at-limit; Path=/; Secure",
        )
        .unwrap();
    let count_baseline = count_limited.export_state().unwrap();
    assert_eq!(count_baseline.cookies.len(), COOKIE_STATE_MAX_COOKIES_V1);
    assert_eq!(count_baseline.cookies[0].value, "replaced-at-limit");
    assert_eq!(
        count_limited.set_controlled_session_cookie_from_non_http(
            &document_url,
            &document_url,
            "one-too-many=blocked; Path=/; Secure",
        ),
        Err(ControlledCookiePolicyError::InvalidCookie)
    );
    assert_eq!(count_limited.export_state().unwrap(), count_baseline);

    let mut byte_limited = CookieStorage::new(150);
    let byte_records = maximum_admitted_cookie_fragment();
    assert!(
        cookie_public_encoded_array_bytes(&byte_records)
            <= COOKIE_STATE_MAX_ENCODED_PUBLIC_ARRAY_BYTES_V1
    );
    let mut over_budget_records = byte_records.clone();
    let next_index = over_budget_records.len();
    over_budget_records.push(state_cookie(
        &format!("budget-{next_index:03}"),
        "example.com",
        "/",
        next_index as u64,
    ));
    assert!(
        cookie_public_encoded_array_bytes(&over_budget_records)
            > COOKIE_STATE_MAX_ENCODED_PUBLIC_ARRAY_BYTES_V1
    );
    let mut rejected_import = CookieStorage::new(150);
    assert_eq!(
        rejected_import.replace_state(
            0,
            CookieStateSnapshotV1 {
                schema_version: COOKIE_STATE_SCHEMA_VERSION_V1,
                revision: 0,
                cookies: over_budget_records,
            },
        ),
        Err(CookieStateError::SnapshotTooLarge)
    );
    assert!(rejected_import.export_state().unwrap().cookies.is_empty());

    let first_byte_value = byte_records[0].value.clone();
    let byte_snapshot = CookieStateSnapshotV1 {
        schema_version: COOKIE_STATE_SCHEMA_VERSION_V1,
        revision: 0,
        cookies: byte_records,
    };
    byte_limited.replace_state(0, byte_snapshot).unwrap();
    let byte_baseline = byte_limited.export_state().unwrap();
    assert_eq!(
        cookie_public_encoded_array_bytes(&byte_baseline.cookies),
        COOKIE_STATE_MAX_ENCODED_PUBLIC_ARRAY_BYTES_V1,
    );
    let last_cookie = byte_baseline.cookies.last().unwrap();
    assert_eq!(
        byte_limited.set_controlled_session_cookie_from_non_http(
            &document_url,
            &document_url,
            &format!(
                "{}={}x; Path=/; Secure; SameSite=Lax",
                last_cookie.name, last_cookie.value,
            ),
        ),
        Err(ControlledCookiePolicyError::InvalidCookie),
    );
    assert_eq!(byte_limited.export_state().unwrap(), byte_baseline);

    byte_limited
        .set_controlled_session_cookie_from_non_http(
            &document_url,
            &document_url,
            &format!("budget-000={first_byte_value}; Path=/; Secure; SameSite=Lax"),
        )
        .unwrap();
    let byte_baseline = byte_limited.export_state().unwrap();
    let overflow_name = "one-encoded-record-too-many";
    let overflow_fixed_bytes = overflow_name.len() + "example.com".len() + "/".len();
    let overflow_value = "y".repeat(COOKIE_STATE_MAX_COOKIE_BYTES_V1 - overflow_fixed_bytes);
    assert_eq!(
        byte_limited.set_controlled_session_cookie_from_non_http(
            &document_url,
            &document_url,
            &format!("{overflow_name}={overflow_value}; Path=/; Secure"),
        ),
        Err(ControlledCookiePolicyError::InvalidCookie)
    );
    assert_eq!(byte_limited.export_state().unwrap(), byte_baseline);

    let mut protected = CookieStorage::new(5);
    protected
        .replace_state(
            0,
            CookieStateSnapshotV1 {
                schema_version: COOKIE_STATE_SCHEMA_VERSION_V1,
                revision: 0,
                cookies: vec![state_cookie("guarded", "example.com", "/", 0)],
            },
        )
        .unwrap();
    let protected_baseline = protected.export_state().unwrap();
    assert_eq!(
        protected.set_controlled_session_cookie_from_non_http(
            &document_url,
            &document_url,
            "guarded=not-overwritten; Path=/; Secure",
        ),
        Err(ControlledCookiePolicyError::InvalidCookie)
    );
    assert_eq!(protected.export_state().unwrap(), protected_baseline);
}

#[test]
fn production_registrable_host_cookie_limit_is_exact_and_atomic() {
    let records = |count: usize| {
        (0..count)
            .map(|index| {
                state_cookie(
                    &format!("bucket-{index:03}"),
                    &format!("shard-{}.example.com", index % 3),
                    "/",
                    index as u64,
                )
            })
            .collect::<Vec<_>>()
    };

    let mut storage = CookieStorage::new(COOKIE_STATE_MAX_COOKIES_PER_REGISTRABLE_HOST_V1);
    assert_eq!(
        storage.replace_state(
            0,
            CookieStateSnapshotV1 {
                schema_version: COOKIE_STATE_SCHEMA_VERSION_V1,
                revision: 0,
                cookies: records(COOKIE_STATE_MAX_COOKIES_PER_REGISTRABLE_HOST_V1),
            },
        ),
        Ok(1),
    );
    let baseline = storage.export_state().unwrap();
    assert_eq!(
        baseline.cookies.len(),
        COOKIE_STATE_MAX_COOKIES_PER_REGISTRABLE_HOST_V1
    );

    assert_eq!(
        storage.replace_state(
            baseline.revision,
            CookieStateSnapshotV1 {
                schema_version: COOKIE_STATE_SCHEMA_VERSION_V1,
                revision: baseline.revision,
                cookies: records(COOKIE_STATE_MAX_COOKIES_PER_REGISTRABLE_HOST_V1 + 1),
            },
        ),
        Err(CookieStateError::TooManyCookies),
    );
    assert_eq!(storage.export_state().unwrap(), baseline);
}

#[test]
fn controlled_cookie_backend_measures_the_exact_public_projection() {
    let cookies = vec![
        state_cookie("escaped", "example.com", "/quote", 9),
        CookieStateRecordV1 {
            name: "second".into(),
            value: "quoted-\"value\\tail".into(),
            domain: "sub.example.com".into(),
            path: "/two".into(),
            host_only: false,
            secure: true,
            http_only: false,
            same_site: CookieStateSameSite::None,
            expires_unix_time_ns: None,
            partitioned: false,
            creation_sequence: u64::MAX,
            last_access_sequence: u64::MAX - 1,
        },
    ];
    let backend = cookie_backend_encoded_array_bytes(&cookies);
    let public = cookie_public_encoded_array_bytes(&cookies);
    assert_eq!(backend, public + 5 * cookies.len());

    let boundary = maximum_admitted_cookie_fragment();
    assert_eq!(
        cookie_public_encoded_array_bytes(&boundary),
        COOKIE_STATE_MAX_ENCODED_PUBLIC_ARRAY_BYTES_V1,
    );
    assert!(
        cookie_backend_encoded_array_bytes(&boundary)
            > COOKIE_STATE_MAX_ENCODED_PUBLIC_ARRAY_BYTES_V1,
        "the regression requires a public-valid fragment rejected by the private JSON shape",
    );
    let mut over_boundary = boundary.clone();
    over_boundary.last_mut().unwrap().value.push('x');
    assert_eq!(
        cookie_public_encoded_array_bytes(&over_boundary),
        COOKIE_STATE_MAX_ENCODED_PUBLIC_ARRAY_BYTES_V1 + 1,
    );
    let mut rejected = CookieStorage::new(COOKIE_STATE_MAX_COOKIES_V1);
    assert_eq!(
        rejected.replace_state(
            0,
            CookieStateSnapshotV1 {
                schema_version: COOKIE_STATE_SCHEMA_VERSION_V1,
                revision: 0,
                cookies: over_boundary,
            },
        ),
        Err(CookieStateError::SnapshotTooLarge),
    );
    assert!(rejected.export_state().unwrap().cookies.is_empty());

    let mut storage = CookieStorage::new(COOKIE_STATE_MAX_COOKIES_V1);
    assert_eq!(
        storage.replace_state(
            0,
            CookieStateSnapshotV1 {
                schema_version: COOKIE_STATE_SCHEMA_VERSION_V1,
                revision: 0,
                cookies: boundary,
            },
        ),
        Ok(1),
    );
}

#[test]
fn controlled_cookie_raw_and_batch_inputs_are_bounded_before_mutation() {
    let mut storage = CookieStorage::new(COOKIE_STATE_MAX_COOKIES_V1 + 1);
    let request = ServoUrl::parse("https://example.com/").unwrap();
    let baseline = storage.export_state().unwrap();

    let huge_raw = format!(
        "huge={}",
        "x".repeat(CONTROLLED_COOKIE_MAX_RAW_VALUE_BYTES_V1)
    );
    assert!(huge_raw.len() > CONTROLLED_COOKIE_MAX_RAW_VALUE_BYTES_V1);
    assert_eq!(
        storage.set_controlled_session_cookie_from_non_http(&request, &request, &huge_raw),
        Err(ControlledCookiePolicyError::InvalidCookie)
    );
    assert_eq!(storage.export_state().unwrap(), baseline);

    let batch = vec!["same=value; Secure"; CONTROLLED_COOKIE_MAX_BATCH_VALUES_V1 + 1];
    assert_eq!(
        storage.set_controlled_session_cookies_from_headers(&request, &request, &batch),
        Err(ControlledCookiePolicyError::InvalidCookie)
    );
    assert_eq!(storage.export_state().unwrap(), baseline);
}

#[test]
fn controlled_session_rejects_insecure_same_site_none_atomically() {
    let mut storage = CookieStorage::new(5);
    let request = ServoUrl::parse("https://example.com/").unwrap();
    let initial_revision = storage.export_state().unwrap().revision;

    assert_eq!(
        storage.set_controlled_session_cookies_from_headers(
            &request,
            &request,
            &[
                "would-have-been-stored=value; Secure",
                "insecure-none=value; SameSite=None",
            ],
        ),
        Err(ControlledCookiePolicyError::InvalidCookie)
    );

    let final_state = storage.export_state().unwrap();
    assert_eq!(final_state.revision, initial_revision);
    assert!(final_state.cookies.is_empty());
}

#[test]
fn controlled_session_rejects_invalid_cookie_wire_shape_atomically() {
    let mut storage = CookieStorage::new(5);
    let request = ServoUrl::parse("https://example.com/").unwrap();
    let initial_revision = storage.export_state().unwrap().revision;

    assert_eq!(
        storage.set_controlled_session_cookies_from_headers(
            &request,
            &request,
            &[
                "would-have-been-stored=value; Secure",
                "ambiguous=bad,value; Secure",
            ],
        ),
        Err(ControlledCookiePolicyError::InvalidCookie)
    );

    let final_state = storage.export_state().unwrap();
    assert_eq!(final_state.revision, initial_revision);
    assert!(final_state.cookies.is_empty());
}

#[test]
fn secure_same_site_none_cookie_export_import_round_trips() {
    let mut source = CookieStorage::new(5);
    let request = ServoUrl::parse("https://example.com/").unwrap();
    source
        .set_controlled_session_cookie_from_header(
            &request,
            &request,
            "session=secret; Secure; SameSite=None",
        )
        .unwrap();
    let exported = source.export_state().unwrap();

    let mut target = CookieStorage::new(5);
    target.replace_state(0, exported.clone()).unwrap();
    assert_eq!(target.export_state().unwrap(), exported);
    assert_eq!(
        target
            .controlled_session_cookies_for_url(&request, &request, CookieSource::HTTP)
            .unwrap()
            .as_deref(),
        Some("session=secret")
    );
}

#[test]
fn controlled_cookie_ordering_never_uses_servo_wall_time() {
    let mut storage = CookieStorage::new(5);
    let request = ServoUrl::parse("https://example.com/account").unwrap();
    // Deliberately reverse both ordinary Servo timestamps before the controlled boundary adopts
    // these otherwise ordinary cookies. Controlled ordering must use canonical controller stamps,
    // never the wall-clock fields.
    for (value, seconds) in [("first=one; Secure", 20), ("second=two; Secure", 10)] {
        let mut cookie = ServoCookie::from_cookie_string(value, &request, CookieSource::HTTP)
            .expect("test cookie parses");
        cookie.creation_time = SystemTime::UNIX_EPOCH + Duration::from_secs(seconds);
        cookie.last_access = SystemTime::UNIX_EPOCH + Duration::from_secs(seconds);
        storage.push(cookie, &request, CookieSource::HTTP);
    }

    assert_eq!(
        storage
            .cookies_for_url(&request, CookieSource::HTTP)
            .as_deref(),
        Some("second=two; first=one")
    );
    assert_eq!(
        storage
            .controlled_session_cookies_for_url(&request, &request, CookieSource::HTTP)
            .unwrap()
            .as_deref(),
        Some("first=one; second=two")
    );
    assert_eq!(
        storage
            .controlled_session_cookies_for_url(&request, &request, CookieSource::NonHTTP)
            .unwrap()
            .as_deref(),
        Some("first=one; second=two")
    );
    let state = storage.export_state().unwrap();
    let first = state
        .cookies
        .iter()
        .find(|cookie| cookie.name == "first")
        .unwrap();
    let second = state
        .cookies
        .iter()
        .find(|cookie| cookie.name == "second")
        .unwrap();
    assert_eq!(first.creation_sequence, 0);
    assert_eq!(second.creation_sequence, 1);
    assert_eq!(first.last_access_sequence, 0);
    assert_eq!(second.last_access_sequence, 1);
}

#[test]
fn controlled_cookie_access_order_is_monotonic_and_canonical() {
    let mut storage = CookieStorage::new(5);
    let root = ServoUrl::parse("https://example.com/").unwrap();
    storage
        .set_controlled_session_cookies_from_headers(
            &root,
            &root,
            &[
                "root=value; Secure; Path=/",
                "account=value; Secure; Path=/account",
            ],
        )
        .unwrap();

    assert_eq!(
        storage
            .controlled_session_cookies_for_url(&root, &root, CookieSource::HTTP)
            .unwrap()
            .as_deref(),
        Some("root=value")
    );
    let state = storage.export_state().unwrap();
    let root_cookie = state
        .cookies
        .iter()
        .find(|cookie| cookie.name == "root")
        .unwrap();
    let account_cookie = state
        .cookies
        .iter()
        .find(|cookie| cookie.name == "account")
        .unwrap();
    assert_eq!(account_cookie.last_access_sequence, 0);
    assert_eq!(root_cookie.last_access_sequence, 1);
}

#[test]
fn controlled_cookie_sequence_exhaustion_compacts_without_wrapping() {
    let mut storage = CookieStorage::new(5);
    let request = ServoUrl::parse("https://example.com/").unwrap();
    let mut record = state_cookie("first", "example.com", "/", 0);
    record.creation_sequence = u64::MAX;
    record.last_access_sequence = u64::MAX;
    storage
        .replace_state(
            0,
            CookieStateSnapshotV1 {
                schema_version: COOKIE_STATE_SCHEMA_VERSION_V1,
                revision: 0,
                cookies: vec![record],
            },
        )
        .unwrap();

    storage
        .set_controlled_session_cookie_from_header(&request, &request, "second=value; Secure")
        .unwrap();
    assert_eq!(
        storage
            .controlled_session_cookies_for_url(&request, &request, CookieSource::HTTP)
            .unwrap()
            .as_deref(),
        Some("first=first-secret; second=value")
    );

    let state = storage.export_state().unwrap();
    let first = state
        .cookies
        .iter()
        .find(|cookie| cookie.name == "first")
        .unwrap();
    let second = state
        .cookies
        .iter()
        .find(|cookie| cookie.name == "second")
        .unwrap();
    assert_eq!(first.creation_sequence, 0);
    assert_eq!(second.creation_sequence, 1);
    assert_eq!(first.last_access_sequence, 0);
    assert_eq!(second.last_access_sequence, 1);
}

#[test]
fn controlled_cookie_v1_context_preserves_frozen_rejections() {
    let mut storage = CookieStorage::new(8);
    let request = ServoUrl::parse("https://api.example.com/account").unwrap();
    let same_site = controlled_cookie_context(
        ControlledCookiePolicy::SessionV1,
        Some("https://www.example.com/"),
        false,
    );
    let cross_site = controlled_cookie_context(
        ControlledCookiePolicy::SessionV1,
        Some("https://example.org/"),
        false,
    );
    let initial = storage.export_state().unwrap();

    assert_eq!(
        storage.controlled_session_cookies_for_url_with_context(
            &request,
            &Method::GET,
            &cross_site,
            CookieSource::HTTP,
        ),
        Err(ControlledCookiePolicyError::SameSiteContextUnsupported),
    );
    assert_eq!(
        storage.set_controlled_session_cookie_from_header_with_context(
            &request,
            &Method::GET,
            &same_site,
            "persistent=blocked; Max-Age=60; Secure",
        ),
        Err(ControlledCookiePolicyError::PersistentCookieUnsupported),
    );
    assert_eq!(
        storage.set_controlled_session_cookie_from_header_with_context(
            &request,
            &Method::GET,
            &same_site,
            "partitioned=blocked; Partitioned; Secure; SameSite=None",
        ),
        Err(ControlledCookiePolicyError::PartitionedCookieUnsupported),
    );
    assert_eq!(storage.export_state().unwrap(), initial);
}

#[test]
fn controlled_cookie_v2_retrieval_obeys_the_samesite_matrix() {
    let mut storage = CookieStorage::new(8);
    let request = ServoUrl::parse("https://api.example.com/account").unwrap();
    let policy = ControlledCookiePolicy::SessionV2 {
        unix_time_ns: 1_700_000_000_000_000_000,
    };
    let same_site = controlled_cookie_context(policy, Some("https://www.example.com/"), false);
    storage
        .set_controlled_session_cookies_from_headers_with_context(
            &request,
            &Method::GET,
            &same_site,
            &[
                "strict=s; Secure; SameSite=Strict",
                "lax=l; Secure; SameSite=Lax",
                "default=d; Secure",
                "none=n; Secure; SameSite=None",
            ],
        )
        .unwrap();

    let read =
        |storage: &mut CookieStorage, method: Method, site: &str, top_level_navigation: bool| {
            storage
                .controlled_session_cookies_for_url_with_context(
                    &request,
                    &method,
                    &controlled_cookie_context(policy, Some(site), top_level_navigation),
                    CookieSource::HTTP,
                )
                .unwrap()
        };
    assert_eq!(
        read(
            &mut storage,
            Method::GET,
            "https://shop.example.com/",
            false,
        )
        .as_deref(),
        Some("strict=s; lax=l; default=d; none=n"),
    );
    assert_eq!(
        read(&mut storage, Method::GET, "https://cross-site.test/", false,).as_deref(),
        Some("none=n"),
    );
    assert_eq!(
        read(&mut storage, Method::GET, "https://cross-site.test/", true,).as_deref(),
        Some("lax=l; default=d; none=n"),
    );
    assert_eq!(
        read(&mut storage, Method::POST, "https://cross-site.test/", true,).as_deref(),
        Some("none=n"),
    );
    assert_eq!(
        storage.controlled_session_cookies_for_url_with_context(
            &request,
            &Method::GET,
            &controlled_cookie_context(policy, None, false),
            CookieSource::HTTP,
        ),
        Err(ControlledCookiePolicyError::SameSiteContextUnsupported),
    );
}

#[test]
fn controlled_cookie_v2_storage_obeys_the_samesite_matrix() {
    let request = ServoUrl::parse("https://api.example.com/account").unwrap();
    let policy = ControlledCookiePolicy::SessionV2 {
        unix_time_ns: 1_700_000_000_000_000_000,
    };
    let cross_site_subresource =
        controlled_cookie_context(policy, Some("https://cross-site.test/"), false);
    let mut subresource_storage = CookieStorage::new(8);
    subresource_storage
        .set_controlled_session_cookies_from_headers_with_context(
            &request,
            &Method::GET,
            &cross_site_subresource,
            &[
                "strict=ignored; Secure; SameSite=Strict",
                "lax=ignored; Secure; SameSite=Lax",
                "default=ignored; Secure",
                "none=stored; Secure; SameSite=None",
            ],
        )
        .unwrap();
    let stored = subresource_storage
        .export_state_with_policy(policy)
        .unwrap();
    assert_eq!(stored.cookies.len(), 1);
    assert_eq!(stored.cookies[0].name, "none");

    let cross_site_navigation =
        controlled_cookie_context(policy, Some("https://cross-site.test/"), true);
    let mut navigation_storage = CookieStorage::new(8);
    navigation_storage
        .set_controlled_session_cookies_from_headers_with_context(
            &request,
            &Method::POST,
            &cross_site_navigation,
            &[
                "strict=stored; Secure; SameSite=Strict",
                "lax=stored; Secure; SameSite=Lax",
                "default=stored; Secure",
                "none=stored; Secure; SameSite=None",
            ],
        )
        .unwrap();
    assert_eq!(
        navigation_storage
            .export_state_with_policy(policy)
            .unwrap()
            .cookies
            .len(),
        4,
    );

    let baseline = navigation_storage.export_state_with_policy(policy).unwrap();
    assert_eq!(
        navigation_storage.set_controlled_session_cookie_from_header_with_context(
            &request,
            &Method::GET,
            &controlled_cookie_context(policy, None, false),
            "missing=site; Secure",
        ),
        Err(ControlledCookiePolicyError::SameSiteContextUnsupported),
    );
    assert_eq!(
        navigation_storage.export_state_with_policy(policy).unwrap(),
        baseline,
    );
}

#[test]
fn controlled_cookie_v2_persistent_precedence_clamp_and_deletion() {
    const NOW: u128 = 1_700_000_000_000_000_000;
    const SECOND_NS: u64 = 1_000_000_000;
    const MAX_AGE_SECONDS: u64 = 34_560_000;

    let request = ServoUrl::parse("https://example.com/account").unwrap();
    let policy = ControlledCookiePolicy::SessionV2 { unix_time_ns: NOW };
    let context = controlled_cookie_context(policy, Some("https://example.com/"), false);
    let mut storage = CookieStorage::new(8);
    storage
        .set_controlled_session_cookies_from_headers_with_context(
            &request,
            &Method::GET,
            &context,
            &[
                "precedence=live; Max-Age=60; Expires=Thu, 01 Jan 1970 00:00:01 GMT; Secure",
                "last-valid=live; Max-Age=10; Max-Age=invalid; Max-Age=20; Secure",
                "clamped=live; Max-Age=40000000; Secure",
                "far-future=live; Expires=Fri, 31 Dec 9999 23:59:59 GMT; Secure",
            ],
        )
        .unwrap();
    let state = storage.export_state_with_policy(policy).unwrap();
    let expires = |name: &str| {
        state
            .cookies
            .iter()
            .find(|cookie| cookie.name == name)
            .unwrap()
            .expires_unix_time_ns
            .unwrap()
    };
    assert_eq!(expires("precedence"), NOW as u64 + 60 * SECOND_NS);
    assert_eq!(expires("last-valid"), NOW as u64 + 20 * SECOND_NS);
    assert_eq!(expires("clamped"), NOW as u64 + MAX_AGE_SECONDS * SECOND_NS,);
    assert_eq!(
        expires("far-future"),
        NOW as u64 + MAX_AGE_SECONDS * SECOND_NS,
    );

    let later_policy = ControlledCookiePolicy::SessionV2 {
        unix_time_ns: NOW + u128::from(SECOND_NS),
    };
    storage
        .set_controlled_session_cookie_from_header_with_context(
            &request,
            &Method::GET,
            &controlled_cookie_context(later_policy, Some("https://example.com/"), false),
            "precedence=deleted; Max-Age=0; Secure",
        )
        .unwrap();
    let deleted = storage.export_state_with_policy(later_policy).unwrap();
    assert_eq!(deleted.revision, state.revision + 1);
    assert_eq!(
        deleted
            .cookies
            .iter()
            .map(|cookie| cookie.name.as_str())
            .collect::<Vec<_>>(),
        vec!["clamped", "far-future", "last-valid"],
    );

    storage
        .set_controlled_session_cookie_from_header_with_context(
            &request,
            &Method::GET,
            &controlled_cookie_context(later_policy, Some("https://example.com/"), false),
            "absent=delete-noop; Max-Age=0; Secure",
        )
        .unwrap();
    assert_eq!(
        storage.export_state_with_policy(later_policy).unwrap(),
        deleted,
    );
}

#[test]
fn controlled_cookie_v2_lazy_purge_is_revisioned_once_for_reads_and_exports() {
    const NOW: u128 = 1_700_000_000_000_000_000;
    const SECOND_NS: u128 = 1_000_000_000;
    let request = ServoUrl::parse("https://example.com/").unwrap();
    let initial_policy = ControlledCookiePolicy::SessionV2 { unix_time_ns: NOW };
    let initial_context =
        controlled_cookie_context(initial_policy, Some("https://example.com/"), false);
    let expired_policy = ControlledCookiePolicy::SessionV2 {
        unix_time_ns: NOW + SECOND_NS,
    };
    let expired_context =
        controlled_cookie_context(expired_policy, Some("https://example.com/"), false);

    let mut read_purge = CookieStorage::new(8);
    read_purge
        .set_controlled_session_cookie_from_header_with_context(
            &request,
            &Method::GET,
            &initial_context,
            "short=live; Max-Age=1; Secure",
        )
        .unwrap();
    let before_read = read_purge.export_state_with_policy(initial_policy).unwrap();
    assert_eq!(
        read_purge
            .controlled_session_cookies_for_url_with_context(
                &request,
                &Method::GET,
                &expired_context,
                CookieSource::HTTP,
            )
            .unwrap(),
        None,
    );
    let after_read = read_purge.export_state_with_policy(expired_policy).unwrap();
    assert!(after_read.cookies.is_empty());
    assert_eq!(after_read.revision, before_read.revision + 1);
    assert_eq!(
        read_purge.export_state_with_policy(expired_policy).unwrap(),
        after_read,
    );

    let mut export_purge = CookieStorage::new(8);
    export_purge
        .set_controlled_session_cookie_from_header_with_context(
            &request,
            &Method::GET,
            &initial_context,
            "short=live; Max-Age=1; Secure",
        )
        .unwrap();
    let before_export = export_purge
        .export_state_with_policy(initial_policy)
        .unwrap();
    let after_export = export_purge
        .export_state_with_policy(expired_policy)
        .unwrap();
    assert!(after_export.cookies.is_empty());
    assert_eq!(after_export.revision, before_export.revision + 1);
    assert_eq!(
        export_purge
            .export_state_with_policy(expired_policy)
            .unwrap(),
        after_export,
    );
}

#[test]
fn controlled_cookie_v2_persistent_state_round_trips_and_batches_are_atomic() {
    const NOW: u128 = 1_700_000_000_000_000_000;
    const SECOND_NS: u128 = 1_000_000_000;
    let request = ServoUrl::parse("https://example.com/").unwrap();
    let policy = ControlledCookiePolicy::SessionV2 { unix_time_ns: NOW };
    let context = controlled_cookie_context(policy, Some("https://example.com/"), false);

    let mut source = CookieStorage::new(8);
    source
        .set_controlled_session_cookie_from_header_with_context(
            &request,
            &Method::GET,
            &context,
            "persistent=value; Max-Age=60; Secure; SameSite=Lax",
        )
        .unwrap();
    let snapshot = source.export_state_with_policy(policy).unwrap();

    let mut restored = CookieStorage::new(8);
    assert_eq!(
        restored.replace_state_with_policy(policy, 0, snapshot.clone()),
        Ok(1),
    );
    assert_eq!(
        restored
            .controlled_session_cookies_for_url_with_context(
                &request,
                &Method::GET,
                &context,
                CookieSource::HTTP,
            )
            .unwrap()
            .as_deref(),
        Some("persistent=value"),
    );
    assert_eq!(
        restored.export_state_with_policy(policy).unwrap().cookies[0].expires_unix_time_ns,
        snapshot.cookies[0].expires_unix_time_ns,
    );

    let mut frozen_v1 = CookieStorage::new(8);
    assert_eq!(
        frozen_v1.replace_state(0, snapshot.clone()),
        Err(CookieStateError::PersistentCookieUnsupported),
    );
    assert!(frozen_v1.export_state().unwrap().cookies.is_empty());

    let expired_policy = ControlledCookiePolicy::SessionV2 {
        unix_time_ns: NOW + 61 * SECOND_NS,
    };
    let mut expired_import = CookieStorage::new(8);
    assert_eq!(
        expired_import.replace_state_with_policy(expired_policy, 0, snapshot),
        Ok(1),
    );
    assert!(
        expired_import
            .export_state_with_policy(expired_policy)
            .unwrap()
            .cookies
            .is_empty(),
    );

    let baseline = restored.export_state_with_policy(policy).unwrap();
    assert_eq!(
        restored.set_controlled_session_cookies_from_headers_with_context(
            &request,
            &Method::GET,
            &context,
            &[
                "accepted=only-if-whole-batch-is-valid; Max-Age=120; Secure",
                "invalid=none-without-secure; SameSite=None",
            ],
        ),
        Err(ControlledCookiePolicyError::InvalidCookie),
    );
    assert_eq!(restored.export_state_with_policy(policy).unwrap(), baseline);
}

#[test]
fn controlled_cookie_v2_time_bounds_fail_before_mutation() {
    let request = ServoUrl::parse("https://example.com/").unwrap();
    let baseline_policy = ControlledCookiePolicy::SessionV2 { unix_time_ns: 0 };
    let baseline_context =
        controlled_cookie_context(baseline_policy, Some("https://example.com/"), false);
    let mut storage = CookieStorage::new(8);
    storage
        .set_controlled_session_cookie_from_header_with_context(
            &request,
            &Method::GET,
            &baseline_context,
            "lower-bound=live; Max-Age=1; Secure",
        )
        .unwrap();
    let baseline = storage.export_state_with_policy(baseline_policy).unwrap();
    assert_eq!(
        baseline.cookies[0].expires_unix_time_ns,
        Some(1_000_000_000)
    );

    let above_u64 = ControlledCookiePolicy::SessionV2 {
        unix_time_ns: u128::from(u64::MAX) + 1,
    };
    let above_u64_context =
        controlled_cookie_context(above_u64, Some("https://example.com/"), false);
    assert_eq!(
        storage.set_controlled_session_cookie_from_header_with_context(
            &request,
            &Method::GET,
            &above_u64_context,
            "never=stored; Max-Age=1; Secure",
        ),
        Err(ControlledCookiePolicyError::TimeRangeUnsupported),
    );
    assert_eq!(
        storage.controlled_session_cookies_for_url_with_context(
            &request,
            &Method::GET,
            &above_u64_context,
            CookieSource::HTTP,
        ),
        Err(ControlledCookiePolicyError::TimeRangeUnsupported),
    );
    assert_eq!(
        storage.export_state_with_policy(above_u64),
        Err(CookieStateError::TimeRangeUnsupported),
    );
    assert_eq!(
        storage.export_state_with_policy(baseline_policy).unwrap(),
        baseline
    );

    let no_persistence_headroom = ControlledCookiePolicy::SessionV2 {
        unix_time_ns: u128::from(u64::MAX),
    };
    let no_persistence_headroom_context =
        controlled_cookie_context(no_persistence_headroom, Some("https://example.com/"), false);
    let mut edge_storage = CookieStorage::new(8);
    edge_storage
        .set_controlled_session_cookie_from_header_with_context(
            &request,
            &Method::GET,
            &no_persistence_headroom_context,
            "session=stored; Secure",
        )
        .unwrap();
    let edge_state = edge_storage
        .export_state_with_policy(no_persistence_headroom)
        .unwrap();
    assert_eq!(edge_state.cookies.len(), 1);
    assert_eq!(edge_state.cookies[0].name, "session");
    assert_eq!(edge_state.cookies[0].expires_unix_time_ns, None);
    edge_storage
        .set_controlled_session_cookie_from_header_with_context(
            &request,
            &Method::GET,
            &no_persistence_headroom_context,
            "session=deleted; Expires=Thu, 01 Jan 1970 00:00:01 GMT; Secure",
        )
        .unwrap();
    assert!(
        edge_storage
            .export_state_with_policy(no_persistence_headroom)
            .unwrap()
            .cookies
            .is_empty(),
    );

    let mut expired_import_at_boundary = CookieStorage::new(8);
    let mut expired_record = state_cookie("expired-import", "example.com", "/", 0);
    expired_record.expires_unix_time_ns = Some(u64::MAX);
    assert_eq!(
        expired_import_at_boundary.replace_state_with_policy(
            no_persistence_headroom,
            0,
            CookieStateSnapshotV1 {
                schema_version: COOKIE_STATE_SCHEMA_VERSION_V1,
                revision: 0,
                cookies: vec![expired_record],
            },
        ),
        Ok(1),
    );
    let expired_import_state = expired_import_at_boundary
        .export_state_with_policy(no_persistence_headroom)
        .unwrap();
    assert_eq!(expired_import_state.revision, 1);
    assert!(expired_import_state.cookies.is_empty());

    let mut rejected_import = CookieStorage::new(8);
    assert_eq!(
        rejected_import.replace_state_with_policy(
            above_u64,
            0,
            CookieStateSnapshotV1 {
                schema_version: COOKIE_STATE_SCHEMA_VERSION_V1,
                revision: 0,
                cookies: vec![state_cookie("import", "example.com", "/", 0)],
            },
        ),
        Err(CookieStateError::TimeRangeUnsupported),
    );
    assert!(rejected_import.export_state().unwrap().cookies.is_empty());
}
