//! Маскирование ключа API в потоковом выводе дочерней команды (`key exec`).
//!
//! Образцы для поиска ([`needles_for_key`]): PEM-текст ключа целиком (с завершающим
//! переводом строки и без), его base64, каждая строка тела PEM и всё тело одной строкой
//! (DER в base64). Так `cat "$APP_STORE_CONNECT_KEY_PATH"`, вывод отдельных строк и
//! `base64 < "$APP_STORE_CONNECT_KEY_PATH"` показывают `***`. Строки `-----BEGIN/END
//! PRIVATE KEY-----` сами по себе не секрет и не маскируются.
//!
//! Алгоритм ([`Masker`]):
//!
//! - Ищутся все вхождения всех строк-образцов, в том числе перекрывающиеся. Пересекающиеся
//!   и соседние вхождения сливаются в одну область, и вся область заменяется одним [`MASK`]:
//!   так при перекрытии двух образцов не остаётся видимого «хвоста» ни одного из них.
//! - Вывод приходит кусками. Позиция в буфере «решена», если любой образец, начинающийся
//!   в ней, либо целиком помещается в буфер, либо уже не совпадает. Нерешённый хвост
//!   (он короче самого длинного образца и совпадает с началом какого-то образца) переносится
//!   в следующий кусок, поэтому значение, разрезанное границей кусков, всё равно маскируется.
//!   Хвост, который не может быть началом значения, выводится сразу: приглашения без
//!   перевода строки не задерживаются.
//! - Открытая область маски тоже переносится между кусками: [`MASK`] выводится один раз.
//!
//! Маскирование — страховка от случайной утечки, а не граница безопасности: ключ,
//! преобразованный дочерней командой (hex, другая разбивка на строки), не распознаётся.

use base64::Engine;
use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Чем заменяется ключ в выводе.
pub const MASK: &[u8] = b"***";

/// Образцы короче этого не маскируются: короткая строка совпадёт с чем угодно и испортит
/// вывод.
const MIN_MASK_LEN: usize = 4;

/// Минимальная длина отдельной строки тела PEM, которая маскируется сама по себе
/// (последняя строка тела бывает короткой).
const MIN_LINE_LEN: usize = 8;

/// Размер буфера чтения вывода дочерней команды.
const READ_BUF: usize = 8 * 1024;

/// Строки, которые нужно маскировать для PEM-текста ключа: сам текст, он же без завершающих
/// пробельных символов, их base64 (стандартный и URL-safe алфавиты без `=`), строки тела
/// и тело одной строкой.
pub fn needles_for_key(pem: &str) -> Vec<Vec<u8>> {
    let value = pem.as_bytes();
    let trimmed = value.trim_ascii_end();
    let mut needles = vec![value.to_vec(), trimmed.to_vec()];
    for form in [value, trimmed] {
        // Без `=`: так находится и base64 внутри более длинной строки, и с паддингом.
        needles.push(STANDARD_NO_PAD.encode(form).into_bytes());
        needles.push(URL_SAFE_NO_PAD.encode(form).into_bytes());
    }
    let body: Vec<&str> = pem
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("-----"))
        .collect();
    for line in &body {
        if line.len() >= MIN_LINE_LEN {
            needles.push(line.as_bytes().to_vec());
        }
    }
    needles.push(body.concat().into_bytes());
    needles.retain(|needle| needle.len() >= MIN_MASK_LEN);
    needles.sort_unstable();
    needles.dedup();
    needles
}

/// Потоковый маскировщик. Не `Clone`/`Debug`: внутри части ключа.
pub struct Masker {
    needles: Vec<Vec<u8>>,
    /// Байты, с которых начинается хотя бы один образец (быстрый пропуск остальных).
    starts: [bool; 256],
    max_len: usize,
    /// Нерешённый хвост предыдущего куска.
    carry: Vec<u8>,
    /// До какой позиции (относительно начала `carry`) байты уже скрыты открытой областью.
    masked_until: usize,
    /// Область маски открыта: [`MASK`] для неё уже выведен.
    region_open: bool,
}

