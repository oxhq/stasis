/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::net::IpAddr;
use std::sync::mpsc;

use net::connector::CACertificates;
use net::protocols::ProtocolRegistry;
use net::resource_thread::{
    ResourceThreadOwner, new_core_resource_thread_with_owner,
    new_core_resource_thread_with_owner_and_after_shutdown_for_testing,
};
use net::test::parse_hostsfile;
use net_traits::{CoreResourceMsg, CoreResourceThread};
use profile_traits::mem::ProfilerChan as MemProfilerChan;
use profile_traits::time::ProfilerChan;
use servo_base::generic_channel;

use crate::create_generic_embedder_proxy;

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

fn owned_resource_thread_with_after_shutdown(
    after_shutdown: impl FnOnce() + Send + 'static,
) -> (CoreResourceThread, ResourceThreadOwner) {
    let (tx, _rx) = generic_channel::channel().unwrap();
    let (mtx, _mrx) = generic_channel::channel().unwrap();
    let (resource_thread, _private_resource_thread, owner) =
        new_core_resource_thread_with_owner_and_after_shutdown_for_testing(
            None,
            ProfilerChan(Some(tx)),
            MemProfilerChan(mtx),
            create_generic_embedder_proxy(),
            None,
            CACertificates::Default,
            false,
            std::sync::Arc::new(ProtocolRegistry::default()),
            after_shutdown,
        );
    (resource_thread, owner)
}

fn request_exit(resource_thread: &CoreResourceThread) {
    let (sender, receiver) = generic_channel::oneshot().unwrap();
    resource_thread.send(CoreResourceMsg::Exit(sender)).unwrap();
    receiver.recv().unwrap();
}

#[test]
fn test_exit() {
    let (tx, _rx) = generic_channel::channel().unwrap();
    let (mtx, _mrx) = generic_channel::channel().unwrap();
    let (resource_thread, _private_resource_thread, owner) = new_core_resource_thread_with_owner(
        None,
        ProfilerChan(Some(tx)),
        MemProfilerChan(mtx),
        create_generic_embedder_proxy(),
        None,
        CACertificates::Default,
        false, /* ignore_certificate_errors */
        std::sync::Arc::new(ProtocolRegistry::default()),
    );
    request_exit(&resource_thread);
    owner.join().unwrap();
}

#[test]
fn exit_ack_precedes_physical_resource_thread_termination() {
    let (after_shutdown_entered, wait_for_after_shutdown) = mpsc::channel();
    let (allow_shutdown_to_finish, shutdown_may_finish) = mpsc::channel();
    let (resource_thread, owner) = owned_resource_thread_with_after_shutdown(move || {
        after_shutdown_entered.send(()).unwrap();
        shutdown_may_finish.recv().unwrap();
    });

    request_exit(&resource_thread);
    wait_for_after_shutdown.recv().unwrap();

    assert!(
        !owner.is_finished(),
        "the Exit ACK must not be treated as physical thread termination"
    );
    allow_shutdown_to_finish.send(()).unwrap();
    owner.join().unwrap();
}

#[test]
fn resource_thread_owner_join_surfaces_post_ack_panic() {
    let (after_shutdown_entered, wait_for_after_shutdown) = mpsc::channel();
    let (resource_thread, owner) = owned_resource_thread_with_after_shutdown(move || {
        after_shutdown_entered.send(()).unwrap();
        panic!("post-ACK ResourceManager teardown panic");
    });

    request_exit(&resource_thread);
    wait_for_after_shutdown.recv().unwrap();

    assert!(
        owner.join().is_err(),
        "joining the physical owner must surface a post-ACK panic"
    );
}

#[test]
fn test_parse_hostsfile() {
    let mock_hosts_file_content = "127.0.0.1 foo.bar.com\n127.0.0.2 servo.test.server";
    let hosts_table = parse_hostsfile(mock_hosts_file_content);
    assert_eq!(2, hosts_table.len());
    assert_eq!(ip("127.0.0.1"), *hosts_table.get("foo.bar.com").unwrap());
    assert_eq!(
        ip("127.0.0.2"),
        *hosts_table.get("servo.test.server").unwrap()
    );
}

#[test]
fn test_parse_malformed_hostsfile() {
    let mock_hosts_file_content =
        "malformed file\n127.0.0.1 foo.bar.com\nservo.test.server 127.0.0.1";
    let hosts_table = parse_hostsfile(mock_hosts_file_content);
    assert_eq!(1, hosts_table.len());
    assert_eq!(ip("127.0.0.1"), *hosts_table.get("foo.bar.com").unwrap());
}

#[test]
fn test_parse_hostsfile_with_line_comment() {
    let mock_hosts_file_content =
        "# this is a line comment\n127.0.0.1 foo.bar.com\n# anothercomment";
    let hosts_table = parse_hostsfile(mock_hosts_file_content);
    assert_eq!(1, hosts_table.len());
    assert_eq!(ip("127.0.0.1"), *hosts_table.get("foo.bar.com").unwrap());
}

