//! Offline copy-save CLI for Crimson Desert `.save` files.
//!
//! Scope (intentionally small):
//!   inventory   structured inventory export (read-only)
//!   inspect     header / HMAC / decode stats (read-only)
//!   roundtrip   no-op body decode→encode + reseal with original nonce
//!   add-item    clone a same-key row, or `--donor-key` GUI add-to-bag, onto a COPY
//!   diff        semantic fingerprint + inventory compare of two copies
//!
//! Never writes the input path. Same-key clone is preferred. `--donor-key`
//! is the GUI add-to-bag path (clone a present row, patch `_itemKey`).
//! Consumable/charge donors (`_coolTimePerCharge` present) are refused.
//! This program does **not** guess GoldBar = 53.

use crimson_rs::save::{Body, FieldValue, ObjectBlock, Save, ScalarValue};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const TRANSFERRED_LOW: u32 = 0x0101;

/// InventoryKey labels from CrimsonAtomtic LocalizationProvider.
/// Not PALOC — a maintained table. Unknown keys stay numeric.
fn inventory_label(key: u32) -> &'static str {
    match key {
        1 => "Camp & Contributions",
        2 => "Backpack",
        5 => "Quest Artifacts",
        8 => "Private Storage",
        9 => "Camp Trading Goods",
        10 => "Valuables",
        13 => "Kuku Pot",
        16 => "Enhanced Kuku Cooler",
        19 => "Gatherables Chest",
        20 => "Collectibles",
        _ => "",
    }
}

fn usage() -> &'static str {
    "cd-copy-save — offline Crimson Desert copy-save helper

USAGE
  cd-copy-save <command> [options]

COMMANDS
  inspect     --save FILE --out DIR
  inventory   --save FILE --out DIR
  roundtrip   --save FILE --out DIR [--write-copy FILE]
  add-item    --save FILE --out-save FILE --out DIR --item-key N
              [--container-key K]   default: first container that already
                                    holds --item-key (prefer Backpack=2)
              [--donor-key D]       if N is absent: clone a present row of D
                                    (GUI add-to-bag). Refuses charge/cooltime
                                    donors. Same-key clone still wins if N is
                                    already in the save.
  diff        --before FILE --after FILE --out DIR

Always operates on copies. --out-save / --write-copy refuse to equal --save.
No-args or --help prints this text and exits 2.

GoldBar: this tool never assumes item key 53. Resolve string_key GoldBar
from a 2.04 iteminfo body, then pass that key. Same-key clone if present;
otherwise --donor-key of a same-shape backpack row (not beer/water).
"
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.len() <= 1 || args.iter().any(|a| a == "--help" || a == "-h") {
        let _ = writeln!(io::stderr(), "{}", usage());
        return ExitCode::from(2);
    }
    match run(&args[1..]) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(io::stderr(), "error: {e}");
            ExitCode::from(1)
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let rest = if args.is_empty() { &[] } else { &args[1..] };
    match cmd {
        "inspect" => cmd_inspect(&parse_kv(rest)?),
        "inventory" => cmd_inventory(&parse_kv(rest)?),
        "roundtrip" => cmd_roundtrip(&parse_kv(rest)?),
        "add-item" => cmd_add_item(&parse_kv(rest)?),
        "diff" => cmd_diff(&parse_kv(rest)?),
        other => Err(format!("unknown command '{other}'\n\n{}", usage())),
    }
}

fn parse_kv(args: &[String]) -> Result<BTreeMap<String, String>, String> {
    let mut m = BTreeMap::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if !a.starts_with("--") {
            return Err(format!("unexpected argument: {a}"));
        }
        let key = a.trim_start_matches('-').to_string();
        if i + 1 >= args.len() {
            return Err(format!("missing value for --{key}"));
        }
        m.insert(key, args[i + 1].clone());
        i += 2;
    }
    Ok(m)
}

fn require<'a>(m: &'a BTreeMap<String, String>, k: &str) -> Result<&'a str, String> {
    m.get(k)
        .map(String::as_str)
        .ok_or_else(|| format!("missing --{k}"))
}

fn sha256_bytes(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let b = fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    Ok(sha256_bytes(&b))
}

fn ensure_out(dir: &str) -> Result<PathBuf, String> {
    let p = PathBuf::from(dir);
    fs::create_dir_all(&p).map_err(|e| format!("mkdir {}: {e}", p.display()))?;
    Ok(p)
}

fn refuse_same(src: &Path, dst: &Path) -> Result<(), String> {
    let a = fs::canonicalize(src).unwrap_or_else(|_| src.to_path_buf());
    let b = if dst.exists() {
        fs::canonicalize(dst).unwrap_or_else(|_| dst.to_path_buf())
    } else {
        dst.to_path_buf()
    };
    if a == b {
        return Err(format!(
            "refusing to overwrite input {}",
            src.display()
        ));
    }
    Ok(())
}

struct Loaded {
    raw: Vec<u8>,
    save: Save,
    body: Body,
    blocks: Vec<ObjectBlock>,
}

fn load_save(path: &Path) -> Result<Loaded, String> {
    let raw = fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let save = Save::parse(&raw).map_err(|e| format!("parse_save: {e}"))?;
    let body = Body::parse(&save.body).map_err(|e| format!("parse_body: {e}"))?;
    let blocks = body.decode_blocks(&save.body);
    Ok(Loaded {
        raw,
        save,
        body,
        blocks,
    })
}

