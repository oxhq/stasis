/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Implementation of cookie storage as specified in
//! <http://tools.ietf.org/html/rfc6265>

use std::cmp::Ordering;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::net::IpAddr;
use std::time::{Duration, SystemTime};

use cookie::{Cookie, SameSite};
use embedder_traits::{ControlledCookieContext, ControlledCookiePolicy};
use http::Method;
use itertools::Itertools;
use malloc_size_of_derive::MallocSizeOf;
use net_traits::pub_domains::{is_same_site, reg_suffix};
use net_traits::{
    CONTROLLED_COOKIE_MAX_BATCH_VALUES_V1, CONTROLLED_COOKIE_MAX_RAW_VALUE_BYTES_V1,
    COOKIE_STATE_MAX_COOKIE_BYTES_V1, COOKIE_STATE_MAX_COOKIES_V1,
    COOKIE_STATE_MAX_ENCODED_PUBLIC_ARRAY_BYTES_V1, COOKIE_STATE_MAX_TOTAL_BYTES_V1,
    COOKIE_STATE_SCHEMA_VERSION_V1, ControlledCookiePolicyError, CookieSource, CookieStateError,
    CookieStateRecordV1, CookieStateSameSite, CookieStateSnapshotV1, SiteDescriptor,
    has_valid_cookie_state_prefix, is_canonical_cookie_state_domain,
    is_valid_cookie_state_name_and_value,
};
use serde::ser::SerializeSeq;
use serde::{Deserialize, Serialize};
use servo_url::ServoUrl;

use crate::cookie::ServoCookie;

#[derive(Clone, Debug, Deserialize, Serialize, MallocSizeOf)]
pub struct CookieStorage {
    version: u32,
    cookies_map: HashMap<String, Vec<ServoCookie>>,
    max_per_host: usize,
    #[serde(default)]
    revision: u64,
    #[serde(default)]
    revision_exhausted: bool,
    /// Next controller-owned ordering stamps. These are separate from the ordinary Servo cookie
    /// wall-clock fields and are used only by the controlled-session paths below.
    #[serde(default)]
    controlled_creation_sequence_next: u64,
    #[serde(default)]
    controlled_creation_sequence_exhausted: bool,
    #[serde(default)]
    controlled_access_sequence_next: u64,
    #[serde(default)]
    controlled_access_sequence_exhausted: bool,
}

#[derive(Debug)]
pub enum RemoveCookieError {
    Overlapping,
    NonHTTP,
}

impl CookieStorage {
    pub fn new(max_cookies: usize) -> CookieStorage {
        CookieStorage {
            version: 1,
            cookies_map: HashMap::new(),
            max_per_host: max_cookies,
            revision: 0,
            revision_exhausted: false,
            controlled_creation_sequence_next: 0,
            controlled_creation_sequence_exhausted: false,
            controlled_access_sequence_next: 0,
            controlled_access_sequence_exhausted: false,
        }
    }

    fn bump_revision(&mut self) {
        match self.revision.checked_add(1) {
            Some(revision) => self.revision = revision,
            None => self.revision_exhausted = true,
        }
    }

    // http://tools.ietf.org/html/rfc6265#section-5.3
    pub fn remove(
        &mut self,
        cookie: &ServoCookie,
        url: &ServoUrl,
        source: CookieSource,
    ) -> Result<Option<ServoCookie>, RemoveCookieError> {
        let domain = reg_host(cookie.cookie.domain().as_ref().unwrap_or(&""));
        let cookies = self.cookies_map.entry(domain).or_default();

        // https://www.ietf.org/id/draft-ietf-httpbis-cookie-alone-01.txt Step 2
        if !cookie.cookie.secure().unwrap_or(false) && !url.is_secure_scheme() {
            let new_domain = cookie.cookie.domain().as_ref().unwrap().to_owned();
            let new_path = cookie.cookie.path().as_ref().unwrap().to_owned();

            let any_overlapping = cookies.iter().any(|c| {
                let existing_domain = c.cookie.domain().as_ref().unwrap().to_owned();
                let existing_path = c.cookie.path().as_ref().unwrap().to_owned();

                c.cookie.name() == cookie.cookie.name()
                    && c.cookie.secure().unwrap_or(false)
                    && (ServoCookie::domain_match(new_domain, existing_domain)
                        || ServoCookie::domain_match(existing_domain, new_domain))
                    && ServoCookie::path_match(new_path, existing_path)
            });

            if any_overlapping {
                return Err(RemoveCookieError::Overlapping);
            }
        }

        // Step 11.1
        let position = cookies.iter().position(|c| {
            c.cookie.domain() == cookie.cookie.domain()
                && c.cookie.path() == cookie.cookie.path()
                && c.cookie.name() == cookie.cookie.name()
        });

        if let Some(ind) = position {
            // Step 11.4
            let c = cookies.remove(ind);

            // http://tools.ietf.org/html/rfc6265#section-5.3 step 11.2
            if c.cookie.http_only().unwrap_or(false) && source == CookieSource::NonHTTP {
                // Undo the removal.
                cookies.push(c);
                Err(RemoveCookieError::NonHTTP)
            } else {
                self.bump_revision();
                Ok(Some(c))
            }
        } else {
            Ok(None)
        }
    }

    pub fn delete_cookies_for_sites(&mut self, sites: &Vec<String>) {
        // Note: We assume the number of sites is smaller than the number of
        // entries in the cookies map. If this assumption stops holding in
        // practice, this implementation can be revised to use `retain`
        // together with a temporary `HashSet` of sites.
        let mut changed = false;
        for site in sites {
            // TODO: We currently mark cookies as expired instead of removing
            // them immediately (same behavior as in the functions below).
            // This is safe because higher-level cookie accessors always call
            // `remove_expired_cookies_for_url` / `remove_all_expired_cookies`.
            // Consider whether we should instead delete the entries directly.
            if let Some(cookies) = self.cookies_map.get_mut(site) {
                for cookie in cookies.iter_mut() {
                    cookie.set_expiry_time_in_past();
                    changed = true;
                }
            }
        }
        if changed {
            self.bump_revision();
        }
    }

    pub fn clear_session_cookies(&mut self) {
        let mut changed = false;
        for cookie in self
            .cookies_map
            .values_mut()
            .flat_map(|cookies| cookies.iter_mut())
            .filter(|cookie| !cookie.persistent)
        {
            cookie.set_expiry_time_in_past();
            changed = true;
        }
        if changed {
            self.bump_revision();
        }
    }

    pub fn clear_storage(&mut self, url: Option<&ServoUrl>) {
        let mut changed = false;
        if let Some(url) = url {
            let domain = reg_host(url.host_str().unwrap_or(""));
            if let Some(cookies) = self.cookies_map.get_mut(&domain) {
                for cookie in cookies.iter_mut() {
                    cookie.set_expiry_time_in_past();
                    changed = true;
                }
            }
        } else {
            changed = !self.cookies_map.is_empty();
            self.cookies_map.clear();
        }
        if changed {
            self.bump_revision();
        }
    }

