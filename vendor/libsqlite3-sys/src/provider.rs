// SPDX-License-Identifier: AGPL-3.0-only

//! SQLCipher 自定义 crypto provider（KT 定制，2026-11-19）。
//!
//! # 为什么需要它
//!
//! Windows/MSVC 下 `libsqlite3-sys` 的 `bundled-sqlcipher-vendored-openssl` 会把
//! OpenSSL 静态链进二进制，而 libsignal 的 `libsignal-net → attest` 也静态链入
//! BoringSSL。两者导出同名符号 1614 个（`EVP_*` / `OPENSSL_sk_*` / `ASN1_*`），
//! MSVC 直接以 LNK2005 拒绝链接。macOS 天然绕过（走 CommonCrypto，不引 OpenSSL），
//! Windows 没有对应物。
//!
//! # 做法（与官方 Signal 同构）
//!
//! SQLCipher 的 provider 通过 `SQLCIPHER_CRYPTO_CUSTOM=<函数名>` 指定，
//! 只要求调用方提供一个 `int setup(sqlcipher_provider *)`。官方 node-sqlcipher
//! 正是用这个口子接自己的实现（其 extension 也是 Rust 项目，依赖 RustCrypto）。
//!
//! 这里用**纯 Rust** 实现全部 17 个回调（RustCrypto: aes/cbc/hmac/pbkdf2），
//! 不链接任何 C crypto 库 —— 纯 Rust 不产生 C 符号，冲突从根上消失。
//!
//! # 契约来源
//!
//! 常量与结构体字段顺序取自本仓 vendored 的 SQLCipher 源码
//! （`vendor/libsqlite3-sys/sqlcipher/sqlite3.c` 的 `sqlcipher.h` 段，
//! `SQLCIPHER_HMAC_*` / `SQLCIPHER_PBKDF2_*` / `typedef struct sqlcipher_provider`），
//! 不依赖外部文档。

use core::ffi::{c_char, c_int, c_uchar, c_void};

// 依赖在本 crate 的 Cargo.toml 里按常规名声明（aes/cbc/hmac/sha2/sha1/pbkdf2/
// getrandom，均为官方 Signal node-sqlcipher extension 的同一代版本），故此处
// 直接使用，无需别名——别名是 engine 侧引入时才需要的（那边另有新版同名 crate）。

const SQLITE_OK: c_int = 0;
const SQLITE_ERROR: c_int = 1;

// 取自 sqlite3.c:108266-108277
const SQLCIPHER_HMAC_SHA1: c_int = 0;
const SQLCIPHER_HMAC_SHA512: c_int = 2;
const SQLCIPHER_PBKDF2_SHA1: c_int = 0;
const SQLCIPHER_PBKDF2_SHA512: c_int = 2;

const CIPHER_DECRYPT: c_int = 0;
const CIPHER_ENCRYPT: c_int = 1;

const AES256_KEY_SZ: c_int = 32;
const AES_BLOCK_SZ: c_int = 16;
const SHA1_HMAC_SZ: c_int = 20;
const SHA512_HMAC_SZ: c_int = 64;

