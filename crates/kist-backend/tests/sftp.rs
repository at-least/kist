//! SFTP 後端的專屬測試：認證方式（密碼／key）、host key 嚴格拒絕、暫存檔殘骸。
//! 需要 `sh tests/sftp-setup.sh` 起的容器與它 export 的環境變數；沒設就跳過。

use kist_backend::sftp::{parse_sftp_url, SftpAuth};

/// 環境變數 → (config URL, known_hosts, password, key 素材)；沒設就 None（跳過）。
type SftpTestEnv = (String, String, String, Option<(String, String)>);

fn sftp_env() -> Option<SftpTestEnv> {
    let url = std::env::var("KIST_TEST_SFTP_URL").ok()?;
    let password = std::env::var("KIST_TEST_SFTP_PASSWORD").ok()?;
    let known_hosts = std::env::var("KIST_TEST_SFTP_KNOWN_HOSTS").ok()?;
    let key = match (
        std::env::var("KIST_TEST_SFTP_KEY").ok(),
        std::env::var("KIST_TEST_SFTP_KEY_PASSPHRASE").ok(),
    ) {
        (Some(k), Some(p)) => Some((k, p)),
        _ => None,
    };
    Some((url, password, known_hosts, key))
}

#[tokio::test]
async fn password_auth_put_get_round_trip() {
    let Some((url, password, known_hosts, _)) = sftp_env() else {
        eprintln!("SFTP password test skipped (tests/sftp-setup.sh 未跑)");
        return;
    };
    let cfg = parse_sftp_url(&url).unwrap();
    let auth = SftpAuth {
        known_hosts: Some(known_hosts.into()),
        key: None,
        password: Some(password),
    };
    let b = kist_backend::Backend::sftp(&cfg, auth).await.unwrap();

    b.put("packs/aa", b"payload".to_vec()).await.unwrap();
    assert_eq!(b.get("packs/aa").await.unwrap(), b"payload");
    // 覆蓋（posix-rename）
    b.put("packs/aa", b"payload-2".to_vec()).await.unwrap();
    assert_eq!(b.get("packs/aa").await.unwrap(), b"payload-2");
    // put_if_absent：已存在要 AlreadyExists，而且**內容不變**（不是「換新」）
    match b.put_if_absent("packs/aa", b"other".to_vec()).await {
        Err(kist_backend::BackendError::AlreadyExists(_)) => {}
        other => panic!("expected AlreadyExists, got {other:?}"),
    }
    assert_eq!(b.get("packs/aa").await.unwrap(), b"payload-2");
    // put_if_absent：不存在的 key 要成功
    b.put_if_absent("packs/bb", b"first".to_vec())
        .await
        .unwrap();
    assert_eq!(b.get("packs/bb").await.unwrap(), b"first");
    b.delete("packs/aa").await.unwrap();
    b.delete("packs/bb").await.unwrap();
    assert!(matches!(
        b.get("packs/aa").await,
        Err(kist_backend::BackendError::NotFound(_))
    ));
}

#[tokio::test]
async fn key_auth_connects_and_round_trips() {
    let Some((url, _, known_hosts, Some((key, passphrase)))) = sftp_env() else {
        eprintln!("SFTP key test skipped (tests/sftp-setup.sh 未跑或缺 key 環境變數)");
        return;
    };
    let cfg = parse_sftp_url(&url).unwrap();
    let auth = SftpAuth {
        known_hosts: Some(known_hosts.into()),
        key: Some((key.into(), Some(passphrase))),
        password: None, // 故意不給密碼：證明 key 認證自己成立
    };
    let b = kist_backend::Backend::sftp(&cfg, auth).await.unwrap();
    b.put("trees/tt", b"key-auth".to_vec()).await.unwrap();
    assert_eq!(b.get("trees/tt").await.unwrap(), b"key-auth");
    b.delete("trees/tt").await.unwrap();
}