    pub fn delete_cookie_with_name(&mut self, url: &ServoUrl, name: String) {
        let mut changed = false;
        let domain = reg_host(url.host_str().unwrap_or(""));
        if let Some(cookies) = self.cookies_map.get_mut(&domain) {
            for cookie in cookies.iter_mut() {
                if cookie.cookie.name() == name {
                    cookie.set_expiry_time_in_past();
                    changed = true;
                }
            }
        }
        if changed {
            self.bump_revision();
        }
    }

    // http://tools.ietf.org/html/rfc6265#section-5.3
    pub fn push(&mut self, mut cookie: ServoCookie, url: &ServoUrl, source: CookieSource) {
        // https://www.ietf.org/id/draft-ietf-httpbis-cookie-alone-01.txt Step 1
        if cookie.cookie.secure().unwrap_or(false) && !url.is_secure_scheme() {
            return;
        }

        let old_cookie = self.remove(&cookie, url, source);
        if old_cookie.is_err() {
            // This new cookie is not allowed to overwrite an existing one.
            return;
        }

        // Step 11
        if let Some(old_cookie) = old_cookie.unwrap() {
            // Step 11.3
            cookie.creation_time = old_cookie.creation_time;
            // A replacement retains the controller-owned creation order just as it retains the
            // ordinary RFC creation time. Ordinary cookies carry `None`, so their behavior is
            // unchanged.
            cookie.controlled_creation_sequence = old_cookie
                .controlled_creation_sequence
                .or(cookie.controlled_creation_sequence);
        }

        // Step 12
        let domain = reg_host(cookie.cookie.domain().as_ref().unwrap_or(&""));
        let cookies = self.cookies_map.entry(domain).or_default();

        if cookies.len() == self.max_per_host {
            let old_len = cookies.len();
            cookies.retain(|c| !is_cookie_expired(c));
            let new_len = cookies.len();

            // https://www.ietf.org/id/draft-ietf-httpbis-cookie-alone-01.txt
            if new_len == old_len
                && !evict_one_cookie(cookie.cookie.secure().unwrap_or(false), cookies)
            {
                return;
            }
        }
        cookies.push(cookie);
        self.bump_revision();
    }

    pub fn cookie_comparator(a: &ServoCookie, b: &ServoCookie) -> Ordering {
        let a_path_len = a.cookie.path().as_ref().map_or(0, |p| p.len());
        let b_path_len = b.cookie.path().as_ref().map_or(0, |p| p.len());
        match a_path_len.cmp(&b_path_len) {
            Ordering::Equal => a.creation_time.cmp(&b.creation_time),
            // Ensure that longer paths are sorted earlier than shorter paths
            Ordering::Greater => Ordering::Less,
            Ordering::Less => Ordering::Greater,
        }
    }

    pub fn remove_expired_cookies_for_url(&mut self, url: &ServoUrl) {
        let domain = reg_host(url.host_str().unwrap_or(""));
        let mut changed = false;
        if let Entry::Occupied(mut entry) = self.cookies_map.entry(domain) {
            let cookies = entry.get_mut();
            let old_len = cookies.len();
            cookies.retain(|c| !is_cookie_expired(c));
            changed = cookies.len() != old_len;
            if cookies.is_empty() {
                entry.remove_entry();
            }
        }
        if changed {
            self.bump_revision();
        }
    }

    pub fn remove_all_expired_cookies(&mut self) {
        let old_len: usize = self.cookies_map.values().map(Vec::len).sum();
        self.cookies_map.retain(|_, cookies| {
            cookies.retain(|c| !is_cookie_expired(c));
            !cookies.is_empty()
        });
        let new_len: usize = self.cookies_map.values().map(Vec::len).sum();
        if new_len != old_len {
            self.bump_revision();
        }
    }

    // http://tools.ietf.org/html/rfc6265#section-5.4
    pub fn cookies_for_url(&mut self, url: &ServoUrl, source: CookieSource) -> Option<String> {
        // Let cookie-list be the set of cookies from the cookie store
        let cookie_list = self.cookies_data_for_url(url, source);

        let reducer = |acc: String, cookie: Cookie<'static>| -> String {
            // Serialize the cookie-list into a cookie-string by processing each cookie in the cookie-list in order:
            // If the cookies' name is not empty, output the cookie's name followed by the %x3D ("=") character.
            // If the cookies' value is not empty, output the cookie's value.
            // If there is an unprocessed cookie in the cookie-list, output the characters %x3B and %x20 ("; ").
            // Security: the above steps allow for "nameless" cookies which have proved to be a security footgun
            // especially with the new cookie name prefix proposals
            (match acc.len() {
                0 => acc,
                _ => acc + "; ",
            }) + cookie.name()
                + "="
                + cookie.value()
        };

        // Serialize the cookie-list into a cookie-string by processing each cookie in the cookie-list in order
        let result = cookie_list.fold("".to_owned(), reducer);