/// 与 SQLCipher 的 `sqlcipher_provider` 二进制兼容（字段顺序不可改）。
#[repr(C)]
pub struct SqlcipherProvider {
    pub init: Option<extern "C" fn() -> c_int>,
    pub shutdown: Option<extern "C" fn()>,
    pub get_provider_name: Option<extern "C" fn(ctx: *mut c_void) -> *const c_char>,
    pub add_random:
        Option<extern "C" fn(ctx: *mut c_void, buffer: *const c_void, length: c_int) -> c_int>,
    pub random:
        Option<extern "C" fn(ctx: *mut c_void, buffer: *mut c_void, length: c_int) -> c_int>,
    pub hmac: Option<
        extern "C" fn(
            ctx: *mut c_void,
            algorithm: c_int,
            hmac_key: *const c_uchar,
            key_sz: c_int,
            input: *const c_uchar,
            input_sz: c_int,
            input2: *const c_uchar,
            input2_sz: c_int,
            output: *mut c_uchar,
        ) -> c_int,
    >,
    pub kdf: Option<
        extern "C" fn(
            ctx: *mut c_void,
            algorithm: c_int,
            password: *const c_uchar,
            password_sz: c_int,
            salt: *const c_uchar,
            salt_sz: c_int,
            workfactor: c_int,
            key_sz: c_int,
            key: *mut c_uchar,
        ) -> c_int,
    >,
    pub cipher: Option<
        extern "C" fn(
            ctx: *mut c_void,
            mode: c_int,
            key: *const c_uchar,
            key_sz: c_int,
            iv: *const c_uchar,
            input: *const c_uchar,
            input_sz: c_int,
            output: *mut c_uchar,
        ) -> c_int,
    >,
    pub get_cipher: Option<extern "C" fn(ctx: *mut c_void) -> *const c_char>,
    pub get_key_sz: Option<extern "C" fn(ctx: *mut c_void) -> c_int>,
    pub get_iv_sz: Option<extern "C" fn(ctx: *mut c_void) -> c_int>,
    pub get_block_sz: Option<extern "C" fn(ctx: *mut c_void) -> c_int>,
    pub get_hmac_sz: Option<extern "C" fn(ctx: *mut c_void, algorithm: c_int) -> c_int>,
    pub ctx_init: Option<extern "C" fn(ctx: *mut *mut c_void) -> c_int>,
    pub ctx_free: Option<extern "C" fn(ctx: *mut *mut c_void) -> c_int>,
    pub fips_status: Option<extern "C" fn(ctx: *mut c_void) -> c_int>,
    pub get_provider_version: Option<extern "C" fn(ctx: *mut c_void) -> *const c_char>,
}

/// provider 名字（SQLCipher 日志与 `PRAGMA cipher_provider` 会显示它）。
static PROVIDER_NAME: &[u8] = b"rustcrypto-kt\0";
static PROVIDER_VERSION: &[u8] = b"1\0";
static CIPHER_NAME: &[u8] = b"aes-256-cbc\0";

// 注：结构体的 init/shutdown 置 None —— SQLCipher 的调用点是
// `if(!preexisting && p->init) { rc = p->init(); }`，为 NULL 时直接跳过。
// 本实现无可初始化的全局状态（每字节都直接读 OS CSPRNG），无需 hook。

extern "C" fn kt_get_provider_name(_ctx: *mut c_void) -> *const c_char {
    PROVIDER_NAME.as_ptr().cast()
}

extern "C" fn kt_get_provider_version(_ctx: *mut c_void) -> *const c_char {
    PROVIDER_VERSION.as_ptr().cast()
}

/// SQLCipher 用它把额外熵混进自己的 PRNG。本实现无内部状态（直接读 OS 熵），
/// 所以丢弃即可 —— 系统 CSPRNG 本身不需要外部加熵。
extern "C" fn kt_add_random(_ctx: *mut c_void, _buffer: *const c_void, _length: c_int) -> c_int {
    SQLITE_OK
}

extern "C" fn kt_random(_ctx: *mut c_void, buffer: *mut c_void, length: c_int) -> c_int {
    if length <= 0 {
        return SQLITE_OK;
    }
    if buffer.is_null() {
        return SQLITE_ERROR;
    }
    // SAFETY: 调用方保证 buffer 至少有 length 字节可写（SQLCipher 契约）。
    let slice = unsafe { core::slice::from_raw_parts_mut(buffer.cast::<u8>(), length as usize) };
    match getrandom::getrandom(slice) {
        Ok(()) => SQLITE_OK,
        Err(_) => SQLITE_ERROR,
    }
}