fn undecoded_total(blocks: &[ObjectBlock]) -> usize {
    fn rec(b: &ObjectBlock) -> usize {
        let mut n = b.undecoded_ranges.iter().map(|(s, e)| e - s).sum();
        for f in &b.fields {
            match &f.value {
                FieldValue::ObjectList { elements, .. } => {
                    n += elements.iter().map(rec).sum::<usize>();
                }
                FieldValue::Locator { child: Some(c), .. } => n += rec(c),
                _ => {}
            }
        }
        n
    }
    blocks.iter().map(rec).sum()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn scalar_display(v: &ScalarValue) -> String {
    match v {
        ScalarValue::Bool(b) => format!("bool:{b}"),
        ScalarValue::U8(x) => x.to_string(),
        ScalarValue::U16(x) => x.to_string(),
        ScalarValue::U32(x) => x.to_string(),
        ScalarValue::U64(x) => x.to_string(),
        ScalarValue::I8(x) => x.to_string(),
        ScalarValue::I16(x) => x.to_string(),
        ScalarValue::I32(x) => x.to_string(),
        ScalarValue::I64(x) => x.to_string(),
        ScalarValue::F32(x) => format!("{x}"),
        ScalarValue::F64(x) => format!("{x}"),
        ScalarValue::F32x3(a) => format!("{a:?}"),
        ScalarValue::F32x4(a) => format!("{a:?}"),
        ScalarValue::U32x4(a) => format!("{a:?}"),
        ScalarValue::Bytes(b) => format!("bytes:{}", hex(b)),
    }
}

fn as_u64(v: &ScalarValue) -> Option<u64> {
    match v {
        ScalarValue::U8(x) => Some(*x as u64),
        ScalarValue::U16(x) => Some(*x as u64),
        ScalarValue::U32(x) => Some(*x as u64),
        ScalarValue::U64(x) => Some(*x),
        ScalarValue::I8(x) => Some(*x as u64),
        ScalarValue::I16(x) => Some(*x as u64),
        ScalarValue::I32(x) => Some(*x as u64),
        ScalarValue::I64(x) => Some(*x as u64),
        ScalarValue::Bool(x) => Some(*x as u64),
        _ => None,
    }
}

#[derive(Clone, Debug)]
struct InvRow {
    block_idx: usize,
    inv_idx: usize,
    item_idx: usize,
    inventory_key: u32,
    item_key: u32,
    transferred_item_key: u32,
    slot_no: u32,
    item_no: u64,
    stack_count: u64,
    is_locked: bool,
    is_new_mark: bool,
    is_new_mark_present: bool,
    mask_hex: String,
    has_cool_time: bool,
}

fn mask_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn collect_inventory(blocks: &[ObjectBlock]) -> Vec<InvRow> {
    let mut rows = Vec::new();
    for (block_idx, block) in blocks.iter().enumerate() {
        if block.class_name != "InventorySaveData" {
            continue;
        }
        for inv_list_field in &block.fields {
            if !inv_list_field.name.eq_ignore_ascii_case("_inventorylist") {
                continue;
            }
            let FieldValue::ObjectList {
                elements: containers,
                ..
            } = &inv_list_field.value
            else {
                continue;
            };
            for (inv_idx, container) in containers.iter().enumerate() {
                let inventory_key = field_u32(container, "_inventoryKey").unwrap_or(0);
                for f in &container.fields {
                    if !f.name.eq_ignore_ascii_case("_itemList") {
                        continue;
                    }
                    let FieldValue::ObjectList { elements: items, .. } = &f.value else {
                        continue;
                    };
                    for (item_idx, item) in items.iter().enumerate() {
                        rows.push(InvRow {
                            block_idx,
                            inv_idx,
                            item_idx,
                            inventory_key,
                            item_key: field_u32(item, "_itemKey").unwrap_or(0),
                            transferred_item_key: field_u32(item, "_transferredItemKey")
                                .unwrap_or(0),
                            slot_no: field_u32(item, "_slotNo").unwrap_or(0),
                            item_no: field_u64(item, "_itemNo").unwrap_or(0),
                            stack_count: field_u64(item, "_stackCount").unwrap_or(0),
                            is_locked: field_truthy(item, "_isLocked"),
                            is_new_mark: field_truthy(item, "_isNewMark"),
                            is_new_mark_present: field_present(item, "_isNewMark"),
                            mask_hex: mask_hex(&item.mask_bytes),
                            has_cool_time: field_present(item, "_coolTimePerCharge"),
                        });
                    }
                }
            }
        }
    }
    rows
}

fn field_present(block: &ObjectBlock, name: &str) -> bool {
    block
        .fields
        .iter()
        .any(|f| f.name.eq_ignore_ascii_case(name) && f.present)
}

fn field_truthy(block: &ObjectBlock, name: &str) -> bool {
    block.fields.iter().any(|f| {
        f.name.eq_ignore_ascii_case(name)
            && f.present
            && matches!(&f.value, FieldValue::Scalar(v) if as_u64(v).unwrap_or(0) != 0)
    })
}

fn field_u32(block: &ObjectBlock, name: &str) -> Option<u32> {
    field_u64(block, name).map(|v| v as u32)
}

fn field_u64(block: &ObjectBlock, name: &str) -> Option<u64> {
    for f in &block.fields {
        if f.name.eq_ignore_ascii_case(name) && f.present {
            if let FieldValue::Scalar(v) = &f.value {
                return as_u64(v);
            }
        }
    }
    None
}

fn inventory_json(rows: &[InvRow]) -> Value {
    let mut by_container: BTreeMap<u32, Vec<Value>> = BTreeMap::new();
    let mut key_counts: BTreeMap<u32, u64> = BTreeMap::new();
    for r in rows {
        key_counts
            .entry(r.item_key)
            .and_modify(|n| *n += r.stack_count.max(1))
            .or_insert(r.stack_count.max(1));
        let label = inventory_label(r.inventory_key);
        by_container.entry(r.inventory_key).or_default().push(json!({
            "block_idx": r.block_idx,
            "inv_idx": r.inv_idx,
            "item_idx": r.item_idx,
            "inventory_key": r.inventory_key,
            "inventory_label": label,
            "item_key": r.item_key,
            "transferred_item_key": r.transferred_item_key,
            "slot_no": r.slot_no,
            "item_no": r.item_no,
            "stack_count": r.stack_count,
            "is_locked": r.is_locked,
            "is_new_mark": r.is_new_mark,
            "is_new_mark_present": r.is_new_mark_present,
        }));
    }
    let containers: Vec<Value> = by_container
        .into_iter()
        .map(|(k, items)| {
            json!({
                "inventory_key": k,
                "inventory_label": inventory_label(k),
                "item_count": items.len(),
                "items": items,
            })
        })
        .collect();
    json!({
        "row_count": rows.len(),
        "distinct_item_keys": key_counts.len(),
        "containers": containers,
        "item_key_stack_totals": key_counts.iter().map(|(k,v)| json!({"item_key": k, "stack_total": v})).collect::<Vec<_>>(),
    })
}

/// Canonical scalar fingerprint used to prove unrelated records did not move.
fn fingerprint(blocks: &[ObjectBlock]) -> (String, u64, BTreeMap<String, String>) {
    let mut map = BTreeMap::new();
    for (i, b) in blocks.iter().enumerate() {
        walk_block(b, &format!("toc[{i}]:{}", b.class_name), &mut map);
    }
    let payload = map
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("\n");
    let n = map.len() as u64;
    (sha256_bytes(payload.as_bytes()), n, map)
}

fn walk_block(block: &ObjectBlock, path: &str, out: &mut BTreeMap<String, String>) {
    out.insert(
        format!("{path}.@class"),
        format!("{}#{}", block.class_name, block.class_index),
    );
    for f in &block.fields {
        let fp = format!("{path}.{}", f.name);
        if !f.present {
            out.insert(fp, format!("absent:marker={}", f.absent_marker as u8));
            continue;
        }
        match &f.value {
            FieldValue::None => {
                out.insert(fp, "none".into());
            }
            FieldValue::Scalar(v) => {
                out.insert(fp, format!("{}:{}", f.kind.as_str(), scalar_display(v)));
            }
            FieldValue::InlineBytes { count, bytes } => {
                out.insert(fp, format!("inline:{count}:{}", sha256_bytes(bytes)));
            }
            FieldValue::DynamicArray {
                count,
                bytes,
                header_variant,
                header_bytes,
                trailer_bytes,
            } => {
                out.insert(
                    fp,
                    format!(
                        "dyn:{header_variant}:{count}:h{}:b{}:t{}",
                        sha256_bytes(header_bytes),
                        sha256_bytes(bytes),
                        sha256_bytes(trailer_bytes)
                    ),
                );
            }
            FieldValue::Locator {
                child_type_name,
                child,
                ..
            } => {
                out.insert(fp.clone(), format!("locator:{child_type_name}"));
                if let Some(c) = child {
                    walk_block(c, &fp, out);
                }
            }
            FieldValue::ObjectList {
                count,
                header_variant,
                header_bytes,
                elements,
            } => {
                out.insert(
                    fp.clone(),
                    format!(
                        "list:{header_variant}:count={count}:n={}:h={}",
                        elements.len(),
                        sha256_bytes(header_bytes)
                    ),
                );
                for (i, el) in elements.iter().enumerate() {
                    walk_block(el, &format!("{fp}[{i}]"), out);
                }
            }
        }
    }
}

fn header_summary(save: &Save, raw_len: usize, file_sha: &str) -> Value {
    json!({
        "file_bytes": raw_len,
        "file_sha256": file_sha,
        "magic": "SAVE",
        "version": save.header.version(),
        "flags": save.header.flags(),
        "flags_hex": format!("0x{:04x}", save.header.flags()),
        "uncompressed_size": save.header.uncompressed_size(),
        "payload_size": save.header.payload_size(),
        "hmac_ok": save.hmac_ok,
        "body_sha256": sha256_bytes(&save.body),
        "nonce_hex": hex(&save.header.nonce()),
        "crypto": {
            "cipher": "ChaCha20 (IETF, header nonce = u32 counter || 12-byte nonce)",
            "mac": "HMAC-SHA256 over LZ4-compressed bytes (post-decrypt)",
            "compression": "LZ4 block (not frame)",
            "key_source": "version-derived in crimson-rs; no per-account key",
        },
    })
}

fn decode_stats(loaded: &Loaded) -> Value {
    let mut classes: BTreeMap<String, u64> = BTreeMap::new();
    for b in &loaded.blocks {
        *classes.entry(b.class_name.clone()).or_insert(0) += 1;
    }
    json!({
        "schema_types": loaded.body.schema.type_count,
        "schema_end": loaded.body.schema.schema_end,
        "root_type": loaded.body.schema.root_type,
        "toc_count": loaded.body.toc.entries.len(),
        "decoded_blocks": loaded.blocks.len(),
        "undecoded_bytes": undecoded_total(&loaded.blocks),
        "class_counts": classes,
        "schema_type_names": loaded.body.schema.types.iter().map(|t| t.name.clone()).collect::<Vec<_>>(),
        "currency_like_schema_fields": currency_like_schema_fields(&loaded.body),
        "bank_schema_fields": schema_fields_named(&loaded.body, "InventoryItemContentsSaveData_BankData"),
        "item_save_schema_fields": schema_fields_named(&loaded.body, "ItemSaveData"),
    })
}

fn schema_fields_named(body: &Body, class: &str) -> Vec<Value> {
    body.schema
        .types
        .iter()
        .filter(|t| t.name == class)
        .flat_map(|t| {
            t.fields.iter().map(|f| {
                json!({
                    "field": f.name,
                    "type_name": f.type_name,
                    "meta_kind": f.meta_kind,
                    "meta_size": f.meta_size,
                })
            })
        })
        .collect()
}

fn currency_like_schema_fields(body: &Body) -> Vec<Value> {
    let mut out = Vec::new();
    for t in &body.schema.types {
        for f in &t.fields {
            let blob = format!("{} {} {}", t.name, f.name, f.type_name).to_ascii_lowercase();
            if blob.contains("money")
                || blob.contains("gold")
                || blob.contains("silver")
                || blob.contains("copper")
                || blob.contains("currency")
                || blob.contains("pearl")
            {
                out.push(json!({
                    "class": t.name,
                    "field": f.name,
                    "type_name": f.type_name,
                    "meta_kind": f.meta_kind,
                    "meta_size": f.meta_size,
                }));
            }
        }
    }
    out
}

fn write_json(path: &Path, v: &Value) -> Result<(), String> {
    let s = serde_json::to_string_pretty(v).map_err(|e| e.to_string())?;
    fs::write(path, s).map_err(|e| format!("write {}: {e}", path.display()))
}

fn cmd_inspect(m: &BTreeMap<String, String>) -> Result<(), String> {
    let save_path = PathBuf::from(require(m, "save")?);
    let out = ensure_out(require(m, "out")?)?;
    let file_sha = sha256_file(&save_path)?;
    let loaded = load_save(&save_path)?;
    let (fp, fp_n, _) = fingerprint(&loaded.blocks);
    let report = json!({
        "command": "inspect",
        "source": save_path.display().to_string(),
        "header": header_summary(&loaded.save, loaded.raw.len(), &file_sha),
        "decode": decode_stats(&loaded),
        "fingerprint_sha256": fp,
        "fingerprint_entries": fp_n,
        "parser_target_gamedata": "2.04 (crimson-rs PARSER_TARGET 2/4)",
    });
    write_json(&out.join("inspect.json"), &report)?;
    println!(
        "inspect hmac_ok={} version={} flags=0x{:04x} blocks={} undecoded={} fp={}",
        loaded.save.hmac_ok,
        loaded.save.header.version(),
        loaded.save.header.flags(),
        loaded.blocks.len(),
        undecoded_total(&loaded.blocks),
        fp
    );
    Ok(())
}

fn cmd_inventory(m: &BTreeMap<String, String>) -> Result<(), String> {
    let save_path = PathBuf::from(require(m, "save")?);
    let out = ensure_out(require(m, "out")?)?;
    let file_sha = sha256_file(&save_path)?;
    let loaded = load_save(&save_path)?;
    let rows = collect_inventory(&loaded.blocks);
    let inv = inventory_json(&rows);
    let report = json!({
        "command": "inventory",
        "source": save_path.display().to_string(),
        "header": header_summary(&loaded.save, loaded.raw.len(), &file_sha),
        "decode": decode_stats(&loaded),
        "inventory": inv,
        "goldbar_note": "Item names are not stored in the save. GoldBar is a string_key in iteminfo, not a save field. This export lists numeric ItemKey only. Do not assume 53.",
    });
    write_json(&out.join("inventory.json"), &report)?;
    println!(
        "inventory rows={} containers_listed hmac_ok={}",
        rows.len(),
        loaded.save.hmac_ok
    );
    Ok(())
}

fn cmd_roundtrip(m: &BTreeMap<String, String>) -> Result<(), String> {
    let save_path = PathBuf::from(require(m, "save")?);
    let out = ensure_out(require(m, "out")?)?;
    let file_sha = sha256_file(&save_path)?;
    let loaded = load_save(&save_path)?;
    let undecoded = undecoded_total(&loaded.blocks);
    let encoded = loaded
        .body
        .write(&loaded.save.body, &loaded.blocks)
        .map_err(|e| format!("encode_body: {e}"))?;
    let body_identical = encoded == loaded.save.body;

    let mut resealed = loaded.save.clone();
    resealed.body = encoded.clone();
    let nonce = loaded.save.header.nonce();
    let written = resealed
        .write_with_nonce(nonce)
        .map_err(|e| format!("write_with_nonce: {e}"))?;
    let reparsed = Save::parse(&written).map_err(|e| format!("reparse written: {e}"))?;
    let body_after_reseal = reparsed.body == encoded;
    let hmac_after = reparsed.hmac_ok;
    let file_bytes_identical = written == loaded.raw;

    if let Some(copy) = m.get("write-copy") {
        let dst = PathBuf::from(copy);
        refuse_same(&save_path, &dst)?;
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).ok();
        }
        fs::write(&dst, &written).map_err(|e| format!("write {}: {e}", dst.display()))?;
    }

    let report = json!({
        "command": "roundtrip",
        "source": save_path.display().to_string(),
        "header": header_summary(&loaded.save, loaded.raw.len(), &file_sha),
        "decode": decode_stats(&loaded),
        "body_decode_encode_identical": body_identical,
        "undecoded_bytes": undecoded,
        "reseal": {
            "hmac_ok_on_reload": hmac_after,
            "body_sha256_after": sha256_bytes(&reparsed.body),
            "body_matches_encoded": body_after_reseal,
            "on_disk_bytes_identical_to_input": file_bytes_identical,
            "note": "LZ4 is not guaranteed to emit the original compressed bytes; body identity is the load-bearing check. File-byte identity is reported but not required.",
        },
        "ok": body_identical && undecoded == 0 && hmac_after && body_after_reseal,
    });
    write_json(&out.join("roundtrip.json"), &report)?;
    println!(
        "roundtrip body_identical={} undecoded={} hmac_reload={} file_bytes_identical={} ok={}",
        body_identical,
        undecoded,
        hmac_after,
        file_bytes_identical,
        report["ok"]
    );
    Ok(())
}