impl Masker {
    pub fn new(needles: impl IntoIterator<Item = Vec<u8>>) -> Self {
        let needles: Vec<Vec<u8>> = needles.into_iter().filter(|n| !n.is_empty()).collect();
        let mut starts = [false; 256];
        for needle in &needles {
            starts[usize::from(needle[0])] = true;
        }
        let max_len = needles.iter().map(Vec::len).max().unwrap_or(0);
        Self {
            needles,
            starts,
            max_len,
            carry: Vec::new(),
            masked_until: 0,
            region_open: false,
        }
    }

    /// Обрабатывает очередной кусок; возвращает то, что уже можно вывести.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.process(chunk, false)
    }

    /// Конец потока: выводит остаток.
    pub fn finish(&mut self) -> Vec<u8> {
        self.process(&[], true)
    }

    fn process(&mut self, chunk: &[u8], eof: bool) -> Vec<u8> {
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(chunk);
        let limit = if eof {
            buf.len()
        } else {
            self.decided_len(&buf)
        };

        let mut out = Vec::with_capacity(limit + MASK.len());
        for pos in 0..limit {
            if self.starts[usize::from(buf[pos])] {
                let rest = &buf[pos..];
                for needle in &self.needles {
                    if rest.starts_with(needle) {
                        if !self.region_open {
                            out.extend_from_slice(MASK);
                            self.region_open = true;
                        }
                        self.masked_until = self.masked_until.max(pos + needle.len());
                    }
                }
            }
            if pos < self.masked_until {
                continue;
            }
            self.region_open = false;
            out.push(buf[pos]);
        }

        if eof {
            self.masked_until = 0;
            self.region_open = false;
        } else {
            self.masked_until = self.masked_until.saturating_sub(limit);
            self.carry = buf.split_off(limit);
        }
        out
    }

    /// Длина решённой части буфера: первая позиция, с которой хвост буфера — собственный
    /// префикс какого-то образца (дальше без следующего куска решить нельзя).
    fn decided_len(&self, buf: &[u8]) -> usize {
        let from = buf.len().saturating_sub(self.max_len.saturating_sub(1));
        (from..buf.len())
            .find(|&pos| {
                let tail = &buf[pos..];
                self.needles
                    .iter()
                    .any(|needle| needle.len() > tail.len() && needle.starts_with(tail))
            })
            .unwrap_or(buf.len())
    }
}

