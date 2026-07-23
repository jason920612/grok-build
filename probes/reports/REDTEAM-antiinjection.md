# Red-team report: prompt-injection defense (`antiinjection`)

**Scope:** `crates/codegen/xai-grok-tools/src/antiinjection.rs`  
**Call sites:** `registry/types.rs` (`finalize_output` → `defend_output`), blackboard digest (`sanitize_untrusted` on foreign entries)  
**Verification command:**

```bash
source ~/.cargo/env && cd /mnt/c/Users/jason/Desktop/redteam-wt \
  && CARGO_TARGET_DIR=~/grok-build-target PROTOC=/usr/bin/protoc \
     cargo test -p xai-grok-tools --lib antiinjection
```

**Result after fixes:** `14 passed; 0 failed` (6 pre-existing + 8 new regression tests).

---

## Summary

The original defense used case-insensitive exact-substring matching of trusted harness markers, then inserted a single U+200B (ZWSP) after the first character. Provenance fencing wrapped network tool output with a process nonce on `---8<---` delimiters only.

That design failed several concrete, testable attacks:

1. Invisible interrupters (ZWSP/ZWNJ/soft-hyphen/bidi/combining marks) skipped the substring scan.
2. Fullwidth / exotic-space confusables never matched ASCII markers.
3. ZWSP-only defanging reconstituted the exact marker after any format-char strip.
4. Fence `source="..."` did not escape quotes; close tag `</untrusted-data>` had no nonce and was forgeable inside the payload.

All of the above are fixed with regression tests named below. Pre-existing tests remain green (protections were not weakened).

---

## Findings

### F1 — Format-character interruption of trusted markers

| | |
|---|---|
| **Bypass** | Insert ZWSP, ZWNJ, ZWJ, soft hyphen, word joiner, BOM, bidi overrides, or combining marks between characters of `system-reminder` / `[compass]` / etc. Exact `find` misses; many readers strip those code points and the real marker reappears. |
| **Pre-fix behavior** | `hits == 0`; marker intact after strip of format chars. |
| **Test (fail→pass)** | `bypass_zwsp_interrupted_marker_is_neutralized` |
| | `bypass_format_char_interrupted_markers_are_neutralized` |
| | `bypass_bidi_interrupted_compass_is_neutralized` |
| **Fix** | `fold_untrusted`: strip known smuggler / format / common combining ranges before matching. |

### F2 — Fullwidth / compatibility confusables

| | |
|---|---|
| **Bypass** | Fullwidth Latin `ｓｙｓｔｅｍ－ｒｅｍｉｎｄｅｒ` (U+FF53…) is visually and (under NFKC) textually `system-reminder`, but never matched ASCII `find`. |
| **Pre-fix behavior** | `hits == 0`. |
| **Test (fail→pass)** | `bypass_fullwidth_homoglyph_marker_is_neutralized` |
| **Fix** | `fold_confusable`: map fullwidth ASCII U+FF01..=U+FF5E → ASCII; map exotic Unicode spaces → U+0020. |

### F3 — NBSP / thin-space in `!! correction`

| | |
|---|---|
| **Bypass** | `!!\u{00a0}correction` (NBSP) did not match `!! correction`. |
| **Pre-fix behavior** | `hits == 0`. |
| **Test (fail→pass)** | `bypass_nbsp_correction_marker_is_neutralized` |
| **Fix** | Same space-folding as F2. |

### F4 — ZWSP neutralization reversible by format strip

| | |
|---|---|
| **Bypass** | After legitimate neutralization, `s\u{200b}ystem-reminder` becomes `system-reminder` if any pipeline or tokenizer strips ZWSP (Cf). The task explicitly asked whether ZWSP-only breaks are robust — they were not. |
| **Pre-fix behavior** | Marker reconstituted after `strip_format_chars`. |
| **Test (fail→pass)** | `bypass_zwsp_only_defang_not_reversible_by_format_strip` |
| **Fix** | Keep ZWSP (existing contract / legibility tests) **and** insert durable U+00B7 middle-dot after the first character. Middle-dot is not a format character; format-strip cannot reconstitute the marker. |

### F5 — Fence `source` attribute injection

| | |
|---|---|
| **Bypass** | `fence_untrusted_output(content, "web\"><evil")` interpolated `source` raw into `source="{source}"`, breaking out of the attribute / tag. |
| **Pre-fix behavior** | Raw `"` and `>` appeared in the open tag. |
| **Test (fail→pass)** | `fence_source_attribute_is_escaped` |
| **Fix** | `escape_attr`: `&`, `"`, `<`, `>` → entities; drop other C0 controls (keep tab). |