fn cmd_add_item(m: &BTreeMap<String, String>) -> Result<(), String> {
    let save_path = PathBuf::from(require(m, "save")?);
    let out_save = PathBuf::from(require(m, "out-save")?);
    let out = ensure_out(require(m, "out")?)?;
    refuse_same(&save_path, &out_save)?;
    let item_key: u32 = require(m, "item-key")?
        .parse()
        .map_err(|_| " --item-key must be a decimal u32".to_string())?;
    let prefer_container: Option<u32> = m.get("container-key").and_then(|s| s.parse().ok());
    let donor_key: Option<u32> = m.get("donor-key").and_then(|s| s.parse().ok());
    if m.contains_key("donor-key") && donor_key.is_none() {
        return Err(" --donor-key must be a decimal u32".into());
    }

    let file_sha = sha256_file(&save_path)?;
    let mut loaded = load_save(&save_path)?;
    if !loaded.save.hmac_ok {
        return Err("input HMAC failed; refusing to mutate".into());
    }
    if undecoded_total(&loaded.blocks) != 0 {
        return Err("undecoded bytes in input; refusing to mutate".into());
    }

    let before_rows = collect_inventory(&loaded.blocks);
    let (fp_before, fp_n_before, map_before) = fingerprint(&loaded.blocks);

    let loc = find_clone_source(&loaded.blocks, item_key, donor_key, prefer_container)?;
    let added = apply_clone_and_patch(&mut loaded.blocks, item_key, loc)?;

    let new_body = loaded
        .body
        .write(&loaded.save.body, &loaded.blocks)
        .map_err(|e| format!("encode after clone: {e}"))?;
    let new_parsed = Body::parse(&new_body).map_err(|e| format!("reparse body: {e}"))?;
    let new_blocks = new_parsed.decode_blocks(&new_body);
    if undecoded_total(&new_blocks) != 0 {
        return Err("post-clone re-decode has undecoded bytes; aborting".into());
    }

    let mut out_save_obj = loaded.save.clone();
    out_save_obj.body = new_body;
    let written = out_save_obj
        .write_with_nonce(loaded.save.header.nonce())
        .map_err(|e| format!("write_with_nonce: {e}"))?;
    if let Some(parent) = out_save.parent() {
        fs::create_dir_all(parent).ok();
    }
    fs::write(&out_save, &written).map_err(|e| format!("write {}: {e}", out_save.display()))?;

    // Read back the copy only.
    let readback = load_save(&out_save)?;
    let after_rows = collect_inventory(&readback.blocks);
    let (fp_after, fp_n_after, map_after) = fingerprint(&readback.blocks);
    let diff = fingerprint_diff(&map_before, &map_after);
    let inv_diff = inventory_diff(&before_rows, &after_rows);

    let report = json!({
        "command": "add-item",
        "source": save_path.display().to_string(),
        "source_sha256": file_sha,
        "copy": out_save.display().to_string(),
        "copy_sha256": sha256_file(&out_save)?,
        "item_key": item_key,
        "donor_key": donor_key,
        "clone": added,
        "fingerprint_before": { "sha256": fp_before, "entries": fp_n_before },
        "fingerprint_after": { "sha256": fp_after, "entries": fp_n_after },
        "fingerprint_diff_counts": {
            "added": diff.added.len(),
            "removed": diff.removed.len(),
            "changed": diff.changed.len(),
        },
        "fingerprint_diff": diff.to_json(),
        "inventory_diff": inv_diff,
        "readback": {
            "hmac_ok": readback.save.hmac_ok,
            "undecoded_bytes": undecoded_total(&readback.blocks),
            "body_sha256": sha256_bytes(&readback.save.body),
        },
        "ok": readback.save.hmac_ok
            && undecoded_total(&readback.blocks) == 0
            && inv_diff["added_rows"] == 1
            && inv_diff["removed_rows"] == 0,
    });
    write_json(&out.join("add_item.json"), &report)?;
    println!(
        "add-item copy={} hmac_ok={} added_rows={} fp_added={} fp_removed={} fp_changed={} ok={}",
        out_save.display(),
        readback.save.hmac_ok,
        inv_diff["added_rows"],
        diff.added.len(),
        diff.removed.len(),
        diff.changed.len(),
        report["ok"]
    );
    Ok(())
}

