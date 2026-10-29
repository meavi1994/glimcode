# Codex UTF-8 output fix

`codex-0.165.0-utf8-output.patch` applies to OpenAI Codex tag `rust-v0.165.0` (commit `360319d`). It fixes output decoding inside Codex, independently of Glimcode's persistence backend.

Codex caps captured shell output at a byte limit. If that limit bisects the final UTF-8 character, the decoder can reinterpret the entire otherwise-valid UTF-8 output as a legacy code page. A large directory tree reproduces this as `в”...` instead of box-drawing branches.

The patch preserves UTF-8 when the only error is an incomplete final character and the preceding text contains valid multibyte UTF-8. Ambiguous ASCII plus legacy-encoding output still follows the existing encoding detector. The existing CP1251, CP866, Latin-1 and Windows-1252 tests pass alongside regressions for truncated trees at 64 KiB and 1 MiB, and partial accented, CJK and emoji characters (10 decoder tests in total).

The same-version patched CLI was built and installed locally. The original binary is retained beside it as `codex.before-utf8-output-fix`. Automatic Codex upgrades may replace this local patch. No upstream issue or PR has been published.

To rebuild from an official source checkout:

```sh
git checkout rust-v0.165.0
git apply /path/to/codex-0.165.0-utf8-output.patch
cd codex-rs
cargo test -p codex-protocol exec_output
cargo build --release -p codex-cli --bin codex
```
