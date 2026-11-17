// SPDX-License-Identifier: AGPL-3.0-only

//! ADR 0005 S1 spike：一次性链接（QR 注入 `backup5` capability）→ 捕获信封
//! `ephemeral_backup_key` → 尝试长轮询下载 transfer_archive。
//!
//! 取证纪律：不打印任何密钥值（只记存在性/长度），不保存注册数据（链接用
//! 的 store 落在临时目录，仅证明流程可用）。证据仅三点：信封是否携带 key、
//! transfer_archive 是否可取、档案字节数。跑完后请在手机端删除该关联设备。

// M6 P0 编译期能力门禁（与 `src/main.rs` 同一条门，设计：
// docs/plan/engine-store-encryption.md §7 P0）。bin target 各自独立编译，
// `main.rs` 顶部的 `compile_error!` 覆盖不到这里；本 bin 同样会
// `SqliteStore::open`（临时目录、无 key），漏开 feature 时磁盘上就是明文库，
// 所以门要各写一份：`cargo build --no-default-features --bin backup5-spike`
// 必须直接编译失败（exit 101）。
#[cfg(not(feature = "sqlcipher"))]
compile_error!(
    "engine requires the sqlcipher feature: PRAGMA key is a silent no-op on plain SQLite (backup5-spike bin)"
);

/// M6 P0 运行时能力门（评审第 3 条：spike 打开库的路径必须经**同一条**门）。
///
/// 本 crate 是独立 bin（无 `lib.rs`），共享实现的唯一姿势是 `#[path]` 同源
/// 引入——`src/store_cipher.rs` 不引用 `crate::` 下的模块，可以被两个 bin
/// 各编一份。`allow(dead_code)`：主 bin 才用得到的启动证据/断言辅助面在本
/// crate 里用不上。
#[allow(dead_code)]
#[path = "../store_cipher.rs"]
mod store_cipher;

use std::time::Duration;

use futures::channel::mpsc;
use futures::{future, StreamExt};
use presage::libsignal_service::configuration::{Endpoint, ServiceCredentials, SignalServers};
use presage::libsignal_service::prelude::PushService;
use presage::libsignal_service::provisioning::{
    link_device, NewDeviceRegistration, SecondaryDeviceProvisioning,
};
use presage::libsignal_service::push_service::HttpAuthOverride;
use presage::model::identity::OnNewIdentity;
use presage::store::Store;
use presage_store_sqlite::SqliteStore;
use rand::{
    distr::{Alphanumeric, SampleString},
    rng, RngCore,
};
use reqwest::Method;

const USER_AGENT: &str = "kt-signal-engine backup5-spike";
const DEVICE_NAME: &str = "KT backup5 spike";

