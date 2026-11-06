# cd-copy-save

Minimal Linux CLI for **offline copies** of Crimson Desert `.save` files.
Built against vendored `bbfox0703/crimson-rs` `8e84704` (parser target **2.04**).

It does **not** guess GoldBar = 53. Item names live in `iteminfo`, not in the save.

## What it does

| Command | Writes input? | Purpose |
| --- | --- | --- |
| `inspect` | no | HMAC, version/flags, decode stats, scalar fingerprint |
| `inventory` | no | Structured inventory JSON (`ItemKey` + stacks + containers) |
| `roundtrip` | no (optional `--write-copy`) | Body decode→encode identity + reseal with original nonce |
| `add-item` | **never** — requires `--out-save` ≠ `--save` | Clone one **already-present** item key onto a copy |
| `diff` | no | Semantic fingerprint + inventory compare of two copies |

`add-item` will **refuse** if `--item-key` is not already in the save. Cloning a different item's mask into a GoldBar (or any other) key is how the engine CTDs; this CLI will not do it.

## Build (Linux)

Prerequisites: Rust 1.85+ (edition 2024), git, Python 3 for the two-line vendor patch.

```bash
# from the CrimsonAtomtic repo root
./tools/copy_save_cli/build.sh
# binary: tools/copy_save_cli/target/release/cd-copy-save
```

`build.sh` clones `vendor/crimson-rs` (gitignored) at the pinned commit and applies two **local-only** link patches (`rlib` crate-type, `pub mod save`). It does not push anywhere.

## Usage

```bash
cd-copy-save inspect   --save slot1/save.save --out /tmp/out-inspect
cd-copy-save inventory --save slot1/save.save --out /tmp/out-inv
cd-copy-save roundtrip --save slot1/save.save --out /tmp/out-rt --write-copy /tmp/slot1-roundtrip.save
cd-copy-save add-item  --save slot1/save.save --out-save /tmp/slot1-plus-one.save \
                       --out /tmp/out-add --item-key <CONFIRMED>
cd-copy-save diff      --before slot1/save.save --after /tmp/slot1-plus-one.save --out /tmp/out-diff
```

No-args / `--help` prints usage and exits 2.

## Confirming GoldBar

Committed data in crimson-rs (`data/keys.txt`) is **numeric ItemKey only**.
`string_key` `GoldBar` is in the game's `iteminfo` table. To confirm:

1. From a 2.04.xx install, extract `0008` `iteminfo.staticinfobody` (and DLC `iteminfo.dlc_01` if loaded).
2. Parse with crimson-rs `parse_iteminfo_from_bytes` and select rows whose `string_key` is exactly `GoldBar`.
3. If that key is **already in the save**, `add-item --item-key <that key>` is the safe clone.
4. If the save does **not** hold that key, do not retarget some other row. Need a same-mask currency donor (the GUI add-item flow) or a save that already has a GoldBar.

The old public CE table listing `53 = GoldBar` is **not** used here. This repo pins `2200 = Pyeonjeon_Arrow`; it does not pin GoldBar.

## Crypto / keys

Save files are `SAVE` v2 / flags `0x0080`, ChaCha20 + HMAC-SHA256 + LZ4.
The key is **version-derived inside crimson-rs**, not per-account. No Steam ID, no game install, and no extra secret is required to read or reseal a `.save`.

## Privacy

Do not commit `save.save` / `lobby.save` or inventory dumps. Root `.gitignore` already ignores `**/save.save` and `**/lobby.save`.
