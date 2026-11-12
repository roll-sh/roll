//! Accounts and IAM entities in the metadata: the default account, accounts' buckets, keys and
//! roles, IAM documents and MFA steps, all rebuilt from object storage alone.

mod common;

use common::*;
use s3_store::iam::{self, MfaDevice, User};
use s3_store::sts::IamRoleSpec;
use s3_store::{AccountUpdate, BucketOptions, Permission, StoreError};

#[tokio::test]
async fn accounts_survive_recovery() {
    let (backend, clock, s) = env("store-accounts").await;
    let default = s.default_account_id();
    assert_eq!(default, s.account_id(), "the deployment's account id is the default account's");
    let d = s.require_account(&default).unwrap();
    assert_eq!(d.canonical, s.owner_id());
    assert_eq!(s.accounts().unwrap().len(), 1);

    let a = s.create_account("alt", Some("Alt@Example.com")).await.unwrap();
    assert_eq!((a.id.len(), a.canonical.len(), a.email.as_deref()), (12, 64, Some("alt@example.com")));
    assert!(matches!(s.create_account("dup", Some("alt@example.com")).await, Err(StoreError::Conflict(_))), "emails are unique");
    assert!(matches!(s.create_account("", None).await, Err(StoreError::InvalidArgument(_))));
    assert_eq!(s.account_by_email("ALT@example.com").unwrap().unwrap().id, a.id);
    assert_eq!(s.account_by_canonical(&a.canonical).unwrap().unwrap().id, a.id);

    // Buckets, keys and roles belong to accounts; names stay global.
    s.create_bucket("mine", BucketOptions::default()).await.unwrap();
    s.create_bucket_in(&a.id, "theirs", BucketOptions::default()).await.unwrap();
    assert!(matches!(s.create_bucket_in(&a.id, "mine", BucketOptions::default()).await, Err(StoreError::BucketExists(_))));
    assert!(matches!(s.create_bucket_in("999999999999", "x-y-z", BucketOptions::default()).await, Err(StoreError::NotFound(_))));
    s.branch_bucket("theirs", "theirs-branch").await.unwrap();
    assert_eq!(s.bucket("theirs-branch").unwrap().account_id, a.id, "a branch is its parent's account's");
    let root = s.create_key_in(&a.id, "root", Permission::Admin, None).await.unwrap();
    assert!(root.is_root());
    let scoped = s.create_key_in(&a.id, "scoped", Permission::Admin, Some(vec!["theirs".into()])).await.unwrap();
    assert!(!scoped.is_root());
    s.create_iam_role(&a.id, "ci", IamRoleSpec { path: "/".into(), trust_policy: "{}".into(), ..Default::default() }).await.unwrap();
    assert!(matches!(s.create_iam_role(&a.id, "CI", IamRoleSpec { trust_policy: "{}".into(), ..Default::default() }).await, Err(StoreError::Conflict(_))), "role names are case-insensitive");
    s.create_role("ci", s3_store::sts::RoleSpec { description: String::new(), permission: Permission::Read, buckets: None, policy: None, trust: Default::default(), max_session_s: None }).await.unwrap();
    assert_eq!(s.role_in(&a.id, "ci").unwrap().unwrap().permission, None, "the same name in another account is another role");

    // IAM documents and a user's key, in one transaction.
    let key = s
        .iam_write(|t| {
            let u = User { name: "Alice".into(), id: iam::entity_id("AIDA"), path: "/".into(), created_at: t.now(), ..Default::default() };
            t.put(&a.id, iam::kind::USER, "Alice", &u)?;
            t.put(&a.id, iam::kind::MFA, "alice", &MfaDevice { name: "alice".into(), serial: format!("arn:aws:iam::{}:mfa/alice", a.id), seed: "JBSWY3DPEHPK3PXP".into(), user: Some("Alice".into()), enabled_at: Some(1), last_step: 10, ..Default::default() })?;
            t.create_user_key(&a.id, "Alice")
        })
        .await
        .unwrap();
    assert_eq!((key.user.as_deref(), key.buckets.as_deref()), (Some("Alice"), Some(&[][..])), "a user's key has no permission level");
    // A failing transaction changes nothing.
    let r: Result<(), StoreError> = s
        .iam_write(|t| {
            t.put(&a.id, iam::kind::GROUP, "half", &serde_json::json!({"name": "half"}))?;
            Err(StoreError::LimitExceeded("no".into()))
        })
        .await;
    assert!(matches!(r, Err(StoreError::LimitExceeded(_))));
    // MFA steps are used once.
    let serial = format!("arn:aws:iam::{}:mfa/alice", a.id);
    assert!(!s.use_mfa_step(&a.id, &serial, 10).await.unwrap(), "not newer than the last step");
    assert!(s.use_mfa_step(&a.id, &serial, 11).await.unwrap());
    assert!(!s.use_mfa_step(&a.id, &serial, 11).await.unwrap(), "no replay");
    assert!(!s.use_mfa_step(&default, &serial, 12).await.unwrap(), "another account's device");
    s.set_key_active(&key.id, false).await.unwrap();
    s.update_account(&a.id, AccountUpdate { name: Some("Alt Inc".into()), email: Some(None) }).await.unwrap();

    // Everything is rebuilt from object storage alone.
    s.close().await;
    let s2 = open(&backend, &clock).await;
    assert_eq!(s2.default_account_id(), default);
    let a2 = s2.require_account(&a.id).unwrap();
    assert_eq!((a2.canonical.as_str(), a2.name.as_str(), a2.email), (a.canonical.as_str(), "Alt Inc", None));
    assert_eq!(s2.buckets_of(&a.id).unwrap().iter().map(|b| b.name.clone()).collect::<Vec<_>>(), vec!["theirs", "theirs-branch"]);
    assert_eq!(s2.buckets_of(&default).unwrap().iter().map(|b| b.name.clone()).collect::<Vec<_>>(), vec!["mine"]);
    let keys = s2.keys_of(&a.id).unwrap();
    assert_eq!(keys.len(), 3);
    assert!(!keys.iter().find(|k| k.id == key.id).unwrap().active);
    assert!(s2.iam_user(&a.id, "alice").unwrap().is_some(), "names are case-insensitive");
    assert!(s2.iam_group(&a.id, "half").unwrap().is_none());
    assert_eq!(s2.mfa_device(&a.id, &serial).unwrap().unwrap().last_step, 11);
    assert_eq!(s2.roles_in(&a.id).unwrap().len(), 1);
    assert_eq!(s2.roles().unwrap().len(), 1);

    // An account goes only without buckets, and takes its keys, roles and IAM entities along.
    assert!(matches!(s2.delete_account(&a.id).await, Err(StoreError::DeleteConflict(_))));
    s2.delete_bucket("theirs-branch", true).await.unwrap();
    s2.delete_bucket("theirs", true).await.unwrap();
    s2.delete_account(&a.id).await.unwrap();
    assert!(s2.account(&a.id).unwrap().is_none());
    assert!(s2.keys_of(&a.id).unwrap().is_empty() && s2.roles_in(&a.id).unwrap().is_empty());
    assert!(s2.iam_user(&a.id, "alice").unwrap().is_none());
    assert!(matches!(s2.delete_account(&default).await, Err(StoreError::InvalidRequest(_))), "the default account stays");
}