#[derive(Clone)]
struct CloneLoc {
    block_idx: usize,
    inv_idx: usize,
    item_idx: usize,
    inventory_key: u32,
    source_item_key: u32,
    source_mask_hex: String,
    used_donor: bool,
}

fn pick_row(matches: &[InvRow], prefer_container: Option<u32>) -> Result<InvRow, String> {
    if matches.is_empty() {
        return Err("no clone-source rows".into());
    }
    if let Some(ck) = prefer_container {
        return matches
            .iter()
            .find(|r| r.inventory_key == ck)
            .cloned()
            .ok_or_else(|| {
                format!(
                    "clone source not found in container {ck} (have inventory_keys {:?})",
                    matches
                        .iter()
                        .map(|r| r.inventory_key)
                        .collect::<BTreeSet<_>>()
                )
            });
    }
    Ok(matches
        .iter()
        .find(|r| r.inventory_key == 2)
        .cloned()
        .unwrap_or_else(|| matches[0].clone()))
}

fn loc_from_row(pick: &InvRow, used_donor: bool) -> CloneLoc {
    CloneLoc {
        block_idx: pick.block_idx,
        inv_idx: pick.inv_idx,
        item_idx: pick.item_idx,
        inventory_key: pick.inventory_key,
        source_item_key: pick.item_key,
        source_mask_hex: pick.mask_hex.clone(),
        used_donor,
    }
}

