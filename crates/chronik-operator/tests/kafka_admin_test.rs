//! KafkaAdmin against a real broker: every call the topic and user
//! controllers make, checked by reading the result back.
//!
//! Ignored by default; `tests/cluster/e2e/operator_kafka_admin.sh` starts a
//! broker (with and without SASL) and runs it:
//!
//!   CHRONIK_ADMIN_TEST_BOOTSTRAP=localhost:9092 \
//!   [CHRONIK_ADMIN_TEST_SECRET=<CHRONIK_INTER_BROKER_SECRET>] \
//!   [CHRONIK_ADMIN_TEST_TLS_DIR=<dir with ca.crt, tls.crt, tls.key>] \
//!     cargo test -p chronik-operator --test kafka_admin_test -- --ignored
//!
//! CHRONIK_ADMIN_TEST_ACLS=1 when the broker enforces ACLs; otherwise the test
//! expects SECURITY_DISABLED handling.
//!
//! With a secret, the broker must require SASL: the test logs in as the broker
//! user, and checks a created user can log in and a deleted one cannot.

use std::collections::BTreeMap;

use chronik_operator::kafka_admin::{AclEntry, Credentials, KafkaAccess, KafkaAdmin, KafkaTls};
use chronik_scram::SaslMechanism;

fn access(credentials: Option<Credentials>) -> Option<KafkaAccess> {
    let bootstrap = std::env::var("CHRONIK_ADMIN_TEST_BOOTSTRAP").ok()?;
    // An unreachable first address: the client must move on to the next.
    Some(KafkaAccess {
        bootstrap: vec!["127.0.0.1:1".into(), bootstrap],
        credentials,
        tls: tls(),
    })
}