        match result.len() {
            0 => None,
            _ => Some(result),
        }
    }

    /// Return the HTTP Cookie header for a deterministic controlled session.
    ///
    /// The current Servo jar does not carry a request-site argument into ordinary cookie
    /// selection. Controlled sessions therefore admit only a schemefully same-site context. Once
    /// that proof holds, Strict, Lax, unspecified, and None cookies all pass the SameSite boundary;
    /// the existing domain/path/Secure/HttpOnly checks remain authoritative.
    pub fn controlled_session_cookies_for_url(
        &mut self,
        url: &ServoUrl,
        top_level_url: &ServoUrl,
        source: CookieSource,
    ) -> Result<Option<String>, ControlledCookiePolicyError> {
        let context = ControlledCookieContext {
            policy: ControlledCookiePolicy::SessionV1,
            site_for_cookies: Some(top_level_url.as_url().clone()),
            top_level_navigation: false,
        };
        self.controlled_session_cookies_for_url_with_context(url, &Method::GET, &context, source)
    }

    /// Return a controlled Cookie header using captured request provenance and explicit time.
    pub fn controlled_session_cookies_for_url_with_context(
        &mut self,
        url: &ServoUrl,
        method: &Method,
        context: &ControlledCookieContext,
        source: CookieSource,
    ) -> Result<Option<String>, ControlledCookiePolicyError> {
        let same_site = controlled_same_site(url, context)?;
        match context.policy {
            ControlledCookiePolicy::SessionV1 if !same_site => {
                return Err(ControlledCookiePolicyError::SameSiteContextUnsupported);
            },
            ControlledCookiePolicy::SessionV1 => {},
            ControlledCookiePolicy::SessionV2 { unix_time_ns } => {
                self.remove_controlled_expired_cookies_for_url(url, unix_time_ns)?;
            },
        }
        self.ensure_controlled_ordering()
            .map_err(|()| ControlledCookiePolicyError::InvalidCookie)?;

        let domain = reg_host(url.host_str().unwrap_or(""));
        let matching_count = self
            .cookies_map
            .entry(domain.clone())
            .or_default()
            .iter()
            .filter(|cookie| {
                cookie.appropriate_for_url(url, source)
                    && controlled_same_site_allows_cookie(cookie, same_site, method, context)
            })
            .count();
        let access_sequences = self
            .reserve_controlled_access_sequences(matching_count)
            .map_err(|()| ControlledCookiePolicyError::InvalidCookie)?;
        let cookies = self.cookies_map.entry(domain).or_default();
        let mut matching: Vec<_> = cookies
            .iter_mut()
            .filter(|cookie| {
                cookie.appropriate_for_url(url, source)
                    && controlled_same_site_allows_cookie(cookie, same_site, method, context)
            })
            .collect();
        matching.sort_by(|a, b| controlled_cookie_comparator(a, b));

        let mut result = String::new();
        for (cookie, access_sequence) in matching.into_iter().zip(access_sequences) {
            cookie.controlled_last_access_sequence = Some(access_sequence);
            if !result.is_empty() {
                result.push_str("; ");
            }
            result.push_str(cookie.cookie.name());
            result.push('=');
            result.push_str(cookie.cookie.value());
        }
        if result.is_empty() {
            Ok(None)
        } else {
            self.bump_revision();
            Ok(Some(result))
        }
    }

    /// Parse and store one controlled-session Set-Cookie value without wall-clock or partition
    /// ambiguity. Rejection happens before `push`, so the jar and its revision remain unchanged.
    pub fn set_controlled_session_cookie_from_header(
        &mut self,
        request_url: &ServoUrl,
        top_level_url: &ServoUrl,
        cookie_value: &str,
    ) -> Result<(), ControlledCookiePolicyError> {
        self.set_controlled_session_cookies_from_headers(
            request_url,
            top_level_url,
            &[cookie_value],
        )
    }

    pub fn set_controlled_session_cookie_from_header_with_context(
        &mut self,
        request_url: &ServoUrl,
        method: &Method,
        context: &ControlledCookieContext,
        cookie_value: &str,
    ) -> Result<(), ControlledCookiePolicyError> {
        self.set_controlled_session_cookies_from_headers_with_context(
            request_url,
            method,
            context,
            &[cookie_value],
        )
    }

    /// Parse and store one cookie authored by a page API under controlled-session policy. This
    /// deliberately accepts the original cookie string so attribute *presence* (including an
    /// invalid `Max-Age`) is rejected before parsing can erase that evidence.
    pub fn set_controlled_session_cookie_from_non_http(
        &mut self,
        request_url: &ServoUrl,
        top_level_url: &ServoUrl,
        cookie_value: &str,
    ) -> Result<(), ControlledCookiePolicyError> {
        self.set_controlled_session_cookie_values(
            request_url,
            top_level_url,
            &[cookie_value],
            CookieSource::NonHTTP,
        )
    }

    pub fn set_controlled_session_cookie_from_non_http_with_context(
        &mut self,
        request_url: &ServoUrl,
        context: &ControlledCookieContext,
        cookie_value: &str,
    ) -> Result<(), ControlledCookiePolicyError> {
        self.set_controlled_session_cookie_values_with_context(
            request_url,
            &Method::GET,
            context,
            &[cookie_value],
            CookieSource::NonHTTP,
        )
    }

    /// Validate a complete response's Set-Cookie list before mutating the jar. This preserves the
    /// response-level fail-closed boundary when a later header contains an unsupported attribute.
    pub fn set_controlled_session_cookies_from_headers(
        &mut self,
        request_url: &ServoUrl,
        top_level_url: &ServoUrl,
        cookie_values: &[&str],
    ) -> Result<(), ControlledCookiePolicyError> {
        self.set_controlled_session_cookie_values(
            request_url,
            top_level_url,
            cookie_values,
            CookieSource::HTTP,
        )
    }

    pub fn set_controlled_session_cookies_from_headers_with_context(
        &mut self,
        request_url: &ServoUrl,
        method: &Method,
        context: &ControlledCookieContext,
        cookie_values: &[&str],
    ) -> Result<(), ControlledCookiePolicyError> {
        self.set_controlled_session_cookie_values_with_context(
            request_url,
            method,
            context,
            cookie_values,
            CookieSource::HTTP,
        )
    }

    fn set_controlled_session_cookie_values(
        &mut self,
        request_url: &ServoUrl,
        top_level_url: &ServoUrl,
        cookie_values: &[&str],
        source: CookieSource,
    ) -> Result<(), ControlledCookiePolicyError> {
        let context = ControlledCookieContext {
            policy: ControlledCookiePolicy::SessionV1,
            site_for_cookies: Some(top_level_url.as_url().clone()),
            top_level_navigation: false,
        };
        self.set_controlled_session_cookie_values_with_context(
            request_url,
            &Method::GET,
            &context,
            cookie_values,
            source,
        )
    }

    fn set_controlled_session_cookie_values_with_context(
        &mut self,
        request_url: &ServoUrl,
        method: &Method,
        context: &ControlledCookieContext,
        cookie_values: &[&str],
        source: CookieSource,
    ) -> Result<(), ControlledCookiePolicyError> {
        let _ = method;
        let same_site = controlled_same_site(request_url, context)?;
        if matches!(context.policy, ControlledCookiePolicy::SessionV1) && !same_site {
            return Err(ControlledCookiePolicyError::SameSiteContextUnsupported);
        }
        if cookie_values.len() > CONTROLLED_COOKIE_MAX_BATCH_VALUES_V1 {
            return Err(ControlledCookiePolicyError::InvalidCookie);
        }
        let mut cookies = Vec::with_capacity(cookie_values.len());
        for cookie_value in cookie_values {
            if cookie_value.len() > CONTROLLED_COOKIE_MAX_RAW_VALUE_BYTES_V1 {
                return Err(ControlledCookiePolicyError::InvalidCookie);
            }
            if matches!(context.policy, ControlledCookiePolicy::SessionV1)
                && (has_cookie_attribute(cookie_value, "expires")
                    || has_cookie_attribute(cookie_value, "max-age"))
            {
                return Err(ControlledCookiePolicyError::PersistentCookieUnsupported);
            }
            if has_cookie_attribute(cookie_value, "partitioned") {
                return Err(ControlledCookiePolicyError::PartitionedCookieUnsupported);
            }
            let cookie = match context.policy {
                ControlledCookiePolicy::SessionV1 => {
                    ServoCookie::from_cookie_string(cookie_value, request_url, source)
                },
                ControlledCookiePolicy::SessionV2 { unix_time_ns } => {
                    ServoCookie::from_controlled_cookie_string(
                        cookie_value,
                        request_url,
                        source,
                        unix_time_ns,
                    )?
                },
            }
            .ok_or(ControlledCookiePolicyError::InvalidCookie)?;
            if !is_valid_cookie_state_name_and_value(cookie.cookie.name(), cookie.cookie.value())
                || (cookie.cookie.same_site() == Some(SameSite::None)
                    && cookie.cookie.secure() != Some(true))
            {
                return Err(ControlledCookiePolicyError::InvalidCookie);
            }
            let cross_site_subresource_ignored =
                matches!(context.policy, ControlledCookiePolicy::SessionV2 { .. })
                    && !same_site
                    && !context.top_level_navigation
                    && cookie.cookie.same_site() != Some(SameSite::None);
            if !cross_site_subresource_ignored {
                cookies.push(cookie);
            }
        }
        // Apply the whole response/page mutation to a private jar. Ordinary `push` intentionally
        // reports no result and can ignore a cookie (for example, a non-HTTP overwrite of an
        // HttpOnly cookie). A controlled caller must never observe that as success or consume
        // controller ordering stamps. Requiring a revision change per candidate detects those
        // silent no-ops, while the staged export proves the final global count and byte bounds.
        let mut staged = self.clone();
        let initial_revision = staged.revision;
        if let ControlledCookiePolicy::SessionV2 { unix_time_ns } = context.policy {
            staged.remove_controlled_expired_cookies_for_url(request_url, unix_time_ns)?;
        }
        for cookie in cookies {
            let previous_revision = staged.revision;
            let allow_noop_deletion = matches!(context.policy, ControlledCookiePolicy::SessionV2 { unix_time_ns }
                if cookie.controlled_expiry_time_ns.is_some_and(|expiry| expiry <= u64::try_from(unix_time_ns).unwrap_or(u64::MAX)));
            match context.policy {
                ControlledCookiePolicy::SessionV1 => {
                    staged.push_controlled(cookie, request_url, source)?;
                },
                ControlledCookiePolicy::SessionV2 { unix_time_ns } => {
                    staged.push_controlled_v2(cookie, request_url, source, unix_time_ns)?;
                },
            }
            if staged.revision == previous_revision && !allow_noop_deletion {
                return Err(ControlledCookiePolicyError::InvalidCookie);
            }
        }
        if staged.revision == initial_revision {
            return Ok(());
        }
        staged
            .export_state_with_policy(context.policy)
            .map_err(state_error_to_controlled_policy)?;
        *self = staged;
        Ok(())
    }

    /// <https://cookiestore.spec.whatwg.org/#query-cookies>
    pub fn query_cookies(&mut self, url: &ServoUrl, name: Option<String>) -> Vec<Cookie<'static>> {
        // 1. Retrieve cookie-list given request-uri and "non-HTTP" source
        let cookie_list = self.cookies_data_for_url(url, CookieSource::NonHTTP);

        // 3. For each cookie in cookie-list, run these steps:
        // 3.2. If name is given, then run these steps:
        if let Some(name) = name {
            // Let cookieName be the result of running UTF-8 decode without BOM on cookie’s name.
            // If cookieName does not equal name, then continue.
            cookie_list.filter(|cookie| cookie.name() == name).collect()
        } else {
            cookie_list.collect()
        }

        // Note: we do not convert the list into CookieListItem's here, we do that in script to not not have to define
        // the binding types in net.

        // Return list
    }

    pub fn cookies_data_for_url(
        &mut self,
        url: &ServoUrl,
        source: CookieSource,
    ) -> impl Iterator<Item = cookie::Cookie<'static>> {
        let domain = reg_host(url.host_str().unwrap_or(""));
        let cookies = self.cookies_map.entry(domain).or_default();
        let result: Vec<_> = cookies
            .iter_mut()
            .filter(move |c| c.appropriate_for_url(url, source))
            .sorted_by(|a: &&mut ServoCookie, b: &&mut ServoCookie| {
                // The user agent SHOULD sort the cookie-list
                CookieStorage::cookie_comparator(a, b)
            })
            .map(|c| {
                // Update the last-access-time of each cookie in the cookie-list to the current date and time
                c.touch();
                c.cookie.clone()
            })
            .collect();
        if !result.is_empty() {
            self.bump_revision();
        }
        result.into_iter()
    }

    /// Return a bounded canonical snapshot suitable for privileged controlled-session transfer.
    ///
    /// Persistent cookies are rejected. Creation and last-access ordering use checked,
    /// controller-owned stamps rather than Servo's ordinary wall-clock timestamps. Partitioned
    /// cookies are rejected because the current jar has no partition key.
    pub fn export_state(&mut self) -> Result<CookieStateSnapshotV1, CookieStateError> {
        self.export_state_with_policy(ControlledCookiePolicy::SessionV1)
    }

    /// Export state using either the frozen v1 session-cookie boundary or v2 controlled time.
    pub fn export_state_with_policy(
        &mut self,
        policy: ControlledCookiePolicy,
    ) -> Result<CookieStateSnapshotV1, CookieStateError> {
        match policy {
            ControlledCookiePolicy::SessionV1 => self.remove_all_expired_cookies(),
            ControlledCookiePolicy::SessionV2 { unix_time_ns } => {
                self.remove_all_controlled_expired_cookies(unix_time_ns)
                    .map_err(controlled_policy_to_state_error)?;
            },
        }
        if self.revision_exhausted {
            return Err(CookieStateError::RevisionExhausted);
        }

        let cookies: Vec<&ServoCookie> = self.cookies_map.values().flatten().collect();
        if cookies.len() > COOKIE_STATE_MAX_COOKIES_V1 {
            return Err(CookieStateError::TooManyCookies);
        }
        if matches!(policy, ControlledCookiePolicy::SessionV1)
            && cookies.iter().any(|cookie| cookie.persistent)
        {
            return Err(CookieStateError::PersistentCookieUnsupported);
        }
        if matches!(policy, ControlledCookiePolicy::SessionV2 { .. })
            && cookies
                .iter()
                .any(|cookie| cookie.persistent && cookie.controlled_expiry_time_ns.is_none())
        {
            return Err(CookieStateError::PersistentCookieUnsupported);
        }
        if cookies
            .iter()
            .any(|cookie| cookie.cookie.partitioned() == Some(true))
        {
            return Err(CookieStateError::PartitionedCookieUnsupported);
        }

        self.ensure_controlled_ordering()
            .map_err(|()| CookieStateError::RevisionExhausted)?;
        let cookies: Vec<&ServoCookie> = self.cookies_map.values().flatten().collect();

        let mut creation_order = cookies.clone();
        creation_order.sort_by(|a, b| {
            a.controlled_creation_sequence
                .cmp(&b.controlled_creation_sequence)
                .then_with(|| cookie_identity(a).cmp(&cookie_identity(b)))
        });
        let creation_sequences: HashMap<_, _> = creation_order
            .into_iter()
            .enumerate()
            .map(|(sequence, cookie)| (cookie_identity(cookie), sequence as u64))
            .collect();

        let mut access_order = cookies.clone();
        access_order.sort_by(|a, b| {
            a.controlled_last_access_sequence
                .cmp(&b.controlled_last_access_sequence)
                .then_with(|| cookie_identity(a).cmp(&cookie_identity(b)))
        });
        let access_sequences: HashMap<_, _> = access_order
            .into_iter()
            .enumerate()
            .map(|(sequence, cookie)| (cookie_identity(cookie), sequence as u64))
            .collect();

        let mut total_bytes = 0usize;
        let mut records = Vec::with_capacity(cookies.len());
        for cookie in cookies {
            let identity = cookie_identity(cookie);
            let public_domain = public_cookie_state_domain(&identity.0);
            let record_bytes = cookie.cookie.name().len()
                + cookie.cookie.value().len()
                + public_domain.len()
                + identity.1.len();
            if record_bytes > COOKIE_STATE_MAX_COOKIE_BYTES_V1 {
                return Err(CookieStateError::CookieTooLarge);
            }
            total_bytes = total_bytes
                .checked_add(record_bytes)
                .ok_or(CookieStateError::SnapshotTooLarge)?;
            if total_bytes > COOKIE_STATE_MAX_TOTAL_BYTES_V1 {
                return Err(CookieStateError::SnapshotTooLarge);
            }
            records.push(CookieStateRecordV1 {
                name: cookie.cookie.name().to_owned(),
                value: cookie.cookie.value().to_owned(),
                domain: public_domain.to_owned(),
                path: identity.1.clone(),
                host_only: cookie.host_only,
                secure: cookie.cookie.secure().unwrap_or(false),
                http_only: cookie.cookie.http_only().unwrap_or(false),
                same_site: project_same_site(cookie.cookie.same_site()),
                expires_unix_time_ns: match policy {
                    ControlledCookiePolicy::SessionV1 => None,
                    ControlledCookiePolicy::SessionV2 { .. } => cookie.controlled_expiry_time_ns,
                },
                partitioned: false,
                creation_sequence: creation_sequences[&identity],
                last_access_sequence: access_sequences[&identity],
            });
        }
        records.sort_by(|a, b| cookie_record_identity(a).cmp(&cookie_record_identity(b)));
        validate_cookie_state_encoded_array(&records)?;

        Ok(CookieStateSnapshotV1 {
            schema_version: COOKIE_STATE_SCHEMA_VERSION_V1,
            revision: self.revision,
            cookies: records,
        })
    }

    /// Atomically replace this jar from a fully validated session-cookie snapshot.
    pub fn replace_state(
        &mut self,
        expected_revision: u64,
        snapshot: CookieStateSnapshotV1,
    ) -> Result<u64, CookieStateError> {
        self.replace_state_with_policy(
            ControlledCookiePolicy::SessionV1,
            expected_revision,
            snapshot,
        )
    }

    /// Atomically replace this jar under an explicit controlled-session cookie policy.
    pub fn replace_state_with_policy(
        &mut self,
        policy: ControlledCookiePolicy,
        expected_revision: u64,
        snapshot: CookieStateSnapshotV1,
    ) -> Result<u64, CookieStateError> {
        if self.revision_exhausted {
            return Err(CookieStateError::RevisionExhausted);
        }
        if expected_revision != self.revision {
            return Err(CookieStateError::StaleRevision);
        }
        let next_revision = self
            .revision
            .checked_add(1)
            .ok_or(CookieStateError::RevisionExhausted)?;
        if snapshot.schema_version != COOKIE_STATE_SCHEMA_VERSION_V1 {
            return Err(CookieStateError::UnsupportedSchemaVersion);
        }
        if snapshot.cookies.len() > COOKIE_STATE_MAX_COOKIES_V1 {
            return Err(CookieStateError::TooManyCookies);
        }
        validate_cookie_state_encoded_array(&snapshot.cookies)?;

        let mut identities = HashSet::new();
        let mut creation_sequences = HashSet::new();
        let mut access_sequences = HashSet::new();
        let mut total_bytes = 0usize;
        let mut replacement: HashMap<String, Vec<ServoCookie>> = HashMap::new();
        let mut maximum_creation_sequence = None;
        let mut maximum_access_sequence = None;
        for record in snapshot.cookies {
            if record.partitioned {
                return Err(CookieStateError::PartitionedCookieUnsupported);
            }
            if matches!(policy, ControlledCookiePolicy::SessionV1)
                && record.expires_unix_time_ns.is_some()
            {
                return Err(CookieStateError::PersistentCookieUnsupported);
            }
            let identity = cookie_record_identity(&record);
            if !identities.insert(identity) {
                return Err(CookieStateError::DuplicateCookieIdentity);
            }
            if !creation_sequences.insert(record.creation_sequence) {
                return Err(CookieStateError::DuplicateCreationSequence);
            }
            if !access_sequences.insert(record.last_access_sequence) {
                return Err(CookieStateError::DuplicateLastAccessSequence);
            }
            maximum_creation_sequence = Some(
                maximum_creation_sequence.map_or(record.creation_sequence, |maximum: u64| {
                    maximum.max(record.creation_sequence)
                }),
            );
            maximum_access_sequence = Some(
                maximum_access_sequence.map_or(record.last_access_sequence, |maximum: u64| {
                    maximum.max(record.last_access_sequence)
                }),
            );
            let record_bytes =
                record.name.len() + record.value.len() + record.domain.len() + record.path.len();
            if record_bytes > COOKIE_STATE_MAX_COOKIE_BYTES_V1 {
                return Err(CookieStateError::CookieTooLarge);
            }
            total_bytes = total_bytes
                .checked_add(record_bytes)
                .ok_or(CookieStateError::SnapshotTooLarge)?;
            if total_bytes > COOKIE_STATE_MAX_TOTAL_BYTES_V1 {
                return Err(CookieStateError::SnapshotTooLarge);
            }

            if let ControlledCookiePolicy::SessionV2 { unix_time_ns } = policy {
                let now = u64::try_from(unix_time_ns)
                    .map_err(|_| CookieStateError::TimeRangeUnsupported)?;
                if record
                    .expires_unix_time_ns
                    .is_some_and(|expiry| expiry <= now)
                {
                    continue;
                }
            }

            let cookie = cookie_from_state_record_with_policy(record, policy)?;
            let host = reg_host(cookie.cookie.domain().as_deref().unwrap_or(""));
            let host_cookies = replacement.entry(host).or_default();
            if host_cookies.len() == self.max_per_host {
                return Err(CookieStateError::TooManyCookies);
            }
            host_cookies.push(cookie);
        }

        self.cookies_map = replacement;
        self.revision = next_revision;
        (
            self.controlled_creation_sequence_next,
            self.controlled_creation_sequence_exhausted,
        ) = next_controlled_sequence(maximum_creation_sequence);
        (
            self.controlled_access_sequence_next,
            self.controlled_access_sequence_exhausted,
        ) = next_controlled_sequence(maximum_access_sequence);
        Ok(self.revision)
    }

    fn push_controlled(
        &mut self,
        mut cookie: ServoCookie,
        url: &ServoUrl,
        source: CookieSource,
    ) -> Result<(), ControlledCookiePolicyError> {
        self.ensure_controlled_ordering()
            .map_err(|()| ControlledCookiePolicyError::InvalidCookie)?;
        cookie.controlled_creation_sequence = Some(
            self.reserve_controlled_creation_sequence()
                .map_err(|()| ControlledCookiePolicyError::InvalidCookie)?,
        );
        cookie.controlled_last_access_sequence = Some(
            self.reserve_controlled_access_sequences(1)
                .map_err(|()| ControlledCookiePolicyError::InvalidCookie)?
                .into_iter()
                .next()
                .ok_or(ControlledCookiePolicyError::InvalidCookie)?,
        );
        self.push(cookie, url, source);
        Ok(())
    }

    fn push_controlled_v2(
        &mut self,
        mut cookie: ServoCookie,
        url: &ServoUrl,
        source: CookieSource,
        unix_time_ns: u128,
    ) -> Result<(), ControlledCookiePolicyError> {
        let now = u64::try_from(unix_time_ns)
            .map_err(|_| ControlledCookiePolicyError::TimeRangeUnsupported)?;
        self.ensure_controlled_ordering()
            .map_err(|()| ControlledCookiePolicyError::InvalidCookie)?;

        if cookie
            .controlled_expiry_time_ns
            .is_some_and(|expiry| expiry <= now)
        {
            self.remove(&cookie, url, source)
                .map_err(|_| ControlledCookiePolicyError::InvalidCookie)?;
            return Ok(());
        }

        cookie.controlled_creation_sequence = Some(
            self.reserve_controlled_creation_sequence()
                .map_err(|()| ControlledCookiePolicyError::InvalidCookie)?,
        );
        cookie.controlled_last_access_sequence = Some(
            self.reserve_controlled_access_sequences(1)
                .map_err(|()| ControlledCookiePolicyError::InvalidCookie)?
                .into_iter()
                .next()
                .ok_or(ControlledCookiePolicyError::InvalidCookie)?,
        );

        if cookie.cookie.secure().unwrap_or(false) && !url.is_secure_scheme() {
            return Err(ControlledCookiePolicyError::InvalidCookie);
        }
        let old_cookie = self
            .remove(&cookie, url, source)
            .map_err(|_| ControlledCookiePolicyError::InvalidCookie)?;
        if let Some(old_cookie) = old_cookie {
            cookie.creation_time = old_cookie.creation_time;
            cookie.controlled_creation_sequence = old_cookie
                .controlled_creation_sequence
                .or(cookie.controlled_creation_sequence);
        }

        let domain = reg_host(cookie.cookie.domain().as_ref().unwrap_or(&""));
        let cookies = self.cookies_map.entry(domain).or_default();
        if cookies.len() == self.max_per_host
            && !evict_one_controlled_cookie(cookie.cookie.secure().unwrap_or(false), cookies)
        {
            return Err(ControlledCookiePolicyError::InvalidCookie);
        }
        cookies.push(cookie);
        self.bump_revision();
        Ok(())
    }

    fn remove_controlled_expired_cookies_for_url(
        &mut self,
        url: &ServoUrl,
        unix_time_ns: u128,
    ) -> Result<(), ControlledCookiePolicyError> {
        let now = u64::try_from(unix_time_ns)
            .map_err(|_| ControlledCookiePolicyError::TimeRangeUnsupported)?;
        let domain = reg_host(url.host_str().unwrap_or(""));
        let mut changed = false;
        if let Entry::Occupied(mut entry) = self.cookies_map.entry(domain) {
            let cookies = entry.get_mut();
            let old_len = cookies.len();
            cookies.retain(|cookie| {
                !cookie
                    .controlled_expiry_time_ns
                    .is_some_and(|expiry| expiry <= now)
            });
            changed = cookies.len() != old_len;
            if cookies.is_empty() {
                entry.remove_entry();
            }
        }
        if changed {
            self.bump_revision();
        }
        Ok(())
    }

    fn remove_all_controlled_expired_cookies(
        &mut self,
        unix_time_ns: u128,
    ) -> Result<(), ControlledCookiePolicyError> {
        let now = u64::try_from(unix_time_ns)
            .map_err(|_| ControlledCookiePolicyError::TimeRangeUnsupported)?;
        let old_len: usize = self.cookies_map.values().map(Vec::len).sum();
        self.cookies_map.retain(|_, cookies| {
            cookies.retain(|cookie| {
                !cookie
                    .controlled_expiry_time_ns
                    .is_some_and(|expiry| expiry <= now)
            });
            !cookies.is_empty()
        });
        let new_len: usize = self.cookies_map.values().map(Vec::len).sum();
        if new_len != old_len {
            self.bump_revision();
        }
        Ok(())
    }

    fn ensure_controlled_ordering(&mut self) -> Result<(), ()> {
        if self
            .cookies_map
            .values()
            .flatten()
            .any(|cookie| cookie.controlled_creation_sequence.is_none())
        {
            self.compact_controlled_creation_sequences()?;
        }
        if self
            .cookies_map
            .values()
            .flatten()
            .any(|cookie| cookie.controlled_last_access_sequence.is_none())
        {
            self.compact_controlled_access_sequences()?;
        }
        Ok(())
    }

    fn reserve_controlled_creation_sequence(&mut self) -> Result<u64, ()> {
        if self.controlled_creation_sequence_exhausted {
            self.compact_controlled_creation_sequences()?;
        }
        let sequence = self.controlled_creation_sequence_next;
        match sequence.checked_add(1) {
            Some(next) => self.controlled_creation_sequence_next = next,
            None => self.controlled_creation_sequence_exhausted = true,
        }
        Ok(sequence)
    }

    fn reserve_controlled_access_sequences(&mut self, count: usize) -> Result<Vec<u64>, ()> {
        if count == 0 {
            return Ok(Vec::new());
        }
        let count = u64::try_from(count).map_err(|_| ())?;
        let last_offset = count.checked_sub(1).ok_or(())?;
        if self.controlled_access_sequence_exhausted
            || self
                .controlled_access_sequence_next
                .checked_add(last_offset)
                .is_none()
        {
            self.compact_controlled_access_sequences()?;
        }
        let first = self.controlled_access_sequence_next;
        let last = first.checked_add(last_offset).ok_or(())?;
        match last.checked_add(1) {
            Some(next) => self.controlled_access_sequence_next = next,
            None => self.controlled_access_sequence_exhausted = true,
        }
        Ok((first..=last).collect())
    }

    fn compact_controlled_creation_sequences(&mut self) -> Result<(), ()> {
        let mut cookies: Vec<_> = self.cookies_map.values_mut().flatten().collect();
        cookies.sort_by(|a, b| {
            optional_controlled_sequence_comparator(
                a.controlled_creation_sequence,
                b.controlled_creation_sequence,
            )
            .then_with(|| cookie_identity(a).cmp(&cookie_identity(b)))
        });
        for (sequence, cookie) in cookies.iter_mut().enumerate() {
            cookie.controlled_creation_sequence = Some(u64::try_from(sequence).map_err(|_| ())?);
        }
        self.controlled_creation_sequence_next = u64::try_from(cookies.len()).map_err(|_| ())?;
        self.controlled_creation_sequence_exhausted = false;
        Ok(())
    }

    fn compact_controlled_access_sequences(&mut self) -> Result<(), ()> {
        let mut cookies: Vec<_> = self.cookies_map.values_mut().flatten().collect();
        cookies.sort_by(|a, b| {
            optional_controlled_sequence_comparator(
                a.controlled_last_access_sequence,
                b.controlled_last_access_sequence,
            )
            .then_with(|| cookie_identity(a).cmp(&cookie_identity(b)))
        });
        for (sequence, cookie) in cookies.iter_mut().enumerate() {
            cookie.controlled_last_access_sequence = Some(u64::try_from(sequence).map_err(|_| ())?);
        }
        self.controlled_access_sequence_next = u64::try_from(cookies.len()).map_err(|_| ())?;
        self.controlled_access_sequence_exhausted = false;
        Ok(())
    }

    pub fn cookie_site_descriptors(&self) -> Vec<SiteDescriptor> {
        self.cookies_map
            .keys()
            .cloned()
            .map(SiteDescriptor::new)
            .collect()
    }
}

