//! Генерация таблиц операций из OpenAPI-спецификации App Store Connect API.
//!
//! Вход — снимок спецификации Apple `spec/openapi.oas.json` (без правок) и файл-дополнение
//! `spec/overlay.toml` (наборы инструментов, уровни доступа, имена, исключения). Результат —
//! `$OUT_DIR/operations.rs` со статическими таблицами `OPERATIONS`, `EXCLUDED` и `TOOLSETS`,
//! которые подключает `src/spec.rs`. Сеть не используется. Подробности:
//!
//! - `build/overlay.rs` — разбор файла-дополнения;
//! - `build/generate.rs` — разбор операций, параметров, тела и ответа, проверки, вывод кода;
//! - `build/naming.rs` — имена инструментов и краткие описания;
//! - `build/access.rs` — уровни доступа;
//! - `build/schema.rs` — JSON Schema аргументов с раскрытыми `$ref`.
//!
//! Если задана переменная `APPSTORE_CONNECT_SPEC_REPORT`, в указанный файл пишется отчёт
//! (JSON): число операций по уровням и наборам, исключённые операции, размеры схем.
//! Его читает `spec/update_spec.py`.

#[path = "build/access.rs"]
mod access;
#[path = "build/generate.rs"]
mod generate;
#[path = "build/naming.rs"]
mod naming;
#[path = "build/overlay.rs"]
mod overlay;
#[path = "build/schema.rs"]
mod schema;

use std::{env, fs, path::PathBuf};

const SPEC_PATH: &str = "spec/openapi.oas.json";
const CHECKSUM_PATH: &str = "spec/openapi.oas.json.sha256";
const OVERLAY_PATH: &str = "spec/overlay.toml";
const REPORT_ENV: &str = "APPSTORE_CONNECT_SPEC_REPORT";

fn main() {
    println!("cargo:rerun-if-changed={SPEC_PATH}");
    println!("cargo:rerun-if-changed={CHECKSUM_PATH}");
    println!("cargo:rerun-if-changed={OVERLAY_PATH}");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build");
    println!("cargo:rerun-if-env-changed={REPORT_ENV}");

    let raw = fs::read_to_string(SPEC_PATH).expect("не удалось прочитать спецификацию");
    check_checksum(&raw);
    let spec: serde_json::Value =
        serde_json::from_str(&raw).expect("спецификация не является корректным JSON");
    let overlay_raw =
        fs::read_to_string(OVERLAY_PATH).expect("не удалось прочитать файл-дополнение");
    let overlay = overlay::parse(&overlay_raw);

    let output = generate::generate(&spec, &overlay);

    if let Some(path) = env::var_os(REPORT_ENV).filter(|path| !path.is_empty()) {
        fs::write(path, &output.report).expect("не удалось записать отчёт генератора");
    }
    if !output.errors.is_empty() {
        panic!(
            "спецификация и spec/overlay.toml не согласованы ({} ошибок):\n- {}",
            output.errors.len(),
            output.errors.join("\n- ")
        );
    }

    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR не задан")).join("operations.rs");
    fs::write(out, output.code).expect("не удалось записать сгенерированные операции");
}

/// Снимок спецификации не правится вручную: его SHA-256 должен совпадать с записанным
/// скриптом обновления (`spec/update_spec.py`) в `spec/openapi.oas.json.sha256`.
fn check_checksum(raw: &str) {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;

    let recorded = fs::read_to_string(CHECKSUM_PATH).expect("не удалось прочитать SHA-256 снимка");
    let recorded = recorded
        .split_whitespace()
        .next()
        .expect("в файле SHA-256 снимка нет хеша");
    let actual = Sha256::digest(raw.as_bytes())
        .iter()
        .fold(String::new(), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        });
    assert_eq!(
        actual, recorded,
        "SHA-256 {SPEC_PATH} не совпадает с {CHECKSUM_PATH}: снимок меняется только скриптом spec/update_spec.py"
    );
}