/// Копирует поток `reader` в `writer`, маскируя значения. Ошибка записи (например, закрытый
/// канал у `| head`) прекращает копирование: `reader` закрывается, как в обычном конвейере.
pub async fn pump<R, W>(mut reader: R, mut writer: W, mut masker: Masker) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; READ_BUF];
    loop {
        let read = reader.read(&mut buf).await?;
        if read == 0 {
            break;
        }
        let out = masker.push(&buf[..read]);
        if !out.is_empty() {
            writer.write_all(&out).await?;
            writer.flush().await?;
        }
    }
    let out = masker.finish();
    writer.write_all(&out).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::generated_key_pem;

    fn masker(values: &[&str]) -> Masker {
        Masker::new(values.iter().map(|v| v.as_bytes().to_vec()))
    }

    /// Прогоняет текст кусками заданного размера.
    fn run(masker: &mut Masker, text: &str, chunk: usize) -> String {
        let mut out = Vec::new();
        for part in text.as_bytes().chunks(chunk) {
            out.extend(masker.push(part));
        }
        out.extend(masker.finish());
        String::from_utf8(out).unwrap()
    }

    /// Результат должен совпадать при любом разбиении на куски.
    fn all_chunkings(make: impl Fn() -> Masker, text: &str) -> String {
        let whole = run(&mut make(), text, text.len().max(1));
        for size in 1..=text.len().max(1) {
            assert_eq!(run(&mut make(), text, size), whole, "chunk size {size}");
        }
        whole
    }

    #[test]
    fn masks_single_value() {
        assert_eq!(
            all_chunkings(
                || masker(&["s3cret-value"]),
                "token=s3cret-value; again s3cret-value\n"
            ),
            "token=***; again ***\n"
        );
    }

    #[test]
    fn masks_value_split_across_chunks() {
        let mut m = masker(&["s3cret-value"]);
        let mut out = m.push(b"before s3cr");
        // Начало значения задержано, остальное выведено сразу.
        assert_eq!(out, b"before ");
        out.extend(m.push(b"et-val"));
        out.extend(m.push(b"ue after"));
        out.extend(m.finish());
        assert_eq!(String::from_utf8(out).unwrap(), "before *** after");
    }

    #[test]
    fn does_not_hold_unrelated_tail() {
        let mut m = masker(&["s3cret-value"]);
        assert_eq!(m.push(b"Password: "), b"Password: ");
        assert_eq!(m.push(b"xyz"), b"xyz");
        // Хвост, похожий на начало значения, ждёт; на конце потока выводится как есть.
        assert_eq!(m.push(b" s3c"), b" ");
        assert_eq!(m.finish(), b"s3c");
    }

    #[test]
    fn masks_overlapping_values_as_one_region() {
        assert_eq!(
            all_chunkings(|| masker(&["abcdefgh", "efghijkl"]), "x abcdefghijkl y"),
            "x *** y"
        );
        assert_eq!(
            all_chunkings(
                || masker(&["pass", "password-long"]),
                "1 password-long 2 pass 3 passw"
            ),
            "1 *** 2 *** 3 ***w"
        );
        assert_eq!(all_chunkings(|| masker(&["abcd"]), "abcdabcd!"), "***!");
    }

    #[test]
    fn masks_every_form_of_the_key() {
        let pem = generated_key_pem();
        let key = || Masker::new(needles_for_key(&pem));
        let body: Vec<&str> = pem.lines().filter(|l| !l.starts_with("-----")).collect();
        assert!(body.len() >= 2, "{pem}");

        // `cat` файла ключа.
        assert_eq!(run(&mut key(), &pem, 7), "***");
        assert_eq!(
            run(&mut key(), &format!("[{}]", pem.trim_end()), 5),
            "[***]"
        );
        // Отдельная строка тела и тело одной строкой.
        assert_eq!(
            run(&mut key(), &format!("line: {}\n", body[0]), 3),
            "line: ***\n"
        );
        assert_eq!(
            run(&mut key(), &format!("der={}\n", body.concat()), 64),
            "der=***\n"
        );
        // base64 всего файла (`base64 < key.p8`), с переносами и без паддинга.
        let encoded = base64::engine::general_purpose::STANDARD.encode(&pem);
        let out = run(&mut key(), &encoded, 11);
        assert!(!out.contains(encoded.trim_end_matches('=')), "{out}");
        assert!(out.starts_with("***"), "{out}");
        // Заголовки PEM сами по себе не секрет.
        assert_eq!(
            run(&mut key(), "-----BEGIN PRIVATE KEY-----\n", 4),
            "-----BEGIN PRIVATE KEY-----\n"
        );
    }

    #[test]
    fn passes_through_without_values() {
        let mut m = Masker::new(Vec::new());
        assert_eq!(m.push(b"plain"), b"plain");
        assert_eq!(m.finish(), b"");
    }

    #[test]
    fn handles_binary_output() {
        let value = [0xffu8, 0x00, 0x10, 0x80, 0x7f];
        let mut m = Masker::new([value.to_vec()]);
        let mut input = vec![1u8, 2];
        input.extend_from_slice(&value);
        input.push(3);
        let mut out = Vec::new();
        for byte in &input {
            out.extend(m.push(std::slice::from_ref(byte)));
        }
        out.extend(m.finish());
        assert_eq!(out, [1, 2, b'*', b'*', b'*', 3]);
    }

    #[tokio::test]
    async fn pump_masks_stream() {
        let input: &[u8] = b"line one s3cret-value\nline two\n";
        let mut output = Vec::new();
        pump(input, &mut output, masker(&["s3cret-value"]))
            .await
            .unwrap();
        assert_eq!(output, b"line one ***\nline two\n");
    }
}