extern "C" fn kt_hmac(
    _ctx: *mut c_void,
    algorithm: c_int,
    hmac_key: *const c_uchar,
    key_sz: c_int,
    input: *const c_uchar,
    input_sz: c_int,
    input2: *const c_uchar,
    input2_sz: c_int,
    output: *mut c_uchar,
) -> c_int {
    if hmac_key.is_null() || output.is_null() || key_sz <= 0 {
        return SQLITE_ERROR;
    }
    // SAFETY: 指针与长度由 SQLCipher 保证；input2 可为空（长度 0）。
    let key = unsafe { core::slice::from_raw_parts(hmac_key, key_sz as usize) };
    let parts: [(&[u8], c_int); 2] = [
        (
            unsafe { core::slice::from_raw_parts(input, input_sz.max(0) as usize) },
            input_sz,
        ),
        (
            unsafe {
                if input2.is_null() || input2_sz <= 0 {
                    &[]
                } else {
                    core::slice::from_raw_parts(input2, input2_sz as usize)
                }
            },
            input2_sz,
        ),
    ];
    let _ = parts;

    macro_rules! run_hmac {
        ($hash:ty, $out_len:expr) => {{
            use hmac::Mac as _;
            let Ok(mut mac) = <hmac::Hmac<$hash> as hmac::Mac>::new_from_slice(key) else {
                return SQLITE_ERROR;
            };
            if input_sz > 0 && !input.is_null() {
                mac.update(unsafe { core::slice::from_raw_parts(input, input_sz as usize) });
            }
            if input2_sz > 0 && !input2.is_null() {
                mac.update(unsafe { core::slice::from_raw_parts(input2, input2_sz as usize) });
            }
            let tag = mac.finalize().into_bytes();
            debug_assert_eq!(tag.len(), $out_len);
            unsafe { core::ptr::copy_nonoverlapping(tag.as_ptr(), output, tag.len()) };
            SQLITE_OK
        }};
    }

    match algorithm {
        SQLCIPHER_HMAC_SHA1 => run_hmac!(sha1::Sha1, SHA1_HMAC_SZ as usize),
        SQLCIPHER_HMAC_SHA512 => run_hmac!(sha2::Sha512, SHA512_HMAC_SZ as usize),
        _ => SQLITE_ERROR,
    }
}

extern "C" fn kt_kdf(
    _ctx: *mut c_void,
    algorithm: c_int,
    password: *const c_uchar,
    password_sz: c_int,
    salt: *const c_uchar,
    salt_sz: c_int,
    workfactor: c_int,
    key_sz: c_int,
    key: *mut c_uchar,
) -> c_int {
    if password.is_null() || salt.is_null() || key.is_null() || key_sz <= 0 || workfactor < 0 {
        return SQLITE_ERROR;
    }
    // SAFETY: 指针与长度由 SQLCipher 保证。
    let pass = unsafe { core::slice::from_raw_parts(password, password_sz.max(0) as usize) };
    let salt = unsafe { core::slice::from_raw_parts(salt, salt_sz.max(0) as usize) };
    let out = unsafe { core::slice::from_raw_parts_mut(key, key_sz as usize) };

    match algorithm {
        SQLCIPHER_PBKDF2_SHA1 => {
            pbkdf2::pbkdf2_hmac::<sha1::Sha1>(pass, salt, workfactor as u32, out);
            SQLITE_OK
        }
        SQLCIPHER_PBKDF2_SHA512 => {
            pbkdf2::pbkdf2_hmac::<sha2::Sha512>(pass, salt, workfactor as u32, out);
            SQLITE_OK
        }
        _ => SQLITE_ERROR,
    }
}

