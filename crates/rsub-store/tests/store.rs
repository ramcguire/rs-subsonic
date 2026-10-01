//! Store tests run against in-memory SQLite and, when `RSUB_TEST_PG_URL` is set
//! (and the `postgres` feature is on), against Postgres too.

use rsub_core::Roles;
use rsub_store::{NewUser, StoreError, UserUpdate, store_tests};

fn new_user(name: &str) -> NewUser<'_> {
    NewUser {
        username: name,
        password_enc: vec![1, 2, 3],
        email: Some("a@example.com"),
        roles: Roles::USER_DEFAULT,
        max_bitrate: 320,
    }
}

store_tests! {
    retired_sources_leave_the_folders(db) {
        let lib = |key: &str| rsub_core::backend::RemoteLibrary {
            key: key.into(),
            name: key.into(),
            locations: vec![],
            change_marker: Some("1".into()),
            scanning: false,
        };
        let kept = db.ensure_source("kept", "plex").await.unwrap();
        let gone = db.ensure_source("gone", "plex").await.unwrap();
        db.sync_libraries(kept, &[lib("1")]).await.unwrap();
        db.sync_libraries(gone, &[lib("1"), lib("2")]).await.unwrap();
        assert_eq!(db.libraries().await.unwrap().len(), 3);
        assert_eq!(db.retire_sources(&[kept]).await.unwrap(), 2);
        let live = db.libraries().await.unwrap();
        assert_eq!(live.iter().map(|l| l.source_id).collect::<Vec<_>>(), [kept]);
        assert_eq!(db.retire_sources(&[]).await.unwrap(), 1);
        assert!(db.libraries().await.unwrap().is_empty());
        // Back in the config: live again, due for a full sync.
        let back = db.sync_libraries(gone, &[lib("1")]).await.unwrap();
        assert_eq!(back.len(), 1);
        assert!(back[0].last_full_sync_at.is_none() && back[0].change_marker.is_none());
    }

    users_roundtrip(db) {
        assert_eq!(db.count_users().await.unwrap(), 0);
        let id = db.create_user(new_user("alice")).await.unwrap();
        db.create_user(new_user("bob")).await.unwrap();
        assert!(matches!(db.create_user(new_user("alice")).await, Err(StoreError::Conflict(_))));

        let alice = db.user_by_username("alice").await.unwrap().unwrap();
        assert_eq!(alice.id, id);
        assert_eq!(alice.password_enc, [1, 2, 3]);
        assert_eq!(alice.roles, Roles::USER_DEFAULT);
        assert_eq!(alice.max_bitrate, 320);
        assert_eq!(alice.id, id);
        assert!(db.user_by_username("carol").await.unwrap().is_none());
        let names: Vec<_> = db.list_users().await.unwrap().into_iter().map(|u| u.username).collect();
        assert_eq!(names, ["alice", "bob"]);
        assert_eq!(db.count_users().await.unwrap(), 2);
    }

    api_keys(db) {
        let id = db.create_user(new_user("alice")).await.unwrap();
        db.create_api_key(id, "phone", &[9; 32]).await.unwrap();
        assert_eq!(db.user_by_api_key_hash(&[9; 32]).await.unwrap().unwrap().id, id);
        assert!(db.user_by_api_key_hash(&[8; 32]).await.unwrap().is_none());
        assert!(matches!(db.create_api_key(id, "dup", &[9; 32]).await, Err(StoreError::Conflict(_))));
    }

    users_change(db) {
        let alice = db.create_user(new_user("alice")).await.unwrap();
        let change = UserUpdate {
            email: Some(None),
            roles: Some(Roles::STREAM),
            max_bitrate: Some(128),
            ..UserUpdate::default()
        };
        assert!(db.update_user(alice, change).await.unwrap());
        let a = db.user_by_username("alice").await.unwrap().unwrap();
        assert_eq!((a.email, a.roles, a.max_bitrate), (None, Roles::STREAM, 128));
        assert_eq!(a.password_enc, [1, 2, 3], "left as it was");
        assert!(!db.update_user(alice + 100, UserUpdate::default()).await.unwrap());

        db.create_api_key(alice, "phone", &[9; 32]).await.unwrap();
        let k2 = db.create_api_key(alice, "laptop", &[8; 32]).await.unwrap();
        let names: Vec<_> = db.api_keys(alice).await.unwrap().into_iter().map(|k| k.name).collect();
        assert_eq!(names, ["phone", "laptop"]);
        assert!(db.delete_api_key(alice, k2).await.unwrap());
        assert!(!db.delete_api_key(alice, k2).await.unwrap());
        assert!(db.user_by_api_key_hash(&[8; 32]).await.unwrap().is_none());

        assert!(db.delete_user(alice).await.is_ok_and(|d| d), "no admins to keep");
        assert!(db.user_by_api_key_hash(&[9; 32]).await.unwrap().is_none(), "keys go with the user");
        assert!(!db.delete_user(alice).await.unwrap());
    }

    last_admin_stays(db) {
        let mut admin = new_user("root");
        admin.roles = Roles::ALL;
        let root = db.create_user(admin).await.unwrap();
        let demote = || UserUpdate { roles: Some(Roles::USER_DEFAULT), ..UserUpdate::default() };
        assert!(matches!(db.update_user(root, demote()).await, Err(StoreError::LastAdmin)));
        assert!(matches!(db.delete_user(root).await, Err(StoreError::LastAdmin)));
        assert!(db.user_by_username("root").await.unwrap().unwrap().is_admin(), "rolled back");
        // Other changes to the last admin are fine.
        let bitrate = UserUpdate { max_bitrate: Some(64), ..UserUpdate::default() };
        assert!(db.update_user(root, bitrate).await.unwrap());

        let mut other = new_user("second");
        other.roles = Roles::ALL;
        db.create_user(other).await.unwrap();
        assert!(db.update_user(root, demote()).await.unwrap());
        let second = db.user_by_username("second").await.unwrap().unwrap();
        assert!(matches!(db.delete_user(second.id).await, Err(StoreError::LastAdmin)));
    }

    usernames_ignore_case(db) {
        let id = db.create_user(new_user("Alice")).await.unwrap();
        assert!(matches!(db.create_user(new_user("aLICE")).await, Err(StoreError::Conflict(_))));
        let found = db.user_by_username("ALICE").await.unwrap().unwrap();
        assert_eq!((found.id, found.username.as_str()), (id, "Alice"), "stored as given");
    }

    ids_are_not_reused(db) {
        let mut admin = new_user("root");
        admin.roles = Roles::ALL;
        db.create_user(admin).await.unwrap();
        let bob = db.create_user(new_user("bob")).await.unwrap();
        assert!(db.delete_user(bob).await.unwrap());
        assert!(db.create_user(new_user("carol")).await.unwrap() > bob);
    }

    nul_is_dropped(db) {
        db.set_setting("k", "a\0b").await.unwrap();
        assert_eq!(db.get_setting("k").await.unwrap().as_deref(), Some("ab"));
    }

    settings(db) {
        assert_eq!(db.get_setting("k").await.unwrap(), None);
        db.set_setting("k", "v1").await.unwrap();
        db.set_setting("k", "v2").await.unwrap();
        assert_eq!(db.get_setting("k").await.unwrap().as_deref(), Some("v2"));
    }
}

