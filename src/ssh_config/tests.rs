use super::*;

#[test]
fn ssh_config_resolves_matching_blocks_and_ordered_identity_files() {
    let home = Path::new("/fixture/home");
    let config = r#"# earlier defaults and unrelated hosts
Host other
  HostName other.example
  ProxyCommand unrelated
Host Prod build-? !build-z
  HostName repo.example
  User release
  Port = 2202
  IdentityFile "~/.ssh/work # key" # inside quotes it is part of the name
  IdentityFile %d/.ssh/%n-%h-%p-%r-%%
  ProxyJump bastion
Host Prod
  Port 2299
  User fallback
  IdentityFile none
Host *
  User local-default
  Port 22
  IdentityFile "${HOME}/.ssh/work # key"
  Include conf.d/*.conf
Match exec "unsupported-command"
  HostName should-not-be-used
  IdentityFile ~/.ssh/ignored
Host only-here
  HostName %h.example
"#;
    let loaded = resolve(config, "Prod", home, "local-user").unwrap();
    assert!(loaded.matched);
    assert_eq!(loaded.alias, "Prod");
    assert_eq!(
        (loaded.host.as_str(), loaded.user.as_str(), loaded.port),
        ("repo.example", "release", 2202)
    );
    assert_eq!(
        loaded.identities,
        [
            home.join(".ssh/work # key"),
            home.join(".ssh/Prod-repo.example-2202-release-%")
        ]
    );
    assert_eq!(
        loaded.unsupported,
        [
            Unsupported {
                keyword: "ProxyJump".into(),
                line: 11
            },
            Unsupported {
                keyword: "Include".into(),
                line: 20
            },
            Unsupported {
                keyword: "Match".into(),
                line: 21
            },
        ]
    );

    let excluded = resolve(config, "build-z", home, "local-user").unwrap();
    assert_eq!(
        (
            excluded.host.as_str(),
            excluded.user.as_str(),
            excluded.port
        ),
        ("build-z", "local-default", 22)
    );
    assert!(!excluded
        .unsupported
        .iter()
        .any(|item| item.keyword == "ProxyJump"));
    assert_eq!(
        resolve(config, "build-a", home, "local-user").unwrap().host,
        "repo.example"
    );
    assert_eq!(
        resolve(config, "prod", home, "local-user").unwrap().host,
        "prod"
    );
    assert_eq!(
        resolve(config, "only-here", home, "local-user")
            .unwrap()
            .host,
        "only-here.example"
    );
}

#[test]
fn ssh_config_reports_invalid_active_values_and_keeps_unknown_hosts_distinct() {
    let home = Path::new("/fixture/home");
    let missing = resolve(
        "Include more.conf\nHost other\n  Port 2121",
        "missing",
        home,
        "local-user",
    )
    .unwrap();
    assert!(!missing.matched);
    assert_eq!(
        missing.unsupported,
        [Unsupported {
            keyword: "Include".into(),
            line: 1
        }]
    );
    let literal = resolve("Host one\nIdentityFile none", "one", home, "local-user").unwrap();
    assert!(literal.matched && literal.identities.is_empty());
    assert_eq!(literal.user, "local-user");
    assert_eq!(literal.port, 22);

    for (line, keyword) in [
        ("Port 0", "Port"),
        ("Port 65536", "Port"),
        ("User two users", "User"),
        ("HostName user@server.example", "HostName"),
        ("IdentityFile \"unclosed", "IdentityFile"),
        ("IdentityFile %x", "IdentityFile"),
    ] {
        let error = resolve(&format!("Host one\n{line}\n"), "one", home, "local-user").unwrap_err();
        assert!(error.contains(keyword) && error.contains('2'), "{error}");
    }
}

#[test]
fn ssh_config_loads_the_requested_home_and_bounds_reads() {
    let fixture = tempfile::tempdir().unwrap();
    let dir = fixture.path().join(".ssh");
    std::fs::create_dir(&dir).unwrap();
    let config = dir.join("config");
    std::fs::write(
        &config,
        "Host alias\nHostName localhost\nUser fixture\nIdentityFile ~/.ssh/key\n",
    )
    .unwrap();
    let loaded = load_config(fixture.path(), "alias", "local").unwrap();
    assert_eq!(loaded.user, "fixture");
    assert_eq!(loaded.identities, [dir.join("key")]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let original = std::fs::metadata(&config).unwrap().permissions();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o662)).unwrap();
        assert_eq!(
            load_config(fixture.path(), "alias", "local").unwrap_err(),
            text("Проверьте владельца и права записи ~/.ssh/config")
        );
        std::fs::set_permissions(&config, original).unwrap();
    }
    std::fs::write(&config, vec![b'#'; MAX_CONFIG_BYTES as usize + 1]).unwrap();
    assert!(load_config(fixture.path(), "alias", "local")
        .unwrap_err()
        .contains("1 MiB"));
}