fn find_clone_source(
    blocks: &[ObjectBlock],
    item_key: u32,
    donor_key: Option<u32>,
    prefer_container: Option<u32>,
) -> Result<CloneLoc, String> {
    let rows = collect_inventory(blocks);
    let same_key: Vec<InvRow> = rows.iter().filter(|r| r.item_key == item_key).cloned().collect();
    if !same_key.is_empty() {
        let pick = pick_row(&same_key, prefer_container)?;
        return Ok(loc_from_row(&pick, false));
    }
    let Some(donor_key) = donor_key else {
        return Err(format!(
            "item_key {item_key} is not present in this save. \
             Refusing to guess a donor. Pass --donor-key D to clone a present \
             row of D (GUI add-to-bag). Charge/cooltime donors are refused. \
             Do not retarget beer/water onto a currency or GoldBar row."
        ));
    };
    let donors: Vec<InvRow> = rows
        .iter()
        .filter(|r| r.item_key == donor_key)
        .cloned()
        .collect();
    if donors.is_empty() {
        return Err(format!(
            "item_key {item_key} is absent and --donor-key {donor_key} is also absent"
        ));
    }
    let safe: Vec<InvRow> = donors.iter().filter(|r| !r.has_cool_time).cloned().collect();
    if safe.is_empty() {
        return Err(format!(
            "--donor-key {donor_key} only exists as a charge/cooltime row \
             (_coolTimePerCharge present). Cloning that mask is the historical \
             beer/water CTD. Pick a backpack row without cooltime."
        ));
    }
    let pick = pick_row(&safe, prefer_container)?;
    Ok(loc_from_row(&pick, true))
}