/// Bound the exact compact public cookie-array projection used by the shell and SDK.
///
/// The lower backend record uses snake_case fields and numeric sequence values on its private
/// channel, while the public v1 projection uses camelCase and canonical decimal strings. Measuring
/// the private representation would reject valid public fragments near the frozen boundary, so
/// this serializer deliberately mirrors the public representation byte-for-byte.
fn validate_cookie_state_encoded_array(
    records: &[CookieStateRecordV1],
) -> Result<(), CookieStateError> {
    let mut counter =
        CookieStateEncodedSizeCounter::new(COOKIE_STATE_MAX_ENCODED_PUBLIC_ARRAY_BYTES_V1);
    serde_json::to_writer(&mut counter, &PublicCookieStateArray(records))
        .map_err(|_| CookieStateError::SnapshotTooLarge)
}

struct PublicCookieStateArray<'a>(&'a [CookieStateRecordV1]);

impl Serialize for PublicCookieStateArray<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for cookie in self.0 {
            sequence.serialize_element(&PublicCookieStateRecord::from(cookie))?;
        }
        sequence.end()
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PublicCookieStateRecord<'a> {
    name: &'a str,
    value: &'a str,
    domain: &'a str,
    path: &'a str,
    host_only: bool,
    secure: bool,
    http_only: bool,
    same_site: CookieStateSameSite,
    expires_unix_time_ns: Option<PublicWireU64>,
    partitioned: bool,
    creation_sequence: PublicWireU64,
    last_access_sequence: PublicWireU64,
}