#[tokio::main]
async fn main() {
    // 能力门在**任何磁盘动作之前**（评审第 3 条）：缺 SQLCipher 能力时连临时
    // 目录都不建、也不进 `SqliteStore::open`（它会 create_if_missing 建明文
    // 库）。失败证据行由门的 `report_denial_once` 落一条，这里不再重复打印，
    // 只把退出码交代清楚。
    if store_cipher::ensure_cipher_capable().await.is_err() {
        std::process::exit(1);
    }
    let spike_dir = std::env::temp_dir().join("kt-backup5-spike-store");
    let _ = std::fs::remove_dir_all(&spike_dir);
    std::fs::create_dir_all(&spike_dir).expect("create temp dir");
    let store_path = spike_dir.join("store.db");
    let store_path = store_path.to_str().expect("utf8 temp path");

    let store = SqliteStore::open(store_path, OnNewIdentity::Trust)
        .await
        .expect("open spike store");

    let mut rng = rng();
    let password = Alphanumeric.sample_string(&mut rng, 24);
    let mut signaling_key = [0u8; 52];
    rng.fill_bytes(&mut signaling_key);

    let push_service = PushService::new(SignalServers::Production, None, USER_AGENT);
    let (tx, mut rx) = mpsc::channel(1);

    let (wait_for_scan, registration) = future::join(
        link_device(
            &mut store.aci_protocol_store(),
            &mut store.pni_protocol_store(),
            &mut rng,
            push_service,
            &password,
            DEVICE_NAME,
            tx,
        ),
        async move {
            let Some(SecondaryDeviceProvisioning::Url(url)) = rx.next().await else {
                return Err("no provisioning URL arrived".to_owned());
            };

            let mut qr_url = url.clone();
            qr_url
                .query_pairs_mut()
                .append_pair("capabilities", "backup5");
            println!("provisioning URL (plain):  {url}");
            println!("provisioning URL (spike):  {qr_url}");
            render_qr(qr_url.as_str());
            println!("waiting for phone to scan ...");

            match rx.next().await {
                Some(SecondaryDeviceProvisioning::NewDeviceRegistration(data)) => Ok(data),
                other => Err(format!("unexpected provisioning event: {other:?}")),
            }
        },
    )
    .await;

    if let Err(error) = wait_for_scan {
        eprintln!("spike: link_device failed: {error}");
        return;
    }
    let data = match registration {
        Ok(data) => data,
        Err(error) => {
            eprintln!("spike: {error}");
            return;
        }
    };

    let NewDeviceRegistration {
        phone_number,
        service_ids,
        device_id,
        ephemeral_backup_key,
        ..
    } = data;

    println!(
        "linked: phone={phone_number} aci={} device_id={device_id}",
        service_ids.aci
    );
    match ephemeral_backup_key {
        Some(key) => println!("S1 GATE: ephemeral_backup_key PRESENT (len={})", key.len()),
        None => {
            println!("S1 GATE: ephemeral_backup_key ABSENT — 一票否决点命中，真机不配合 backup5");
            println!("spike 结束。请在手机 Signal → 已关联设备 中删除 “{DEVICE_NAME}”。");
            return;
        }
    }

    // 手机正在上传档案：transfer_archive 长轮询 200={cdn,key} / 204=未就绪。
    let push = PushService::new(
        SignalServers::Production,
        Some(ServiceCredentials {
            aci: Some(service_ids.aci),
            pni: Some(service_ids.pni),
            phonenumber: phone_number,
            password: Some(password),
            device_id: Some(device_id),
        }),
        USER_AGENT,
    );

    let manifest = {
        let mut polls = 0u32;
        loop {
            let resp = match send(
                &push,
                Method::GET,
                Endpoint::service("v1/devices/transfer_archive?timeout=60"),
            )
            .await
            {
                Ok(resp) => resp,
                Err(error) => {
                    eprintln!("spike: transfer_archive request failed: {error}");
                    return;
                }
            };
            let status = resp.status();
            match status.as_u16() {
                200 => {
                    break match resp.bytes().await {
                        Ok(bytes) => bytes,
                        Err(error) => {
                            eprintln!("spike: manifest body read failed: {error}");
                            return;
                        }
                    }
                }
                204 => {
                    polls += 1;
                    if polls >= 10 {
                        eprintln!(
                            "spike: manifest not ready after {polls} long-polls — 手机侧未完成上传或已拒绝"
                        );
                        return;
                    }
                    println!("manifest not ready (204), poll #{polls} ...");
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
                _ => {
                    let body = resp.text().await.unwrap_or_default();
                    println!(
                        "transfer_archive returned {}: {}",
                        status,
                        truncate(&body, 300)
                    );
                    return;
                }
            }
        }
    };

    let manifest: serde_json::Value = match serde_json::from_slice(&manifest) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("spike: manifest json parse failed: {error}");
            return;
        }
    };
    let Some(cdn) = manifest["cdn"].as_u64().map(|c| c as u32) else {
        eprintln!("spike: manifest missing cdn: {manifest}");
        return;
    };
    let Some(key) = manifest["key"].as_str() else {
        eprintln!("spike: manifest missing key: {manifest}");
        return;
    };

    let archive = match send(
        &push,
        Method::GET,
        Endpoint::cdn(cdn, format!("attachments/{key}")),
    )
    .await
    {
        Ok(resp) => resp,
        Err(error) => {
            eprintln!("spike: archive download request failed: {error}");
            return;
        }
    };
    let status = archive.status();
    let bytes = match archive.bytes().await {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("spike: archive body read failed: {error}");
            return;
        }
    };

    let out = std::env::temp_dir().join("kt-backup5-spike-archive.bin");
    if let Err(error) = std::fs::write(&out, &bytes) {
        eprintln!("spike: archive write failed: {error}");
        return;
    }
    println!(
        "S1 RESULT: archive downloaded from cdn{cdn}: status={status} bytes={} file={}",
        bytes.len(),
        out.display()
    );
    println!("spike 结束。请在手机 Signal → 已关联设备 中删除 “{DEVICE_NAME}”。");
}

fn render_qr(url: &str) {
    let ok = std::process::Command::new("npx")
        .args(["--yes", "qrcode-terminal", url])
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !ok {
        println!("（QR 渲染失败，请用上面 spike URL 自行生成二维码）");
    }
}

async fn send(
    push: &PushService,
    method: Method,
    endpoint: Endpoint<'_>,
) -> Result<reqwest::Response, String> {
    let builder = push
        .request(method, endpoint, HttpAuthOverride::NoOverride)
        .map_err(|error| error.to_string())?;
    builder.send().await.map_err(|error| error.to_string())
}

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}