### F6 — Fence early-close via bare `</untrusted-data>`

| | |
|---|---|
| **Bypass** | Payload contained `</untrusted-data>`. The real closer was the same fixed string with **no nonce**, so the model could see an early “end of untrusted data” while the nonce only protected `---8<---` lines. |
| **Pre-fix behavior** | First `</untrusted-data>` appeared inside the payload region. |
| **Test (fail→pass)** | `bypass_fence_early_close_tag_is_defanged` |
| **Fix** | (1) Real closer is `</untrusted-data-{nonce}>`. (2) `neutralize_fence_lookalikes` HTML-escapes `<untrusted-data` / `</untrusted-data` inside the payload before wrapping. |

---

## Fix design (post-change pipeline)

```
untrusted tool / board text
        │
        ▼
 fold_untrusted  ── strip smugglers, fold fullwidth + exotic spaces
        │
        ▼
 marker scan (case-insensitive TRUSTED_MARKERS)
        │
        ▼
 defang: first_char + ZWSP + middle-dot (·)
        │
        ▼
 [if WebFetch|WebSearch]
   neutralize fence lookalikes in body
   wrap with nonced open/---8<---/close + escaped source=
```

---

## Residual gaps (honest, not closed)

These were considered and either out of scope for structural marker defense or not closed without larger product tradeoffs:

1. **Script / homoglyph confusables beyond fullwidth ASCII**  
   Cyrillic `с`/`а`/`е` lookalikes do not NFKC-collapse to Latin and are not folded. A full confusables database would be needed; not added (no `unicode-normalization` / confusable tables in this crate). Visual impersonation remains partially possible.

2. **Newline / whitespace-split markers**  
   `sys\ntem-reminder` and `system- reminder` are not joined. Aggressive whitespace-collapse would damage legitimate multi-line file content. Not folded.

3. **Local reads still sanitized but not fenced**  
   By design (`kind_is_external` only `WebFetch|WebSearch`). Attacker-authored files that the model `read_file`s get marker neutralization only — no provenance fence or “this is data” anchor. Documented product choice; residual for threat (b).

4. **Semantic instruction injection without markers**  
   Plain “ignore previous instructions and …” is not a trusted marker. Fencing helps for network tools; marker sanitization does not address free-form social engineering.

5. **Fold side effects on legitimate untrusted text**  
   Stripping bidi marks / combining characters and mapping fullwidth ASCII rewrites some legitimate international text in tool returns. Accepted for the untrusted channel; could surprise CJK fullwidth punctuation in fetched pages.

6. **Nonce not cryptographic**  
   `fence_nonce` is a `DefaultHasher` of pid + address + time. Fine against external web content that cannot observe process state; not a secret against a co-resident attacker who can read process memory or prior tool transcripts in-session.

7. **Unknown tool kinds skip defense**  
   `finalize_output` uses raw text when `kind` lookup returns `None`. Not changed here (call-site policy).

---

## Tests inventory

| Test | Role |
|------|------|
| `neutralizes_forged_system_reminder` | Pre-existing — still passes |
| `neutralizes_forged_compass_and_correction` | Pre-existing — still passes |
| `clean_content_is_untouched` | Pre-existing — still passes |
| `fence_is_unforgeable_and_anchored` | Pre-existing — still passes |
| `only_network_kinds_are_fenced` | Pre-existing — still passes |
| `read_output_is_sanitized_but_not_fenced` | Pre-existing — still passes |
| `bypass_zwsp_interrupted_marker_is_neutralized` | F1 |
| `bypass_format_char_interrupted_markers_are_neutralized` | F1 |
| `bypass_bidi_interrupted_compass_is_neutralized` | F1 |
| `bypass_fullwidth_homoglyph_marker_is_neutralized` | F2 |
| `bypass_nbsp_correction_marker_is_neutralized` | F3 |
| `bypass_zwsp_only_defang_not_reversible_by_format_strip` | F4 |
| `fence_source_attribute_is_escaped` | F5 |
| `bypass_fence_early_close_tag_is_defanged` | F6 |

Each `bypass_*` / fence-attr test encodes an attack that returned `hits == 0` or reconstituted a marker / broke fence structure against the original implementation (confirmed via independent Python reimplementation of the pre-fix logic before landing the Rust fix).