#[test]
fn test_parse_hostsfile_with_end_of_line_comment() {
    let mock_hosts_file_content =
        "127.0.0.1 foo.bar.com # line ending comment\n127.0.0.2 servo.test.server #comment";
    let hosts_table = parse_hostsfile(mock_hosts_file_content);
    assert_eq!(2, hosts_table.len());
    assert_eq!(ip("127.0.0.1"), *hosts_table.get("foo.bar.com").unwrap());
    assert_eq!(
        ip("127.0.0.2"),
        *hosts_table.get("servo.test.server").unwrap()
    );
}

#[test]
fn test_parse_hostsfile_with_2_hostnames_for_1_address() {
    let mock_hosts_file_content = "127.0.0.1 foo.bar.com baz.bar.com";
    let hosts_table = parse_hostsfile(mock_hosts_file_content);
    assert_eq!(2, hosts_table.len());
    assert_eq!(ip("127.0.0.1"), *hosts_table.get("foo.bar.com").unwrap());
    assert_eq!(ip("127.0.0.1"), *hosts_table.get("baz.bar.com").unwrap());
}

#[test]
fn test_parse_hostsfile_with_4_hostnames_for_1_address() {
    let mock_hosts_file_content = "127.0.0.1 moz.foo.com moz.bar.com moz.baz.com moz.moz.com";
    let hosts_table = parse_hostsfile(mock_hosts_file_content);
    assert_eq!(4, hosts_table.len());
    assert_eq!(ip("127.0.0.1"), *hosts_table.get("moz.foo.com").unwrap());
    assert_eq!(ip("127.0.0.1"), *hosts_table.get("moz.bar.com").unwrap());
    assert_eq!(ip("127.0.0.1"), *hosts_table.get("moz.baz.com").unwrap());
    assert_eq!(ip("127.0.0.1"), *hosts_table.get("moz.moz.com").unwrap());
}

#[test]
fn test_parse_hostsfile_with_tabs_instead_spaces() {
    let mock_hosts_file_content = "127.0.0.1\tfoo.bar.com\n127.0.0.2\tservo.test.server";
    let hosts_table = parse_hostsfile(mock_hosts_file_content);
    assert_eq!(2, hosts_table.len());
    assert_eq!(ip("127.0.0.1"), *hosts_table.get("foo.bar.com").unwrap());
    assert_eq!(
        ip("127.0.0.2"),
        *hosts_table.get("servo.test.server").unwrap()
    );
}

#[test]
fn test_parse_hostsfile_with_valid_ipv4_addresses() {
    let mock_hosts_file_content =
        "255.255.255.255 foo.bar.com\n169.0.1.201 servo.test.server\n192.168.5.0 servo.foo.com";
    let hosts_table = parse_hostsfile(mock_hosts_file_content);
    assert_eq!(3, hosts_table.len());
}

#[test]
fn test_parse_hostsfile_with_invalid_ipv4_addresses() {
    let mock_hosts_file_content = "256.255.255.255 foo.bar.com\n169.0.1000.201 servo.test.server \
                                   \n192.168.5.500 servo.foo.com\n192.abc.100.2 test.servo.com";
    let hosts_table = parse_hostsfile(mock_hosts_file_content);
    assert_eq!(0, hosts_table.len());
}

#[test]
fn test_parse_hostsfile_with_valid_ipv6_addresses() {
    let mock_hosts_file_content = "2001:0db8:0000:0000:0000:ff00:0042:8329 foo.bar.com\n\
                                   2001:db8:0:0:0:ff00:42:8329 moz.foo.com\n\
                                   2001:db8::ff00:42:8329 foo.moz.com moz.moz.com\n\
                                   0000:0000:0000:0000:0000:0000:0000:0001 bar.moz.com\n\
                                   ::1 foo.bar.baz baz.foo.com\n\
                                   2001:0DB8:85A3:0042:1000:8A2E:0370:7334 baz.bar.moz\n\
                                   :: unspecified.moz.com";
    let hosts_table = parse_hostsfile(mock_hosts_file_content);
    assert_eq!(9, hosts_table.len());
}

#[test]
fn test_parse_hostsfile_with_invalid_ipv6_addresses() {
    let mock_hosts_file_content = "12001:0db8:0000:0000:0000:ff00:0042:8329 foo.bar.com\n\
                                   2001:zdb8:0:0:0:gg00:42:t329 moz.foo.com\n\
                                   2002:0DB8:85A3:0042:1000:8A2E:0370:7334/1289 baz3.bar.moz";
    let hosts_table = parse_hostsfile(mock_hosts_file_content);
    assert_eq!(0, hosts_table.len());
}

#[test]
fn test_parse_hostsfile_with_end_of_line_whitespace() {
    let mock_hosts_file_content = "127.0.0.1 foo.bar.com \n\
                                   2001:db8:0:0:0:ff00:42:8329 moz.foo.com\n \
                                   127.0.0.2 servo.test.server ";
    let hosts_table = parse_hostsfile(mock_hosts_file_content);
    assert_eq!(3, hosts_table.len());
    assert_eq!(ip("127.0.0.1"), *hosts_table.get("foo.bar.com").unwrap());
    assert_eq!(
        ip("2001:db8:0:0:0:ff00:42:8329"),
        *hosts_table.get("moz.foo.com").unwrap()
    );
    assert_eq!(
        ip("127.0.0.2"),
        *hosts_table.get("servo.test.server").unwrap()
    );
}