/// With CHRONIK_ADMIN_TEST_TLS_DIR (ca.crt, tls.crt, tls.key), over TLS.
fn tls() -> Option<KafkaTls> {
    let dir = std::path::PathBuf::from(std::env::var("CHRONIK_ADMIN_TEST_TLS_DIR").ok()?);
    let read = |name: &str| std::fs::read(dir.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
    Some(KafkaTls {
        ca_pem: read("ca.crt"),
        cert_pem: read("tls.crt"),
        key_pem: read("tls.key"),
    })
}

fn broker_login() -> Option<Credentials> {
    std::env::var("CHRONIK_ADMIN_TEST_SECRET")
        .ok()
        .map(|password| Credentials {
            username: "__chronik_broker".into(),
            password,
        })
}

fn acl(resource_type: &str, name: &str, pattern: &str, op: &str, effect: &str) -> AclEntry {
    AclEntry {
        resource_type: resource_type.into(),
        resource_name: name.into(),
        pattern_type: pattern.into(),
        operation: op.into(),
        effect: effect.into(),
    }
}

#[tokio::test]
#[ignore = "needs a broker; set CHRONIK_ADMIN_TEST_BOOTSTRAP"]
async fn topics_users_and_acls_round_trip() {
    let Some(admin_access) = access(broker_login()) else {
        eprintln!("SKIP: CHRONIK_ADMIN_TEST_BOOTSTRAP not set");
        return;
    };
    let sasl = admin_access.credentials.is_some();
    let rf: i32 = std::env::var("CHRONIK_ADMIN_TEST_RF")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let mut admin = KafkaAdmin::connect(&admin_access)
        .await
        .expect("connect (and log in)");
    let tag = uuid::Uuid::new_v4().simple().to_string();
    let topic = format!("opadmin-{}", &tag[..12]);

    // Describing an unknown topic says so, and does not auto-create it.
    assert_eq!(admin.describe_topic(&topic).await.unwrap(), None);
    assert_eq!(
        admin.describe_topic(&topic).await.unwrap(),
        None,
        "describe auto-created the topic"
    );

    // Create with a config; creating again is not an error.
    let config = BTreeMap::from([("retention.ms".to_string(), "12345678".to_string())]);
    admin
        .create_topic(&topic, 3, rf, &config)
        .await
        .expect("create");
    admin
        .create_topic(&topic, 3, rf, &config)
        .await
        .expect("create again is not an error");
    let d = admin
        .describe_topic(&topic)
        .await
        .unwrap()
        .expect("created topic is described");
    assert_eq!((d.partitions, d.replication_factor), (3, rf));
    assert_eq!(
        d.config.get("retention.ms").map(String::as_str),
        Some("12345678"),
        "configs: {:?}",
        d.config
    );

    // Alter a config, add partitions.
    let changed = BTreeMap::from([("retention.ms".to_string(), "87654321".to_string())]);
    admin
        .update_topic_config(&topic, &changed)
        .await
        .expect("alter config");
    admin
        .increase_partitions(&topic, 5)
        .await
        .expect("add partitions");
    let d = admin.describe_topic(&topic).await.unwrap().unwrap();
    assert_eq!(d.partitions, 5);
    assert_eq!(
        d.config.get("retention.ms").map(String::as_str),
        Some("87654321")
    );

    // A SCRAM user: created, can log in (when the broker checks), replaced,
    // then deleted and refused.
    let user = format!("opuser-{}", &tag[..12]);
    let password = format!("pw-{tag}");
    admin
        .upsert_scram_user(&user, &password, SaslMechanism::ScramSha512)
        .await
        .expect("create user");
    if sasl {
        let login = access(Some(Credentials {
            username: user.clone(),
            password: password.clone(),
        }))
        .unwrap();
        KafkaAdmin::connect(&login)
            .await
            .expect("the created user logs in");
        let wrong = access(Some(Credentials {
            username: user.clone(),
            password: "wrong".into(),
        }))
        .unwrap();
        assert!(
            KafkaAdmin::connect(&wrong).await.is_err(),
            "a wrong password was accepted"
        );
        let rotated = format!("rotated-{tag}");
        admin
            .upsert_scram_user(&user, &rotated, SaslMechanism::ScramSha512)
            .await
            .expect("replace credential");
        assert!(
            KafkaAdmin::connect(&login).await.is_err(),
            "the old password still works"
        );
        let login = access(Some(Credentials {
            username: user.clone(),
            password: rotated,
        }))
        .unwrap();
        KafkaAdmin::connect(&login)
            .await
            .expect("the new password logs in");
    }

    // ACLs: exactly what is asked, idempotent, then narrowed, then cleared.
    let wanted = vec![
        acl("topic", &topic, "literal", "Read", "Allow"),
        acl("topic", "app-", "prefixed", "Write", "Allow"),
        acl("group", "g1", "literal", "Read", "Allow"),
    ];
    if std::env::var("CHRONIK_ADMIN_TEST_ACLS").as_deref() == Ok("1") {
        assert_eq!(
            admin.set_user_acls(&user, &wanted).await.expect("set ACLs"),
            3
        );
        assert_eq!(
            admin
                .set_user_acls(&user, &wanted)
                .await
                .expect("set ACLs again"),
            3
        );
        let mut have = admin
            .user_acls(&user)
            .await
            .unwrap()
            .expect("ACLs are enabled");
        let mut want = wanted.clone();
        have.sort();
        want.sort();
        assert_eq!(have, want, "the broker holds a different set");

        let narrowed = vec![acl("topic", &topic, "literal", "Write", "Deny")];
        assert_eq!(admin.set_user_acls(&user, &narrowed).await.unwrap(), 1);
        assert_eq!(admin.user_acls(&user).await.unwrap(), Some(narrowed));
        admin.set_user_acls(&user, &[]).await.unwrap();
        assert_eq!(admin.user_acls(&user).await.unwrap(), Some(vec![]));
    } else {
        // ACLs off: the broker says so, and asking for ACLs is an error the
        // user sees rather than a silent no-op; asking for none is fine.
        assert_eq!(admin.user_acls(&user).await.unwrap(), None);
        let e = admin
            .set_user_acls(&user, &wanted)
            .await
            .expect_err("ACLs requested on a cluster without them");
        assert!(e.to_string().contains("ACLs disabled"), "{e}");
        assert_eq!(admin.set_user_acls(&user, &[]).await.unwrap(), 0);
    }

    admin.delete_scram_user(&user).await.expect("delete user");
    admin
        .delete_scram_user(&user)
        .await
        .expect("deleting a missing user is not an error");
    if sasl {
        let login = access(Some(Credentials {
            username: user.clone(),
            password: password.clone(),
        }))
        .unwrap();
        assert!(
            KafkaAdmin::connect(&login).await.is_err(),
            "a deleted user still logs in"
        );
    }

    // Delete the topic; deleting it again is not an error.
    admin.delete_topic(&topic).await.expect("delete topic");
    let mut gone = false;
    for _ in 0..20 {
        if admin.describe_topic(&topic).await.unwrap().is_none() {
            gone = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    assert!(gone, "deleted topic still described");
    admin
        .delete_topic(&topic)
        .await
        .expect("deleting a missing topic is not an error");
}

#[tokio::test]
#[ignore = "needs a broker that requires SASL; set CHRONIK_ADMIN_TEST_BOOTSTRAP and CHRONIK_ADMIN_TEST_SECRET"]
async fn a_wrong_broker_secret_is_refused() {
    let Some(_) = broker_login() else {
        eprintln!("SKIP: CHRONIK_ADMIN_TEST_SECRET not set");
        return;
    };
    let wrong = access(Some(Credentials {
        username: "__chronik_broker".into(),
        password: "not-the-secret-0000".into(),
    }))
    .unwrap();
    assert!(KafkaAdmin::connect(&wrong).await.is_err());
}