extern "C" fn kt_cipher(
    _ctx: *mut c_void,
    mode: c_int,
    key: *const c_uchar,
    key_sz: c_int,
    iv: *const c_uchar,
    input: *const c_uchar,
    input_sz: c_int,
    output: *mut c_uchar,
) -> c_int {
    use aes::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit};

    if key.is_null() || iv.is_null() || input.is_null() || output.is_null() {
        return SQLITE_ERROR;
    }
    if key_sz != AES256_KEY_SZ || input_sz <= 0 || input_sz % AES_BLOCK_SZ != 0 {
        return SQLITE_ERROR;
    }
    // SAFETY: 指针与长度由 SQLCipher 保证。
    let key = unsafe { core::slice::from_raw_parts(key, key_sz as usize) };
    let iv = unsafe { core::slice::from_raw_parts(iv, AES_BLOCK_SZ as usize) };
    let data = unsafe { core::slice::from_raw_parts(input, input_sz as usize) };

    type Aes256CbcEnc = cbc::Encryptor<aes::Aes256>;
    type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;

    let mut buf = data.to_vec();
    let result = match mode {
        CIPHER_ENCRYPT => {
            let Ok(enc) = Aes256CbcEnc::new_from_slices(key, iv) else {
                return SQLITE_ERROR;
            };
            // input_sz 已校验是 16 的整数倍，encrypt_padded_mut::<NoPadding> 不会失败。
            match enc.encrypt_padded_mut::<aes::cipher::block_padding::NoPadding>(
                &mut buf,
                input_sz as usize,
            ) {
                Ok(out) => out,
                Err(_) => return SQLITE_ERROR,
            }
        }
        CIPHER_DECRYPT => {
            let Ok(dec) = Aes256CbcDec::new_from_slices(key, iv) else {
                return SQLITE_ERROR;
            };
            match dec.decrypt_padded_mut::<aes::cipher::block_padding::NoPadding>(&mut buf) {
                Ok(out) => out,
                Err(_) => return SQLITE_ERROR,
            }
        }
        _ => return SQLITE_ERROR,
    };
    // SAFETY: output 至少有 input_sz 字节（SQLCipher 契约）。
    unsafe { core::ptr::copy_nonoverlapping(result.as_ptr(), output, result.len()) };
    SQLITE_OK
}

extern "C" fn kt_get_cipher(_ctx: *mut c_void) -> *const c_char {
    CIPHER_NAME.as_ptr().cast()
}

extern "C" fn kt_get_key_sz(_ctx: *mut c_void) -> c_int {
    AES256_KEY_SZ
}

extern "C" fn kt_get_iv_sz(_ctx: *mut c_void) -> c_int {
    AES_BLOCK_SZ
}

extern "C" fn kt_get_block_sz(_ctx: *mut c_void) -> c_int {
    AES_BLOCK_SZ
}

extern "C" fn kt_get_hmac_sz(_ctx: *mut c_void, algorithm: c_int) -> c_int {
    match algorithm {
        SQLCIPHER_HMAC_SHA1 => SHA1_HMAC_SZ,
        SQLCIPHER_HMAC_SHA512 => SHA512_HMAC_SZ,
        _ => 0,
    }
}

extern "C" fn kt_ctx_init(_ctx: *mut *mut c_void) -> c_int {
    SQLITE_OK
}

extern "C" fn kt_ctx_free(_ctx: *mut *mut c_void) -> c_int {
    SQLITE_OK
}

extern "C" fn kt_fips_status(_ctx: *mut c_void) -> c_int {
    0
}

/// SQLCipher 通过 `SQLCIPHER_CRYPTO_CUSTOM=kt_sqlcipher_provider_setup` 调用本函数
/// 注册 provider。返回值走 SQLCipher 的 rc 判断（0 = SQLITE_OK）。
///
/// # Safety
///
/// `provider` 必须指向 SQLCipher 传入的可写结构体（其生命周期由 SQLCipher 管理）。
#[no_mangle]
pub unsafe extern "C" fn kt_sqlcipher_provider_setup(provider: *mut SqlcipherProvider) -> c_int {
    if provider.is_null() {
        return SQLITE_ERROR;
    }
    let filled = SqlcipherProvider {
        init: None,
        shutdown: None,
        get_provider_name: Some(kt_get_provider_name),
        add_random: Some(kt_add_random),
        random: Some(kt_random),
        hmac: Some(kt_hmac),
        kdf: Some(kt_kdf),
        cipher: Some(kt_cipher),
        get_cipher: Some(kt_get_cipher),
        get_key_sz: Some(kt_get_key_sz),
        get_iv_sz: Some(kt_get_iv_sz),
        get_block_sz: Some(kt_get_block_sz),
        get_hmac_sz: Some(kt_get_hmac_sz),
        ctx_init: Some(kt_ctx_init),
        ctx_free: Some(kt_ctx_free),
        fips_status: Some(kt_fips_status),
        get_provider_version: Some(kt_get_provider_version),
        // 字段顺序与 C 结构体一致；此处显式列出以便 review。
    };
    // SAFETY: 调用方保证 provider 有效且可写。
    unsafe { provider.write(filled) };
    SQLITE_OK
}
