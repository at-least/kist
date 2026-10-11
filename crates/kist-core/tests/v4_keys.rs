//! v4 的金鑰槽（format-v4.md §3）：`key add` 建新槽、`key remove` 只在開啟用的槽仍在列表裡、而且不是要刪的
//! 那一個時才刪，最後一個槽刪不掉。

mod v4_common;

use kist_core::v4::Repo;
use kist_core::CoreError;
use kist_crypto::CryptoError;
use v4_common::*;

const NEW: &[u8] = b"new password";

async fn open_with(t: &TestRepo, password: &[u8]) -> kist_core::Result<Repo> {
    Repo::open(t.backend.clone(), password, None).await
}

#[tokio::test]
async fn a_second_password_opens_the_same_repository() {
    let t = TestRepo::new().await;
    let repo = t.open_uncached().await;
    let name = repo.key_add(NEW).await.unwrap();
    let other = open_with(&t, NEW).await.unwrap();
    assert_eq!(other.config(), repo.config());
    assert_eq!(
        other.keys().cache_dir_name("x"),
        repo.keys().cache_dir_name("x"),
        "the same master key"
    );
    let slots = other.key_list().await.unwrap();
    assert_eq!(slots.len(), 2);
    assert!(slots.iter().any(|s| s.name == name && s.current));
    assert!(slots.iter().any(|s| s.name != name && !s.current));
}

#[tokio::test]
async fn a_password_that_already_has_a_slot_is_refused() {
    let t = TestRepo::new().await;
    let repo = t.open_uncached().await;
    let err = repo.key_add(PASSWORD).await.unwrap_err();
    assert!(matches!(err, CoreError::Usage(_)), "{err}");
    repo.key_add(NEW).await.unwrap();
    assert!(repo.key_add(NEW).await.is_err());
    assert_eq!(repo.key_list().await.unwrap().len(), 2);
}

/// 換密碼：加新的、以新的開啟、刪掉舊的；舊密碼從此開不了。
#[tokio::test]
async fn the_old_slot_is_removed_from_a_repository_opened_with_the_new_password() {
    let t = TestRepo::new().await;
    let repo = t.open_uncached().await;
    repo.key_add(NEW).await.unwrap();
    let old = repo
        .key_list()
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.current)
        .unwrap()
        .name;
    let other = open_with(&t, NEW).await.unwrap();
    other.key_remove(&old).await.unwrap();
    assert!(matches!(
        open_with(&t, PASSWORD).await.unwrap_err(),
        CoreError::Crypto(CryptoError::WrongPassword)
    ));
    open_with(&t, NEW).await.unwrap();
    assert_eq!(other.key_list().await.unwrap().len(), 1);
}

#[tokio::test]
async fn the_slot_this_command_opened_with_is_not_removed() {
    let t = TestRepo::new().await;
    let repo = t.open_uncached().await;
    repo.key_add(NEW).await.unwrap();
    let current = repo
        .key_list()
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.current)
        .unwrap()
        .name;
    let err = repo.key_remove(&current).await.unwrap_err();
    assert!(matches!(err, CoreError::Usage(_)), "{err}");
    assert_eq!(repo.key_list().await.unwrap().len(), 2);
}

#[tokio::test]
async fn an_unknown_or_malformed_slot_is_refused() {
    let t = TestRepo::new().await;
    let repo = t.open_uncached().await;
    for name in [
        "00112233445566778899aabbccddeeff",
        "nothex",
        "ABCDEF00112233445566778899aabbcc",
    ] {
        let err = repo.key_remove(name).await.unwrap_err();
        assert!(matches!(err, CoreError::Usage(_)), "{name}: {err}");
    }
}

/// 開啟用的槽已經不在列表裡（被別處刪了）→ 不刪別的：否則可能一個槽都不剩。
#[tokio::test]
async fn nothing_is_removed_when_the_opening_slot_is_gone() {
    let t = TestRepo::new().await;
    let repo = t.open_uncached().await;
    let new = repo.key_add(NEW).await.unwrap();
    let current = repo
        .key_list()
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.current)
        .unwrap()
        .name;
    t.backend.delete(&format!("keys/{current}")).await.unwrap();
    let err = repo.key_remove(&new).await.unwrap_err();
    assert!(
        matches!(&err, CoreError::Usage(m) if m.contains("no longer listed")),
        "{err}"
    );
    open_with(&t, NEW).await.unwrap();
}