fn apply_clone_and_patch(
    blocks: &mut [ObjectBlock],
    item_key: u32,
    loc: CloneLoc,
) -> Result<Value, String> {
    let mut max_item_no: u64 = 0;
    let mut max_slot: u32 = 0;
    {
        let rows = collect_inventory(blocks);
        for r in &rows {
            max_item_no = max_item_no.max(r.item_no);
            if r.block_idx == loc.block_idx && r.inv_idx == loc.inv_idx {
                max_slot = max_slot.max(r.slot_no);
            }
        }
    }
    let new_item_no = max_item_no.saturating_add(1);
    let new_slot = max_slot.saturating_add(1).min(u16::MAX as u32);
    let new_transferred = ((item_key & 0xFFFF) << 16) | TRANSFERRED_LOW;

    let block = blocks
        .get_mut(loc.block_idx)
        .ok_or("block_idx out of range")?;
    let inv_list = block
        .fields
        .iter_mut()
        .find(|f| f.name.eq_ignore_ascii_case("_inventorylist"))
        .ok_or("no _inventorylist")?;
    let FieldValue::ObjectList {
        elements: containers,
        ..
    } = &mut inv_list.value
    else {
        return Err("_inventorylist is not an object_list".into());
    };
    let container = containers
        .get_mut(loc.inv_idx)
        .ok_or("inv_idx out of range")?;
    let item_list = container
        .fields
        .iter_mut()
        .find(|f| f.name.eq_ignore_ascii_case("_itemList"))
        .ok_or("no _itemList")?;
    let FieldValue::ObjectList {
        count,
        header_variant,
        header_bytes,
        elements: items,
    } = &mut item_list.value
    else {
        return Err("_itemList is not an object_list".into());
    };
    if loc.item_idx >= items.len() {
        return Err("item_idx out of range".into());
    }
    let mut clone = items[loc.item_idx].clone();
    set_scalar_u32(&mut clone, "_itemKey", item_key)?;
    set_scalar_u64(&mut clone, "_stackCount", 1)?;
    set_scalar_u32(&mut clone, "_slotNo", new_slot)?;
    set_scalar_u64(&mut clone, "_itemNo", new_item_no)?;
    if field_present(&clone, "_transferredItemKey")
        || clone
            .fields
            .iter()
            .any(|f| f.name == "_transferredItemKey")
    {
        let _ = set_scalar_u32(&mut clone, "_transferredItemKey", new_transferred);
    }
    items.push(clone);
    *count = items.len() as u32;
    patch_list_count_header(header_bytes, header_variant, *count)?;

    Ok(json!({
        "cloned_from": {
            "block_idx": loc.block_idx,
            "inv_idx": loc.inv_idx,
            "item_idx": loc.item_idx,
            "inventory_key": loc.inventory_key,
            "inventory_label": inventory_label(loc.inventory_key),
            "source_item_key": loc.source_item_key,
            "source_mask_hex": loc.source_mask_hex,
            "used_donor": loc.used_donor,
        },
        "new_slot_no": new_slot,
        "new_item_no": new_item_no,
        "new_stack_count": 1,
        "new_transferred_item_key": new_transferred,
        "appended_at_index": *count - 1,
    }))
}