/// Both migration sets must produce the same tables, columns and nullability.
#[cfg(feature = "postgres")]
#[tokio::test]
async fn schemas_match() {
    use std::collections::BTreeSet;

    let Some(pg) = rsub_store::testing::postgres().await else {
        return;
    };
    let lite = rsub_store::testing::sqlite().await;

    let (rsub_store::Db::Sqlite { read, .. }, rsub_store::Db::Postgres(pool)) = (&lite, &pg) else {
        unreachable!()
    };

    let lite_cols: BTreeSet<(String, String, bool)> = sqlx::query_as::<_, (String, String, bool)>(
        "SELECT m.name, p.name, p.\"notnull\" OR p.pk FROM sqlite_master m, pragma_table_info(m.name) p \
         WHERE m.type = 'table' AND m.name NOT LIKE 'sqlite_%' AND m.name NOT LIKE '_sqlx%'",
    )
    .fetch_all(read)
    .await
    .unwrap()
    .into_iter()
    .collect();

    let pg_cols: BTreeSet<(String, String, bool)> = sqlx::query_as::<_, (String, String, bool)>(
        "SELECT table_name::text, column_name::text, is_nullable = 'NO' FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name NOT LIKE '_sqlx%'",
    )
    .fetch_all(pool)
    .await
    .unwrap()
    .into_iter()
    .collect();

    assert_eq!(lite_cols, pg_cols);
}
