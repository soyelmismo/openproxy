//! Strict decoding of USDC `Transfer` logs returned by `eth_getLogs`.
//!
//! Money-path rule, the reason this module exists:
//!
//! - A **structurally malformed** log (bad topic/data size, non-hex word,
//!   wrong contract, native JSON numbers) is `Err`. The caller must hold the
//!   cursor: skipping it silently would lose a real payment forever.
//! - A **valid ERC-20 transfer we deliberately never credit** — zero value,
//!   or a uint256 value that does not fit the ledger's `i64` micro-USDC
//!   domain — is [`DecodedLog::Ignored`]. It is terminal (never retried) but
//!   MUST NOT hold the cursor: otherwise anyone could send a 0-value
//!   `transfer(collector, 0)` every block and wedge the watcher forever
//!   (a dust attack on the whole checkout).
//!
//! Values are never truncated: an oversized amount is ignored whole, never
//! wrapped into a small credit.

use serde::Serialize;

/// ERC-20 `Transfer(address,address,uint256)` topic0 (keccak of the sig).
pub(super) const TRANSFER_TOPIC: &str =
    "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

/// Hex digits in a 20-byte EVM address.
const ADDRESS_HEX: usize = 40;
/// Hex digits in one 32-byte ABI word (topic or `data`).
const WORD_HEX: usize = 64;

/// A decoded USDC transfer to the collector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UsdcTransfer {
    pub tx_hash: String,
    pub log_index: i64,
    pub block_number: i64,
    /// Amount in micro-USDC (6 decimals).
    pub amount_units: i64,
    pub from_address: String,
    pub to_address: String,
}

/// Result of decoding one `eth_getLogs` entry.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum DecodedLog {
    /// Ours, well-formed and creditable.
    Transfer(UsdcTransfer),
    /// Well-formed but never creditable (zero, or > `i64`). The
    /// scanner advances past it; it is NOT a reason to retry the range.
    Ignored(&'static str),
    /// Removed by a reorg: carries the best-effort `(tx_hash, log_index)`
    /// identity so the scanner can cross-check it against payments that
    /// already credited an order (a CRITICAL, not dust).
    Removed(Option<String>, Option<i64>),
}

/// Strip a required `0x` prefix and enforce an exact hex length.
fn hex_body<'a>(raw: &'a str, len: usize, what: &str) -> Result<&'a str, String> {
    let body = raw
        .strip_prefix("0x")
        .or_else(|| raw.strip_prefix("0X"))
        .ok_or_else(|| format!("{what} must start with 0x"))?;
    if body.len() != len || !body.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "{what} must be exactly 0x followed by {len} hex chars"
        ));
    }
    Ok(body)
}

/// Validate and canonicalise a 20-byte address (`0x` + 40 hex).
fn normalize_address(raw: &str, what: &str) -> Result<String, String> {
    let body = hex_body(raw, ADDRESS_HEX, what)?;
    Ok(format!("0x{}", body.to_ascii_lowercase()))
}

/// Extract the address from an indexed-address topic.
///
/// A topic is a 32-byte ABI word: the 20-byte address sits in the last 40 hex
/// chars, left-padded with 12 zero bytes. The padding is validated, not
/// skipped: a topic whose high bytes are non-zero is not an address.
fn topic_address(raw: &str, what: &str) -> Result<String, String> {
    let body = hex_body(raw, WORD_HEX, what)?;
    let (padding, address) = body.split_at(WORD_HEX - ADDRESS_HEX);
    if padding.bytes().any(|b| b != b'0') {
        return Err(format!("{what} has non-zero address padding"));
    }
    Ok(format!("0x{}", address.to_ascii_lowercase()))
}

/// Parse a JSON-RPC hex quantity (`0x1a`) into an `i64`.
///
/// Leading zeroes are tolerated (some providers pad), but the value must fit
/// in `i64`. Sign characters and native JSON numbers are rejected.
pub(super) fn parse_quantity(raw: &str) -> Result<i64, String> {
    let body = raw
        .strip_prefix("0x")
        .or_else(|| raw.strip_prefix("0X"))
        .ok_or_else(|| format!("quantity is not a hex string: {raw}"))?;
    if body.is_empty() || !body.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("invalid hex quantity {raw}"));
    }
    let digits = body.trim_start_matches('0');
    if digits.is_empty() {
        return Ok(0);
    }
    if digits.len() > 16 {
        return Err(format!("hex quantity {raw} exceeds i64"));
    }
    i64::from_str_radix(digits, 16).map_err(|e| format!("invalid hex quantity {raw}: {e}"))
}