fn set_scalar_u32(block: &mut ObjectBlock, name: &str, v: u32) -> Result<(), String> {
    for f in &mut block.fields {
        if f.name.eq_ignore_ascii_case(name) {
            match &mut f.value {
                FieldValue::Scalar(ScalarValue::U32(x)) => {
                    *x = v;
                    f.present = true;
                    return Ok(());
                }
                FieldValue::Scalar(ScalarValue::U16(x)) => {
                    if v > u16::MAX as u32 {
                        return Err(format!("{name} does not fit in u16"));
                    }
                    *x = v as u16;
                    f.present = true;
                    return Ok(());
                }
                FieldValue::Scalar(ScalarValue::U64(x)) => {
                    *x = v as u64;
                    f.present = true;
                    return Ok(());
                }
                other => {
                    return Err(format!("{name} is not a u16/u32/u64 scalar: {other:?}"));
                }
            }
        }
    }
    Err(format!("field {name} not found"))
}

fn set_scalar_u64(block: &mut ObjectBlock, name: &str, v: u64) -> Result<(), String> {
    for f in &mut block.fields {
        if f.name.eq_ignore_ascii_case(name) {
            match &mut f.value {
                FieldValue::Scalar(ScalarValue::U64(x)) => {
                    *x = v;
                    f.present = true;
                    return Ok(());
                }
                FieldValue::Scalar(ScalarValue::U32(x)) => {
                    if v > u32::MAX as u64 {
                        return Err(format!("{name} does not fit in u32"));
                    }
                    *x = v as u32;
                    f.present = true;
                    return Ok(());
                }
                other => {
                    return Err(format!("{name} is not a u32/u64 scalar: {other:?}"));
                }
            }
        }
    }
    Err(format!("field {name} not found"))
}

fn patch_list_count_header(
    header_bytes: &mut [u8],
    header_variant: &str,
    new_count: u32,
) -> Result<(), String> {
    if header_variant == "marker_run_plus_zeros" {
        let len = header_bytes.len();
        if len < 17 {
            return Err("marker_run_plus_zeros header too small".into());
        }
        let off = len - 17;
        header_bytes[off..off + 4].copy_from_slice(&new_count.to_le_bytes());
        return Ok(());
    }
    let (fixed_size, count_offset, kind) = match header_variant {
        "zero1_count_u24" => (18usize, 1usize, 0u8),
        "zero4_count_u32" => (18, 4, 1),
        "ones_then_count" => (21, 4, 1),
        "one_count_u16be" => (19, 1, 2),
        other => {
            return Err(format!(
                "unsupported object_list header_variant '{other}'"
            ))
        }
    };
    if header_bytes.len() < fixed_size {
        return Err("list header shorter than fixed size".into());
    }
    let pad_len = header_bytes.len() - fixed_size;
    let off = pad_len + count_offset;
    match kind {
        0 => {
            if new_count > 0xFF_FFFF {
                return Err("count exceeds u24".into());
            }
            header_bytes[off] = (new_count & 0xFF) as u8;
            header_bytes[off + 1] = ((new_count >> 8) & 0xFF) as u8;
            header_bytes[off + 2] = ((new_count >> 16) & 0xFF) as u8;
        }
        1 => header_bytes[off..off + 4].copy_from_slice(&new_count.to_le_bytes()),
        2 => {
            if new_count > 0xFFFF {
                return Err("count exceeds u16".into());
            }
            header_bytes[off..off + 2].copy_from_slice(&(new_count as u16).to_be_bytes());
        }
        _ => unreachable!(),
    }
    Ok(())
}

