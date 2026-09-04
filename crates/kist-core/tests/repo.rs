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

/// config 是明文；改動 chunker 參數後必須解不開（綁進 master key 的 AAD），而不是悄悄讓去重失效。
#[tokio::test]
async fn tampered_chunker_params_in_config_are_detected() {
    let t = TestRepo::new().await;
    let cfg_path = t.repo_path().join("config");
    let mut cfg: kist_format::config::RepoConfig =
        kist_format::cbor::decode(&std::fs::read(&cfg_path).unwrap()).unwrap();
    cfg.chunker.avg *= 2;
    std::fs::write(&cfg_path, kist_format::cbor::encode(&cfg).unwrap()).unwrap();
    let err = Repository::open(t.backend.clone(), PASSWORD.as_bytes())
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Crypto(_)), "{err}");
}

/// 荒謬的 chunker 參數：回錯誤，不能 panic（fastcdc 在 release 沒有檢查）。
#[tokio::test]
async fn invalid_chunker_params_in_config_are_rejected() {
    let t = TestRepo::new().await;
    let cfg_path = t.repo_path().join("config");
    let mut cfg: kist_format::config::RepoConfig =
        kist_format::cbor::decode(&std::fs::read(&cfg_path).unwrap()).unwrap();
    cfg.chunker = kist_format::config::ChunkerParams {
        min: 0,
        avg: 0,
        max: 0,
    };
    std::fs::write(&cfg_path, kist_format::cbor::encode(&cfg).unwrap()).unwrap();
    let err = Repository::open(t.backend.clone(), PASSWORD.as_bytes())
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::InvalidConfig(_)), "{err}");
}

#[tokio::test]
async fn init_rejects_invalid_options() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Backend::local(dir.path()).unwrap();
    let mut opts = init_options();
    opts.chunker.min = opts.chunker.max + 1;
    let err = Repository::init(backend.clone(), b"x", opts)
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::InvalidConfig(_)), "{err}");
    let mut opts = init_options();
    opts.pack_target_size = 10; // 比一個 chunk 還小
    assert!(matches!(
        Repository::init(backend, b"x", opts).await,
        Err(CoreError::InvalidConfig(_))
    ));
}
