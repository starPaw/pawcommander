use super::*;

#[test]
fn remote_addresses_round_trip_and_keep_connection_and_posix_history() {
    let local = Location::Local(PathBuf::from("/tmp"));
    let location = resolve("sftp://alice@example.com:2222/папка/two%20words", &local).unwrap();
    assert_eq!(location.path(), Path::new("/папка/two words"));
    assert_eq!(resolve(&location.display(), &local).unwrap(), location);
    assert_eq!(
        resolve("../next", &location).unwrap().path(),
        Path::new("/папка/next")
    );
    assert_eq!(
        resolve("/../../", &location).unwrap().path(),
        Path::new("/")
    );
    assert!(location.ancestors().all(|p| p.remote()));
    assert!(location.ancestors().last().unwrap().parent().is_none());
    let ipv6 = resolve("sftp://alice@[::1]:2222/a", &local).unwrap();
    assert_eq!(resolve(&ipv6.display(), &local).unwrap(), ipv6);
    let special = location.with_path(PathBuf::from("/literal%20/100% #?.txt"));
    assert_eq!(resolve(&special.display(), &local).unwrap(), special);
    let form_ipv6 = Location::Sftp {
        endpoint: Endpoint {
            host: "::1".into(),
            port: 2222,
            user: "a%40b".into(),
            key: None,
            jump: None,
        },
        path: "/".into(),
    };
    let parsed_ipv6 = resolve(&form_ipv6.display(), &local).unwrap();
    assert_eq!(parsed_ipv6.path(), form_ipv6.path());
    if let Location::Sftp { endpoint, .. } = parsed_ipv6 {
        assert_eq!(endpoint.user, "a%40b");
    }
    assert!(resolve("sftp://alice:secret@example.com/", &local).is_err());
    assert!(resolve("sftp://example.com/", &local).is_err());
    assert!(resolve("sftp://alice@example.com/%00", &local).is_err());
    assert!(resolve("sftp://alice@example.com/?q=1", &local).is_err());
}

#[test]
fn remote_key_path_survives_address_edits() {
    let current = Location::Sftp {
        endpoint: Endpoint {
            host: "example.com".into(),
            port: 22,
            user: "alice".into(),
            key: Some(PathBuf::from("/keys/id")),
            jump: None,
        },
        path: "/".into(),
    };
    assert_eq!(
        resolve("sftp://alice@example.com/sub", &current).unwrap(),
        current.join("sub")
    );
}

#[cfg(target_os = "macos")]
#[test]
fn sftp_address_keeps_the_jump_route_for_the_same_server() {
    let local = Location::Local("/tmp".into());
    let direct = resolve("sftp://alice@private.example:2222/a", &local).unwrap();
    let mut routed = direct.clone();
    if let Location::Sftp { endpoint, .. } = &mut routed {
        endpoint.key = Some("/keys/target".into());
        endpoint.jump = Some(Box::new(crate::vfs::JumpHost {
            host: "jump.example".into(),
            port: 22,
            user: "relay".into(),
            key: Some("/keys/jump".into()),
        }));
    }
    assert!(!routed.same_sftp_authority(&direct));
    assert!(routed.might_share_filesystem(&direct));
    assert_eq!(
        resolve("sftp://alice@PRIVATE.example:2222/b", &routed).unwrap(),
        routed.with_path("/b".into())
    );
    let other = resolve("sftp://bob@private.example:2222/b", &routed).unwrap();
    let endpoint = other.sftp_endpoint().unwrap();
    assert_eq!(endpoint.jump, routed.sftp_endpoint().unwrap().jump);
    assert!(endpoint.key.is_none());
    let other_server = resolve("sftp://bob@elsewhere.example:2222/b", &routed).unwrap();
    assert!(other_server.sftp_endpoint().unwrap().jump.is_none());
}

#[test]
fn remote_join_keeps_literal_posix_separators_on_every_client_os() {
    let local = Location::Local(PathBuf::from("/tmp"));
    let root = resolve("sftp://alice@example.com/", &local).unwrap();
    let child = root.join("folder").join("two words");
    assert_eq!(child.path().to_str(), Some("/folder/two words"));
    assert_eq!(child.join("").path().to_str(), Some("/folder/two words"));
    assert_eq!(child.join("/other").path().to_str(), Some("/other"));
    assert_eq!(
        resolve("../next", &child).unwrap().path().to_str(),
        Some("/folder/next")
    );
    assert!(resolve("..\\next", &child).is_err());
    assert!(resolve("sftp://alice@example.com/a%5Cb", &local).is_err());
    assert_eq!(local.join("folder").path(), local.path().join("folder"));
}

#[test]
fn ipv6_address_edits_keep_exact_authenticated_endpoint_without_crossing_authorities() {
    for host in ["::1", "[::1]", "0:0:0:0:0:0:0:1", "[0:0:0:0:0:0:0:1]"] {
        let current = Location::Sftp {
            endpoint: Endpoint {
                host: host.into(),
                port: 2222,
                user: "alice".into(),
                key: Some("/keys/explicit".into()),
                jump: None,
            },
            path: "/".into(),
        };
        for address in [
            "sftp://alice@[::1]:2222/папка",
            "sftp://alice@[0:0:0:0:0:0:0:1]:2222/папка",
        ] {
            let result = resolve(address, &current).unwrap();
            assert_eq!(result, current.join("папка"));
            assert!(result.same_backend(&current));
        }
        for address in [
            "sftp://bob@[::1]:2222/",
            "sftp://alice@[::1]:2223/",
            "sftp://alice@[::2]:2222/",
        ] {
            let other = resolve(address, &current).unwrap();
            assert!(!other.same_sftp_authority(&current));
            let Location::Sftp { endpoint, .. } = other else {
                panic!("remote address")
            };
            assert!(endpoint.key.is_none());
        }
    }
}
