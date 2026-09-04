//! init / open 的行為。

mod common;

use common::*;
use kist_backend::Backend;
use kist_core::{CoreError, Repository};

#[tokio::test]
async fn init_writes_config_and_open_needs_correct_password() {
    let t = TestRepo::new().await;
    assert!(t.repo_path().join("config").is_file());

    let repo = t.open().await;
    assert_eq!(repo.config().chunker, init_options().chunker);
    assert_eq!(repo.config().pack_target_size, 256 * 1024);

    let wrong = Repository::open(t.backend.clone(), b"nope").await;
    assert!(matches!(
        wrong,
        Err(CoreError::Crypto(kist_crypto::CryptoError::WrongPassword))
    ));
}

#[tokio::test]
async fn init_refuses_existing_repo() {
    let t = TestRepo::new().await;
    let again = Repository::init(t.backend.clone(), b"other", init_options()).await;
    assert!(matches!(again, Err(CoreError::RepoExists)));
    // 原本的 config 沒被動
    assert!(t.open().await.config().version == 1);
}

#[tokio::test]
async fn open_empty_dir_is_not_a_repo() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Backend::local(dir.path()).unwrap();
    assert!(matches!(
        Repository::open(backend, b"x").await,
        Err(CoreError::NotARepository)
    ));
}