struct FpDiff {
    added: Vec<String>,
    removed: Vec<String>,
    changed: Vec<(String, String, String)>,
}

impl FpDiff {
    fn to_json(&self) -> Value {
        json!({
            "added": self.added,
            "removed": self.removed,
            "changed": self.changed.iter().map(|(k,a,b)| json!({"path": k, "before": a, "after": b})).collect::<Vec<_>>(),
        })
    }
}

fn fingerprint_diff(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
) -> FpDiff {
    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut changed = Vec::new();
    let keys: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    for k in keys {
        match (before.get(k), after.get(k)) {
            (None, Some(_)) => added.push(k.clone()),
            (Some(_), None) => removed.push(k.clone()),
            (Some(a), Some(b)) if a != b => changed.push((k.clone(), a.clone(), b.clone())),
            _ => {}
        }
    }
    added.sort();
    removed.sort();
    FpDiff {
        added,
        removed,
        changed,
    }
}

fn inventory_diff(before: &[InvRow], after: &[InvRow]) -> Value {
    fn key(r: &InvRow) -> (u32, u64) {
        (r.item_key, r.item_no)
    }
    let bset: BTreeSet<(u32, u64)> = before.iter().map(key).collect();
    let aset: BTreeSet<(u32, u64)> = after.iter().map(key).collect();
    let added: Vec<_> = aset.difference(&bset).cloned().collect();
    let removed: Vec<_> = bset.difference(&aset).cloned().collect();
    json!({
        "before_rows": before.len(),
        "after_rows": after.len(),
        "added_rows": added.len(),
        "removed_rows": removed.len(),
        "added_item_no_pairs": added.iter().map(|(k,n)| json!({"item_key": k, "item_no": n})).collect::<Vec<_>>(),
        "removed_item_no_pairs": removed.iter().map(|(k,n)| json!({"item_key": k, "item_no": n})).collect::<Vec<_>>(),
    })
}

fn cmd_diff(m: &BTreeMap<String, String>) -> Result<(), String> {
    let before_p = PathBuf::from(require(m, "before")?);
    let after_p = PathBuf::from(require(m, "after")?);
    let out = ensure_out(require(m, "out")?)?;
    let before = load_save(&before_p)?;
    let after = load_save(&after_p)?;
    let (fp_b, n_b, map_b) = fingerprint(&before.blocks);
    let (fp_a, n_a, map_a) = fingerprint(&after.blocks);
    let diff = fingerprint_diff(&map_b, &map_a);
    let inv = inventory_diff(
        &collect_inventory(&before.blocks),
        &collect_inventory(&after.blocks),
    );
    let report = json!({
        "command": "diff",
        "before": { "path": before_p.display().to_string(), "sha256": sha256_file(&before_p)?, "fp": fp_b, "entries": n_b, "hmac_ok": before.save.hmac_ok },
        "after": { "path": after_p.display().to_string(), "sha256": sha256_file(&after_p)?, "fp": fp_a, "entries": n_a, "hmac_ok": after.save.hmac_ok },
        "fingerprint_diff_counts": {
            "added": diff.added.len(),
            "removed": diff.removed.len(),
            "changed": diff.changed.len(),
        },
        "fingerprint_diff": diff.to_json(),
        "inventory_diff": inv,
        "undecoded": {
            "before": undecoded_total(&before.blocks),
            "after": undecoded_total(&after.blocks),
        },
    });
    write_json(&out.join("diff.json"), &report)?;
    println!(
        "diff fp_added={} fp_removed={} fp_changed={} inv_added={} inv_removed={}",
        diff.added.len(),
        diff.removed.len(),
        diff.changed.len(),
        inv["added_rows"],
        inv["removed_rows"]
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transferred_formula_matches_editor() {
        // Beer 22007 → 0x55F7_0101 from MainWindowViewModel comments.
        let beer = 22007u32;
        assert_eq!(((beer & 0xFFFF) << 16) | TRANSFERRED_LOW, 0x55F7_0101);
        let report = 1000677u32;
        assert_eq!(((report & 0xFFFF) << 16) | TRANSFERRED_LOW, 0x44E5_0101);
        // GoldBar ItemKey 53 from 2.04 iteminfo (not assumed by add-item).
        assert_eq!(((53u32 & 0xFFFF) << 16) | TRANSFERRED_LOW, 0x0035_0101);
    }

    #[test]
    fn list_count_u24_patches() {
        let mut h = vec![0u8; 18];
        h[1] = 5;
        patch_list_count_header(&mut h, "zero1_count_u24", 6).unwrap();
        assert_eq!(&h[1..4], &[6, 0, 0]);
    }

    #[test]
    fn refuse_same_path() {
        let p = PathBuf::from("/tmp/cd-copy-save-test-same");
        assert!(refuse_same(&p, &p).is_err());
    }
}