#[tokio::test]
async fn unknown_host_key_is_refused() {
    let Some((url, password, known_hosts, _)) = sftp_env() else {
        eprintln!("SFTP host-key refusal test skipped (tests/sftp-setup.sh 未跑)");
        return;
    };
    let cfg = parse_sftp_url(&url).unwrap();
    // known_hosts 放一把「正確格式但不同」的 ed25519 key：演算法協商會過，
    // 但伺服器的真 key 跟記錄不符 → 必須拒連，錯誤要講檔案位置。
    let dir = tempfile::tempdir().unwrap();
    let kh = dir.path().join("known_hosts");
    // 產一把一次性 ed25519 當「錯的記錄」——格式正確、內容跟伺服器真 key 不同
    let out = dir.path().join("wrong");
    let status = std::process::Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-f"])
        .arg(&out)
        .arg("-q")
        .status()
        .expect("ssh-keygen");
    assert!(status.success());
    let wrong_pub = std::fs::read_to_string(out.with_extension("pub")).unwrap();
    // 記在 [127.0.0.1]:port 底下（OpenSSH 對非標準 port 的格式）
    std::fs::write(&kh, format!("[127.0.0.1]:{} {wrong_pub}", cfg.port)).unwrap();
    let _ = known_hosts;
    let auth = SftpAuth {
        known_hosts: Some(kh.clone()),
        key: None,
        password: Some(password),
    };
    let err = match kist_backend::Backend::sftp(&cfg, auth).await {
        Err(kist_backend::BackendError::Sftp(msg)) => msg,
        other => panic!("expected refusal, got {other:?}"),
    };
    assert!(
        err.contains(kh.to_str().unwrap()) && (err.contains("not in") || err.contains("changed")),
        "refusal message should name the known_hosts file: {err}"
    );
}

/// 沒有任何認證素材（也把 agent 排除）時，錯誤要講清楚。
#[tokio::test]
async fn no_auth_material_is_reported_clearly() {
    let Some((url, _, known_hosts, _)) = sftp_env() else {
        eprintln!("SFTP no-auth test skipped (tests/sftp-setup.sh 未跑)");
        return;
    };
    let cfg = parse_sftp_url(&url).unwrap();
    let auth = SftpAuth {
        known_hosts: Some(known_hosts.into()),
        key: None,
        password: None,
    };
    // SSH_AUTH_SOCK 指到不存在的 socket，確保 agent 這條路也不成立。
    // 測試程序共享環境變數：改完要還原。
    let saved = std::env::var("SSH_AUTH_SOCK").ok();
    std::env::set_var("SSH_AUTH_SOCK", "/nonexistent/kist-test-agent.sock");
    let result = kist_backend::Backend::sftp(&cfg, auth).await;
    match saved {
        Some(v) => std::env::set_var("SSH_AUTH_SOCK", v),
        None => std::env::remove_var("SSH_AUTH_SOCK"),
    }
    match result {
        Err(kist_backend::BackendError::Sftp(msg)) => {
            assert!(
                msg.contains("no way to authenticate") || msg.contains("auth"),
                "unexpected error: {msg}"
            );
        }
        other => panic!("expected auth failure, got {other:?}"),
    }
}

/// repo 命名空間的 list 有目錄深度上限：kist 自己的物件最深三層
/// （`snapshots/<client>/<ts>`、`trees/<2hex>/<id>`），上限是數倍寬裕；
/// 超過代表 repo 裡有不該在的東西（或敵意伺服器造鏈）——要乾淨回錯，
/// 不能無限走下去吃記憶體／堆疊。
#[tokio::test]
async fn repo_listing_rejects_absurd_directory_depth() {
    let Some((url, password, known_hosts, _)) = sftp_env() else {
        eprintln!("SFTP depth test skipped (tests/sftp-setup.sh 未跑)");
        return;
    };
    let cfg = parse_sftp_url(&url).unwrap();
    let auth = SftpAuth {
        known_hosts: Some(known_hosts.into()),
        key: None,
        password: Some(password),
    };
    let b = kist_backend::Backend::sftp(&cfg, auth).await.unwrap();

    // 20 層深的目錄鏈＋一個物件。寫入端照常成功（建目錄本來就行）。
    let key = vec!["d"; 20].join("/");
    b.put(&format!("{key}/x"), vec![1u8]).await.unwrap();

    let err = b.list("").await;
    assert!(
        err.is_err(),
        "repo 命名空間 20 層深的 list 必須回錯，卻成功：{:?}",
        err.map(|v| v.len())
    );

    // 對照：正常深度的 prefix 照常可列。
    b.put("packs/normal", vec![2u8]).await.unwrap();
    assert!(b.list("packs").await.is_ok(), "正常深度的 list 不能被連坐");
}