/// Decode one `eth_getLogs` entry.
///
/// - `Ok(Transfer)`: ours and creditable.
/// - `Ok(Ignored)`: valid but terminal — never credit, never retry.
/// - `Err(reason)`: the filter selected it but it cannot be trusted; the
///   caller must hold the cursor.
pub(super) fn decode_log(
    log: &serde_json::Value,
    usdc: &str,
    collector: &str,
    from_block: i64,
    to_block: i64,
) -> Result<DecodedLog, String> {
    if log.get("removed").and_then(serde_json::Value::as_bool) == Some(true) {
        // Best-effort identity for the reorg cross-check in the scanner.
        let tx_hash = log
            .get("transactionHash")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let log_index = log
            .get("logIndex")
            .and_then(serde_json::Value::as_str)
            .and_then(|raw| parse_quantity(raw).ok());
        return Ok(DecodedLog::Removed(tx_hash, log_index));
    }
    let usdc = normalize_address(usdc, "configured usdc_address")?;
    let collector = normalize_address(collector, "configured collector_address")?;

    let address = log
        .get("address")
        .and_then(serde_json::Value::as_str)
        .ok_or("log has no address")?;
    if normalize_address(address, "log address")? != usdc {
        return Err(format!("log address {address} is not the USDC contract"));
    }

    let topics = log
        .get("topics")
        .and_then(serde_json::Value::as_array)
        .ok_or("log has no topics array")?;
    if topics.len() != 3 {
        return Err(format!("expected 3 topics, got {}", topics.len()));
    }
    let topic_str = |i: usize, what: &str| {
        topics
            .get(i)
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("{what} is missing or not a string"))
    };
    let topic0 = hex_body(topic_str(0, "topic0")?, WORD_HEX, "topic0")?;
    if !topic0.eq_ignore_ascii_case(&TRANSFER_TOPIC[2..]) {
        return Err("topic0 is not Transfer".into());
    }
    let from_address = topic_address(topic_str(1, "from topic")?, "from topic")?;
    let to_address = topic_address(topic_str(2, "to topic")?, "to topic")?;
    if to_address != collector {
        return Err(format!("transfer to {to_address} is not the collector"));
    }

    let data = log
        .get("data")
        .and_then(serde_json::Value::as_str)
        .ok_or("log has no data")?;
    let word = hex_body(data, WORD_HEX, "data")?;
    let digits = word.trim_start_matches('0');
    if digits.is_empty() {
        return Ok(DecodedLog::Ignored("zero-value transfer"));
    }
    if digits.len() > 16 {
        return Ok(DecodedLog::Ignored("transfer value exceeds i64"));
    }
    // A 16-digit word can still be > i64::MAX: that is a valid uint256, so
    // ignore it rather than wedging the cursor on a parse error.
    let Ok(amount_units) = i64::from_str_radix(digits, 16) else {
        return Ok(DecodedLog::Ignored("transfer value exceeds i64"));
    };
    if amount_units <= 0 {
        return Ok(DecodedLog::Ignored("non-positive transfer value"));
    }

    let tx_hash = format!(
        "0x{}",
        hex_body(
            log.get("transactionHash")
                .and_then(serde_json::Value::as_str)
                .ok_or("log has no transactionHash")?,
            WORD_HEX,
            "transactionHash"
        )?
        .to_ascii_lowercase()
    );
    let log_index = parse_quantity(
        log.get("logIndex")
            .and_then(serde_json::Value::as_str)
            .ok_or("logIndex is not a hex string")?,
    )?;
    let block_number = parse_quantity(
        log.get("blockNumber")
            .and_then(serde_json::Value::as_str)
            .ok_or("blockNumber is not a hex string")?,
    )?;
    if block_number < from_block || block_number > to_block {
        return Err(format!(
            "block {block_number} outside requested range [{from_block}, {to_block}]"
        ));
    }

    Ok(DecodedLog::Transfer(UsdcTransfer {
        tx_hash,
        log_index,
        block_number,
        amount_units,
        from_address,
        to_address,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLLECTOR: &str = "0x12c7ab78f21ba0253a21d34a0151d44ddb824950";
    const USDC: &str = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";
    const TX: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn log(to: &str, value_hex: &str, idx: &str) -> serde_json::Value {
        serde_json::json!({
            "address": USDC,
            "topics": [
                TRANSFER_TOPIC,
                "0x0000000000000000000000001111111111111111111111111111111111111111",
                format!("0x000000000000000000000000{}", to.trim_start_matches("0x")),
            ],
            "data": format!("0x{value_hex:0>64}"),
            "logIndex": idx,
            "blockNumber": "0x10",
            "transactionHash": TX,
        })
    }

    fn decode(v: &serde_json::Value) -> Result<DecodedLog, String> {
        decode_log(v, USDC, COLLECTOR, 1, 100)
    }

    fn ignored(v: &serde_json::Value) -> &'static str {
        match decode(v) {
            Ok(DecodedLog::Ignored(reason)) => reason,
            other => panic!("expected Ignored, got {other:?}"),
        }
    }

    #[test]
    fn parses_a_usdc_transfer_to_the_collector() {
        let DecodedLog::Transfer(t) = decode(&log(COLLECTOR, "989680", "0x0")).expect("decode")
        else {
            panic!("expected transfer");
        };
        assert_eq!(t.amount_units, 10_000_000, "10 USDC in micro units");
        assert_eq!(t.log_index, 0);
        assert_eq!(t.block_number, 16);
        assert_eq!(t.from_address, "0x1111111111111111111111111111111111111111");
        assert_eq!(t.to_address, COLLECTOR.to_ascii_lowercase());
        assert_eq!(t.tx_hash, TX);
    }

    #[test]
    fn uppercase_hex_and_checksummed_config_are_accepted() {
        let mut l = log(COLLECTOR, "989680", "0x0");
        l["topics"][2] = serde_json::Value::String(format!(
            "0x000000000000000000000000{}",
            COLLECTOR.trim_start_matches("0x").to_uppercase()
        ));
        assert!(matches!(decode(&l), Ok(DecodedLog::Transfer(_))));
    }

    #[test]
    fn removed_logs_are_skipped_not_credited() {
        // A reorg is not a malformed log: nothing was paid, advance.
        let mut l = log(COLLECTOR, "989680", "0x0");
        l["removed"] = serde_json::json!(true);
        ignored(&l);
    }

    #[test]
    fn zero_value_transfer_is_terminal_not_cursor_blocking() {
        // Dust attack: transfer(collector, 0) is a VALID ERC-20 transfer.
        // It must never credit and must never hold the cursor.
        assert_eq!(ignored(&log(COLLECTOR, "0", "0x0")), "zero-value transfer");
    }

    #[test]
    fn oversized_uint256_value_is_ignored_whole_never_truncated() {
        // 2^255-ish: 64 hex digits, far beyond i64. Valid uint256.
        let huge = format!("8{}", "0".repeat(63));
        assert_eq!(
            ignored(&log(COLLECTOR, &huge, "0x0")),
            "transfer value exceeds i64"
        );
        // Exactly 2^64-1: 16 digits, valid uint256, still > i64::MAX.
        assert_eq!(
            ignored(&log(COLLECTOR, &"f".repeat(16), "0x0")),
            "transfer value exceeds i64"
        );
        // 2^63 (i64::MAX + 1): 16 digits that fail i64 parsing.
        assert_eq!(
            ignored(&log(COLLECTOR, &format!("8{}", "0".repeat(15)), "0x0")),
            "transfer value exceeds i64"
        );
    }

    #[test]
    fn wrong_token_address_is_malformed_not_ignored() {
        let mut l = log(COLLECTOR, "989680", "0x0");
        l["address"] = serde_json::json!("0x9999999999999999999999999999999999999999");
        assert!(decode(&l).is_err());
    }

    #[test]
    fn malformed_contract_addresses_hold_the_cursor() {
        let mut l = log(COLLECTOR, "989680", "0x0");
        // 39 hex digits: a short address must never be zero-padded by guess.
        l["address"] = serde_json::json!("0x833589fCD6eDb6E08f4c7C32D4f71b54bdA0291");
        assert!(decode(&l).is_err());
        assert!(
            decode_log(
                &log(COLLECTOR, "989680", "0x0"),
                "0x1234",
                COLLECTOR,
                1,
                100
            )
            .is_err()
        );
    }

    #[test]
    fn transfer_to_another_address_is_malformed() {
        let other = "0x9999999999999999999999999999999999999999";
        assert!(decode(&log(other, "989680", "0x0")).is_err());
    }

    #[test]
    fn topic_size_and_address_padding_are_enforced() {
        let mut short = log(COLLECTOR, "989680", "0x0");
        short["topics"][2] = serde_json::json!("0x1111111111111111111111111111111111111111");
        assert!(decode(&short).is_err(), "20-byte topic must not parse");

        let mut bad_pad = log(COLLECTOR, "989680", "0x0");
        bad_pad["topics"][2] = serde_json::json!(format!(
            "0x010000000000000000000000{}",
            COLLECTOR.trim_start_matches("0x")
        ));
        assert!(
            decode(&bad_pad).is_err(),
            "non-zero padding is not an address"
        );

        let mut numeric = log(COLLECTOR, "989680", "0x0");
        numeric["topics"][1] = serde_json::json!(1);
        assert!(decode(&numeric).is_err());
    }

    #[test]
    fn data_must_be_exactly_one_word() {
        let mut short = log(COLLECTOR, "989680", "0x0");
        short["data"] = serde_json::json!("0x989680");
        assert!(decode(&short).is_err());
        let mut long = log(COLLECTOR, "989680", "0x0");
        long["data"] = serde_json::json!(format!("0x{}", "0".repeat(128)));
        assert!(decode(&long).is_err());
        let mut non_hex = log(COLLECTOR, "989680", "0x0");
        non_hex["data"] = serde_json::json!(format!("0x{}", "z".repeat(64)));
        assert!(decode(&non_hex).is_err());
    }

    #[test]
    fn tx_hash_requires_a_full_32_byte_hex_word() {
        for bad in ["", "0xwatch1", "0xdeadbeef", "not-a-hash"] {
            let mut l = log(COLLECTOR, "989680", "0x0");
            l["transactionHash"] = serde_json::json!(bad);
            assert!(decode(&l).is_err(), "{bad} must be rejected");
        }
        let mut numeric = log(COLLECTOR, "989680", "0x0");
        numeric["transactionHash"] = serde_json::json!(1);
        assert!(decode(&numeric).is_err());
    }

    #[test]
    fn tx_hash_is_canonicalised_for_idempotency() {
        let mut l = log(COLLECTOR, "989680", "0x0");
        l["transactionHash"] = serde_json::json!(TX.to_uppercase().replace("0X", "0x"));
        let DecodedLog::Transfer(t) = decode(&l).expect("decode") else {
            panic!("expected transfer");
        };
        assert_eq!(t.tx_hash, TX, "mixed-case hashes must not double-credit");
    }

    #[test]
    fn quantity_parsing_is_strict() {
        assert_eq!(parse_quantity("0x10").unwrap(), 16);
        assert_eq!(parse_quantity("0x0000010").unwrap(), 16);
        assert_eq!(parse_quantity("0x0").unwrap(), 0);
        assert!(parse_quantity("10").is_err(), "missing 0x");
        assert!(parse_quantity("0x").is_err(), "empty digits");
        assert!(parse_quantity("-0x1").is_err(), "sign");
        assert!(parse_quantity("zz").is_err());
        assert!(parse_quantity("0x10000000000000000").is_err(), "> u64");
    }

    #[test]
    fn numeric_log_index_is_malformed_not_silently_dropped() {
        let mut l = log(COLLECTOR, "989680", "0x0");
        l["logIndex"] = serde_json::json!(3);
        assert!(decode(&l).is_err());
    }

    #[test]
    fn out_of_range_block_is_malformed() {
        assert!(decode_log(&log(COLLECTOR, "989680", "0x0"), USDC, COLLECTOR, 20, 100).is_err());
    }

    #[test]
    fn non_transfer_topic_is_malformed() {
        let mut bad = log(COLLECTOR, "989680", "0x0");
        bad["topics"][0] = serde_json::json!(format!("0x{}", "ab".repeat(32)));
        assert!(decode(&bad).is_err());
    }

    #[test]
    fn six_decimal_precision_is_preserved_exactly() {
        // 10.987654 USDC in micro units: the parser must preserve every digit.
        let amount_units: i64 = 10_000_000 + 987_654;
        let hex = format!("{amount_units:x}");
        let DecodedLog::Transfer(t) = decode(&log(COLLECTOR, &hex, "0x1")).expect("decode") else {
            panic!("expected transfer");
        };
        assert_eq!(t.amount_units, amount_units);
    }
}