impl<'a> From<&'a CookieStateRecordV1> for PublicCookieStateRecord<'a> {
    fn from(cookie: &'a CookieStateRecordV1) -> Self {
        Self {
            name: &cookie.name,
            value: &cookie.value,
            domain: &cookie.domain,
            path: &cookie.path,
            host_only: cookie.host_only,
            secure: cookie.secure,
            http_only: cookie.http_only,
            same_site: cookie.same_site,
            expires_unix_time_ns: cookie.expires_unix_time_ns.map(PublicWireU64),
            partitioned: cookie.partitioned,
            creation_sequence: PublicWireU64(cookie.creation_sequence),
            last_access_sequence: PublicWireU64(cookie.last_access_sequence),
        }
    }
}

#[derive(Clone, Copy)]
struct PublicWireU64(u64);

impl Serialize for PublicWireU64 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}

struct CookieStateEncodedSizeCounter {
    bytes: usize,
    maximum: usize,
}

impl CookieStateEncodedSizeCounter {
    const fn new(maximum: usize) -> Self {
        Self { bytes: 0, maximum }
    }
}

impl Write for CookieStateEncodedSizeCounter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(buffer.len())
            .ok_or_else(|| io::Error::other("cookie state exceeds encoded fragment limit"))?;
        if self.bytes > self.maximum {
            return Err(io::Error::other(
                "cookie state exceeds encoded fragment limit",
            ));
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn cookie_identity(cookie: &ServoCookie) -> (String, String, String) {
    (
        cookie.cookie.domain().unwrap_or_default().to_owned(),
        cookie.cookie.path().unwrap_or("/").to_owned(),
        cookie.cookie.name().to_owned(),
    )
}

fn controlled_cookie_comparator(a: &ServoCookie, b: &ServoCookie) -> Ordering {
    let a_path_len = a.cookie.path().as_ref().map_or(0, |path| path.len());
    let b_path_len = b.cookie.path().as_ref().map_or(0, |path| path.len());
    match a_path_len.cmp(&b_path_len) {
        Ordering::Equal => a
            .controlled_creation_sequence
            .cmp(&b.controlled_creation_sequence)
            .then_with(|| cookie_identity(a).cmp(&cookie_identity(b))),
        Ordering::Greater => Ordering::Less,
        Ordering::Less => Ordering::Greater,
    }
}

fn optional_controlled_sequence_comparator(a: Option<u64>, b: Option<u64>) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn next_controlled_sequence(maximum: Option<u64>) -> (u64, bool) {
    match maximum {
        None => (0, false),
        Some(maximum) => match maximum.checked_add(1) {
            Some(next) => (next, false),
            None => (0, true),
        },
    }
}

fn cookie_record_identity(record: &CookieStateRecordV1) -> (String, String, String) {
    (
        record.domain.clone(),
        record.path.clone(),
        record.name.clone(),
    )
}

fn controlled_same_site(
    request_url: &ServoUrl,
    context: &ControlledCookieContext,
) -> Result<bool, ControlledCookiePolicyError> {
    if !matches!(request_url.scheme(), "http" | "https") {
        return Err(ControlledCookiePolicyError::SameSiteContextUnsupported);
    }
    let site_for_cookies = context
        .site_for_cookies
        .as_ref()
        .ok_or(ControlledCookiePolicyError::SameSiteContextUnsupported)?;
    let site_for_cookies = ServoUrl::from_url(site_for_cookies.clone());
    if !matches!(site_for_cookies.scheme(), "http" | "https") {
        return Err(ControlledCookiePolicyError::SameSiteContextUnsupported);
    }
    Ok(is_same_site(
        &request_url.origin(),
        &site_for_cookies.origin(),
    ))
}

fn controlled_same_site_allows_cookie(
    cookie: &ServoCookie,
    same_site: bool,
    method: &Method,
    context: &ControlledCookieContext,
) -> bool {
    if same_site || matches!(context.policy, ControlledCookiePolicy::SessionV1) {
        return true;
    }
    match cookie.cookie.same_site() {
        Some(SameSite::None) => true,
        Some(SameSite::Strict) => false,
        Some(SameSite::Lax) | None => {
            context.top_level_navigation && controlled_method_is_safe(method)
        },
    }
}

fn controlled_method_is_safe(method: &Method) -> bool {
    method == Method::GET
        || method == Method::HEAD
        || method == Method::OPTIONS
        || method == Method::TRACE
}

fn controlled_policy_to_state_error(error: ControlledCookiePolicyError) -> CookieStateError {
    match error {
        ControlledCookiePolicyError::TimeRangeUnsupported => CookieStateError::TimeRangeUnsupported,
        ControlledCookiePolicyError::PartitionedCookieUnsupported => {
            CookieStateError::PartitionedCookieUnsupported
        },
        ControlledCookiePolicyError::PersistentCookieUnsupported => {
            CookieStateError::PersistentCookieUnsupported
        },
        ControlledCookiePolicyError::SameSiteContextUnsupported
        | ControlledCookiePolicyError::InvalidCookie => CookieStateError::InvalidCookie,
    }
}

fn state_error_to_controlled_policy(error: CookieStateError) -> ControlledCookiePolicyError {
    match error {
        CookieStateError::TimeRangeUnsupported => ControlledCookiePolicyError::TimeRangeUnsupported,
        CookieStateError::PartitionedCookieUnsupported => {
            ControlledCookiePolicyError::PartitionedCookieUnsupported
        },
        CookieStateError::PersistentCookieUnsupported => {
            ControlledCookiePolicyError::PersistentCookieUnsupported
        },
        _ => ControlledCookiePolicyError::InvalidCookie,
    }
}

fn has_cookie_attribute(cookie_value: &str, expected: &str) -> bool {
    cookie_value.split(';').skip(1).any(|attribute| {
        attribute
            .trim()
            .split_once('=')
            .map_or(attribute.trim(), |(name, _)| name.trim())
            .eq_ignore_ascii_case(expected)
    })
}

fn project_same_site(same_site: Option<SameSite>) -> CookieStateSameSite {
    match same_site {
        None => CookieStateSameSite::Unspecified,
        Some(SameSite::Strict) => CookieStateSameSite::Strict,
        Some(SameSite::Lax) => CookieStateSameSite::Lax,
        Some(SameSite::None) => CookieStateSameSite::None,
    }
}

fn cookie_from_state_record_with_policy(
    record: CookieStateRecordV1,
    policy: ControlledCookiePolicy,
) -> Result<ServoCookie, CookieStateError> {
    if !is_canonical_cookie_state_domain(&record.domain)
        || !record.path.starts_with('/')
        || !is_valid_cookie_state_name_and_value(&record.name, &record.value)
        || !has_valid_cookie_state_prefix(
            &record.name,
            record.secure,
            record.host_only,
            &record.path,
        )
    {
        return Err(CookieStateError::InvalidCookie);
    }
    if record.same_site == CookieStateSameSite::None && !record.secure {
        return Err(CookieStateError::InvalidCookie);
    }

    let creation_sequence = record.creation_sequence;
    let last_access_sequence = record.last_access_sequence;
    let expires_unix_time_ns = record.expires_unix_time_ns;
    if let (ControlledCookiePolicy::SessionV2 { unix_time_ns }, Some(expiry)) =
        (policy, expires_unix_time_ns)
    {
        let now =
            u64::try_from(unix_time_ns).map_err(|_| CookieStateError::TimeRangeUnsupported)?;
        let maximum = now
            .checked_add(34_560_000_u64 * 1_000_000_000)
            .ok_or(CookieStateError::TimeRangeUnsupported)?;
        if expiry > maximum {
            return Err(CookieStateError::TimeRangeUnsupported);
        }
    }
    let mut cookie = Cookie::new(record.name.clone(), record.value.clone());
    if !record.host_only {
        cookie.set_domain(internal_cookie_state_domain(&record.domain));
    }
    cookie.set_path(record.path.clone());
    cookie.set_secure(record.secure);
    cookie.set_http_only(record.http_only);
    cookie.set_same_site(match record.same_site {
        CookieStateSameSite::Unspecified => None,
        CookieStateSameSite::Strict => Some(SameSite::Strict),
        CookieStateSameSite::Lax => Some(SameSite::Lax),
        CookieStateSameSite::None => Some(SameSite::None),
    });

    let authority = internal_cookie_state_domain(&record.domain);
    let scheme = if record.secure { "https" } else { "http" };
    let request = ServoUrl::parse(&format!("{scheme}://{authority}{}", record.path))
        .map_err(|_| CookieStateError::InvalidCookie)?;
    let mut wrapped = match policy {
        ControlledCookiePolicy::SessionV1 => {
            ServoCookie::new_wrapped(cookie, &request, CookieSource::HTTP)
        },
        ControlledCookiePolicy::SessionV2 { unix_time_ns } => {
            ServoCookie::new_controlled_wrapped(cookie, &request, CookieSource::HTTP, unix_time_ns)
                .map_err(controlled_policy_to_state_error)?
        },
    }
    .ok_or(CookieStateError::InvalidCookie)?;
    // Servo's public-suffix helper treats every dotless string as a suffix, including its
    // bracketed internal IPv6 host spelling, and therefore converts an IPv6 Domain attribute to
    // host-only. Portable state keeps the caller's explicit flag. Restoring `false` here is safe:
    // the internal bracketed address can only domain-match that exact IP literal.
    if record.domain.contains(':') && !record.host_only && wrapped.host_only {
        wrapped.host_only = false;
    }
    if wrapped.host_only != record.host_only
        || wrapped.cookie.domain().map(public_cookie_state_domain) != Some(record.domain.as_str())
    {
        return Err(CookieStateError::InvalidCookie);
    }
    wrapped.creation_time = SystemTime::UNIX_EPOCH
        .checked_add(Duration::from_nanos(record.creation_sequence))
        .ok_or(CookieStateError::InvalidCookie)?;
    wrapped.last_access = SystemTime::UNIX_EPOCH
        .checked_add(Duration::from_nanos(last_access_sequence))
        .ok_or(CookieStateError::InvalidCookie)?;
    wrapped.controlled_creation_sequence = Some(creation_sequence);
    wrapped.controlled_last_access_sequence = Some(last_access_sequence);
    wrapped.persistent = expires_unix_time_ns.is_some();
    wrapped.expiry_time = None;
    wrapped.controlled_expiry_time_ns = expires_unix_time_ns;
    Ok(wrapped)
}

fn internal_cookie_state_domain(domain: &str) -> String {
    if domain.contains(':') {
        format!("[{domain}]")
    } else {
        domain.to_owned()
    }
}

fn public_cookie_state_domain(domain: &str) -> &str {
    domain
        .strip_prefix('[')
        .and_then(|domain| domain.strip_suffix(']'))
        .unwrap_or(domain)
}

fn reg_host(url: &str) -> String {
    let host_for_ip_parse = url
        .strip_prefix('[')
        .and_then(|url| url.strip_suffix(']'))
        .unwrap_or(url);
    if let Ok(address) = host_for_ip_parse.parse::<IpAddr>() {
        return address.to_string().to_lowercase();
    }

    reg_suffix(url).to_lowercase()
}

fn is_cookie_expired(cookie: &ServoCookie) -> bool {
    matches!(cookie.expiry_time, Some(date_time) if date_time <= SystemTime::now())
}

fn evict_one_cookie(is_secure_cookie: bool, cookies: &mut Vec<ServoCookie>) -> bool {
    // Remove non-secure cookie with oldest access time
    let oldest_accessed = get_oldest_accessed(false, cookies);

    if let Some((index, _)) = oldest_accessed {
        cookies.remove(index);
    } else {
        // All secure cookies were found
        if !is_secure_cookie {
            return false;
        }
        let oldest_accessed = get_oldest_accessed(true, cookies);
        if let Some((index, _)) = oldest_accessed {
            cookies.remove(index);
        }
    }
    true
}

fn evict_one_controlled_cookie(is_secure_cookie: bool, cookies: &mut Vec<ServoCookie>) -> bool {
    let oldest = |secure: bool, cookies: &[ServoCookie]| {
        cookies
            .iter()
            .enumerate()
            .filter(|(_, cookie)| cookie.cookie.secure().unwrap_or(false) == secure)
            .min_by(|(_, left), (_, right)| {
                left.controlled_last_access_sequence
                    .cmp(&right.controlled_last_access_sequence)
                    .then_with(|| cookie_identity(left).cmp(&cookie_identity(right)))
            })
            .map(|(index, _)| index)
    };
    if let Some(index) = oldest(false, cookies) {
        cookies.remove(index);
        return true;
    }
    if !is_secure_cookie {
        return false;
    }
    if let Some(index) = oldest(true, cookies) {
        cookies.remove(index);
        return true;
    }
    false
}

fn get_oldest_accessed(
    is_secure_cookie: bool,
    cookies: &mut [ServoCookie],
) -> Option<(usize, SystemTime)> {
    let mut oldest_accessed = None;
    for (i, c) in cookies.iter().enumerate() {
        if (c.cookie.secure().unwrap_or(false) == is_secure_cookie)
            && oldest_accessed
                .as_ref()
                .is_none_or(|(_, current_oldest_time)| c.last_access < *current_oldest_time)
        {
            oldest_accessed = Some((i, c.last_access));
        }
    }
    oldest_accessed
}
