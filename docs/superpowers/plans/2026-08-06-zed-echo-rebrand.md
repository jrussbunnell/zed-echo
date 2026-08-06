# Zed Echo Rebrand Implementation Plan (Phase 0)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn this fork into a side-by-side-installable macOS app called "Zed Echo" whose user data, bundle identity, and URL scheme never collide with the user's real Zed.

**Architecture:** Change the hardcoded `APP_NAME` constant that derives all user-data paths, rename the binary to satisfy the compile-time assertion that binds the two, override the Dev-channel identity strings, and update the `bundle-dev` metadata. The Dev release channel is reused because `poll_for_updates()` already returns `false` for it, so the fork can never auto-update itself into the real Zed.

**Tech Stack:** Rust 1.95.0, cargo, `script/bundle-mac`, cargo-bundle (Zed's fork), macOS.

## Global Constraints

- Rust toolchain is pinned by `rust-toolchain.toml` to `1.95.0`. Use `~/.cargo/bin/cargo`.
- Use `./script/clippy`, never `cargo clippy`.
- Avoid `unwrap()` / `expect()` in non-test code; propagate with `?`.
- Never silently discard errors with `let _ =` on fallible operations.
- Never create `mod.rs` files.
- Do not write comments that summarize code. Comments explain *why* only.
- `xcode-select -p` must resolve to `/Applications/Xcode-beta.app/Contents/Developer` for the Metal compiler. It already does on this machine.
- Run `rustup` invocations strictly serially; `~/.rustup` is fragile to concurrent use.
- **Exact strings, copied verbatim from the design:**
  - `APP_NAME` = `ZedEcho` (derives `APP_NAME_LOWERCASE` = `zedecho`)
  - Bundle identifier = `dev.jrb.ZedEcho`
  - Display name = `Zed Echo`
  - URL scheme = `zedecho`
  - Windows app identifier = `Zed-Echo`

## HAZARD — read before running anything

Until Task 1 is committed, the built binary writes to `~/Library/Application Support/Zed` and `~/.config/zed` — **the real editor's SQLite thread database**. Do not run the built binary before Task 1 lands.

If you need to run it earlier for any reason, `crates/paths/src/paths.rs` honors a `CUSTOM_DATA_DIR` override (see `config_dir()` / `data_dir()`); use that rather than risking the default path.

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `crates/paths/src/paths.rs` | Derives every user-data path from `APP_NAME` | `APP_NAME` constant; add test module |
| `crates/zed/Cargo.toml` | Package, binary, and macOS bundle metadata | `default-run`, `[[bin]] name`, `[package.metadata.bundle-dev]` |
| `crates/release_channel/src/lib.rs` | Per-channel display/bundle identity | Dev arms of `display_name()`, `app_id()`, `app_identifier()`; add test module |
| `crates/zed/resources/app-icon-dev.png` | 512×512 Dock/Finder icon | Replace |
| `crates/zed/resources/app-icon-dev@2x.png` | 1024×1024 Dock/Finder icon | Replace |

The package name stays `zed`. Only the **binary** name changes. This keeps `cargo build -p zed` and `script/bundle-mac`'s `--package zed` working, and keeps the rebase surface minimal.

---

### Task 1: Isolate user data paths

`crates/zed/src/main.rs:9` contains a hard compile-time assertion:

```rust
const _: () = assert!(
    paths::APP_NAME_LOWERCASE
        .as_bytes()
        .eq_ignore_ascii_case(env!("CARGO_BIN_NAME").as_bytes()),
    "paths::APP_NAME_LOWERCASE must match the binary name. \
     Forks: update APP_NAME in crates/paths/src/paths.rs when renaming the binary.",
);

```

Changing `APP_NAME` without renaming the binary **fails to compile**. The two changes must land in the same task.

**Files:**
- Modify: `crates/paths/src/paths.rs:18`
- Modify: `crates/zed/Cargo.toml:9` (`default-run`), `crates/zed/Cargo.toml:57` (`[[bin]] name`)
- Test: `crates/paths/src/paths.rs` (new `#[cfg(test)] mod tests` at end of file)

**Interfaces:**
- Consumes: nothing.
- Produces: `paths::APP_NAME == "ZedEcho"`, `paths::APP_NAME_LOWERCASE == "zedecho"`. Binary is named `zedecho`; the cargo **package** remains `zed`.

- [ ] **Step 1: Write the failing test**

Append to the end of `crates/paths/src/paths.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_name_is_forked() {
        assert_eq!(APP_NAME, "ZedEcho");
        assert_eq!(APP_NAME_LOWERCASE, "zedecho");
    }

    #[test]
    fn user_data_paths_do_not_collide_with_upstream_zed() {
        let config = config_dir();
        let data = data_dir();
        assert!(
            config.ends_with("zedecho"),
            "config_dir must not be shared with the real Zed, got {config:?}"
        );
        assert!(
            data.ends_with("ZedEcho"),
            "data_dir must not be shared with the real Zed, got {data:?}"
        );
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `~/.cargo/bin/cargo test -p paths`

Expected: FAIL — `assertion \`left == right\` failed: left: "Zed", right: "ZedEcho"`.

- [ ] **Step 3: Change `APP_NAME`**

In `crates/paths/src/paths.rs:18`, replace:

```rust
pub const APP_NAME: &str = "Zed";
```

with:

```rust
pub const APP_NAME: &str = "ZedEcho";
```

Leave the doc comment above it as-is — upstream already documents that forks should change this.

- [ ] **Step 4: Rename the binary to satisfy the compile-time assertion**

In `crates/zed/Cargo.toml:9`, replace `default-run = "zed"` with:

```toml
default-run = "zedecho"
```

In `crates/zed/Cargo.toml:57`, replace the first `[[bin]]` block's name:

```toml
[[bin]]
name = "zedecho"
path = "src/main.rs"
```

Do **not** change `name = "zed"` on line 4 (`[package]`) — the package name must stay `zed`.

- [ ] **Step 5: Run the test to verify it passes**

Run: `~/.cargo/bin/cargo test -p paths`

Expected: PASS, both tests.

- [ ] **Step 6: Verify the whole binary still compiles**

Run: `~/.cargo/bin/cargo build -p zed`

Expected: SUCCESS. If it fails on the `const _: () = assert!` in `crates/zed/src/main.rs:9`, the `[[bin]] name` in Step 4 does not match `zedecho`.

Confirm the output binary path is `target/debug/zedecho`:

Run: `ls -la target/debug/zedecho`

Expected: the file exists.

- [ ] **Step 7: Commit**

```bash
git add crates/paths/src/paths.rs crates/zed/Cargo.toml
git commit -m "Isolate Zed Echo user data from upstream Zed"
```

---

### Task 2: Override Dev release channel identity

**Files:**
- Modify: `crates/release_channel/src/lib.rs:45` (`app_identifier()`, Windows-only), `:206` (`display_name()`), `:228` (`app_id()`)
- Test: `crates/release_channel/src/lib.rs` (new `#[cfg(test)] mod tests` at end of file)

**Interfaces:**
- Consumes: nothing from Task 1.
- Produces: `ReleaseChannel::Dev.display_name() == "Zed Echo"`, `ReleaseChannel::Dev.app_id() == "dev.jrb.ZedEcho"`. Task 3 must use the same `app_id` string in the bundle identifier — macOS requires them to match.

Note: `app_identifier()` is `#[cfg(target_os = "windows")]`, so it does not compile on this machine. Change it anyway for correctness; it is not testable here.

- [ ] **Step 1: Write the failing test**

Append to the end of `crates/release_channel/src/lib.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_channel_is_zed_echo() {
        assert_eq!(ReleaseChannel::Dev.display_name(), "Zed Echo");
        assert_eq!(ReleaseChannel::Dev.app_id(), "dev.jrb.ZedEcho");
    }

    #[test]
    fn dev_channel_never_self_updates() {
        assert!(!ReleaseChannel::Dev.poll_for_updates());
    }

    #[test]
    fn other_channels_are_untouched() {
        assert_eq!(ReleaseChannel::Stable.display_name(), "Zed");
        assert_eq!(ReleaseChannel::Stable.app_id(), "dev.zed.Zed");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `~/.cargo/bin/cargo test -p release_channel`

Expected: FAIL — `dev_channel_is_zed_echo`, left `"Zed Dev"`, right `"Zed Echo"`.

- [ ] **Step 3: Change the Dev arms**

In `display_name()` (around line 206), change the Dev arm:

```rust
    pub fn display_name(&self) -> &'static str {
        match self {
            ReleaseChannel::Dev => "Zed Echo",
            ReleaseChannel::Nightly => "Zed Nightly",
            ReleaseChannel::Preview => "Zed Preview",
            ReleaseChannel::Stable => "Zed",
        }
    }
```

In `app_id()` (around line 228), change the Dev arm:

```rust
    pub fn app_id(&self) -> &'static str {
        match self {
            ReleaseChannel::Dev => "dev.jrb.ZedEcho",
            ReleaseChannel::Nightly => "dev.zed.Zed-Nightly",
            ReleaseChannel::Preview => "dev.zed.Zed-Preview",
            ReleaseChannel::Stable => "dev.zed.Zed",
        }
    }
```

In `app_identifier()` (around line 45), change the Dev arm:

```rust
#[cfg(target_os = "windows")]
pub fn app_identifier() -> &'static str {
    match *RELEASE_CHANNEL {
        ReleaseChannel::Dev => "Zed-Echo",
        ReleaseChannel::Nightly => "Zed-Editor-Nightly",
        ReleaseChannel::Preview => "Zed-Editor-Preview",
        ReleaseChannel::Stable => "Zed-Editor-Stable",
    }
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `~/.cargo/bin/cargo test -p release_channel`

Expected: PASS, all three tests.

- [ ] **Step 5: Commit**

```bash
git add crates/release_channel/src/lib.rs
git commit -m "Give the Dev release channel Zed Echo identity"
```

---

### Task 3: Claim a distinct bundle identity and URL scheme

All four upstream channels declare `osx_url_schemes = ["zed"]`. Left unchanged, the fork fights the real Zed for `zed://` in LaunchServices.

**Files:**
- Modify: `crates/zed/Cargo.toml:283-289` (`[package.metadata.bundle-dev]`)

**Interfaces:**
- Consumes: `ReleaseChannel::Dev.app_id()` from Task 2 — the `identifier` here must match it exactly.
- Produces: an `.app` bundle named `Zed Echo.app` with identifier `dev.jrb.ZedEcho` and URL scheme `zedecho://`.

- [ ] **Step 1: Change the bundle metadata**

In `crates/zed/Cargo.toml`, replace the `[package.metadata.bundle-dev]` block:

```toml
[package.metadata.bundle-dev]
icon = ["resources/app-icon-dev@2x.png", "resources/app-icon-dev.png"]
identifier = "dev.jrb.ZedEcho"
name = "Zed Echo"
osx_minimum_system_version = "10.15.7"
osx_info_plist_exts = ["resources/info/*"]
osx_url_schemes = ["zedecho"]
```

Leave `bundle-nightly`, `bundle-preview`, and the stable `bundle` blocks untouched.

- [ ] **Step 2: Verify the manifest parses and nothing else changed**

Run: `~/.cargo/bin/cargo metadata --no-deps --format-version 1 --manifest-path crates/zed/Cargo.toml > /dev/null && echo "manifest OK"`

Expected: `manifest OK`.

Run: `git diff --stat crates/zed/Cargo.toml`

Expected: exactly one file changed, 3 insertions and 3 deletions (identifier, name, osx_url_schemes).

- [ ] **Step 3: Verify identifier and app_id agree**

Run:

```bash
grep -n 'identifier = "dev.jrb.ZedEcho"' crates/zed/Cargo.toml && \
grep -n '"dev.jrb.ZedEcho"' crates/release_channel/src/lib.rs && \
echo "identifiers match"
```

Expected: `identifiers match`. macOS requires the bundle identifier and `app_id()` to be identical; a mismatch produces an app that launches but misbehaves on relaunch and URL handling.

- [ ] **Step 4: Commit**

```bash
git add crates/zed/Cargo.toml
git commit -m "Claim the dev.jrb.ZedEcho bundle identity and zedecho URL scheme"
```

---

### Task 4: Give Zed Echo a distinct icon

The Dock must be unambiguous when Zed and Zed Echo are both running. The existing dev icon is a 512×512 / 1024×1024 RGBA PNG pair.

This task needs ImageMagick, which is **not currently installed** on this machine.

**Files:**
- Modify: `crates/zed/resources/app-icon-dev.png` (512×512)
- Modify: `crates/zed/resources/app-icon-dev@2x.png` (1024×1024)

**Interfaces:**
- Consumes: nothing.
- Produces: recolored icons at the same paths and dimensions. `bundle-dev`'s `icon` array already points at these filenames, so no manifest change is needed.

- [ ] **Step 1: Back up the originals**

```bash
cp crates/zed/resources/app-icon-dev.png /tmp/app-icon-dev.png.orig
cp crates/zed/resources/app-icon-dev@2x.png /tmp/app-icon-dev@2x.png.orig
```

- [ ] **Step 2: Install ImageMagick**

Run: `brew install imagemagick`

Expected: installs, and `magick --version` prints a version.

If you prefer not to install it, skip to Step 3b.

- [ ] **Step 3a: Hue-rotate the icons**

`-modulate brightness,saturation,hue` with hue `200` rotates roughly 180° — enough to be unmistakable at Dock size.

```bash
magick crates/zed/resources/app-icon-dev.png \
  -modulate 100,120,200 crates/zed/resources/app-icon-dev.png
magick crates/zed/resources/app-icon-dev@2x.png \
  -modulate 100,120,200 crates/zed/resources/app-icon-dev@2x.png
```

- [ ] **Step 3b: (Alternative) Drop in your own artwork**

If you have your own icon, export it at exactly 512×512 and 1024×1024 RGBA PNG and overwrite both files. Any square RGBA PNG at those dimensions works.

- [ ] **Step 4: Verify dimensions and format survived**

Run: `file crates/zed/resources/app-icon-dev.png crates/zed/resources/app-icon-dev@2x.png`

Expected:
```
crates/zed/resources/app-icon-dev.png:    PNG image data, 512 x 512, 8-bit/color RGBA, non-interlaced
crates/zed/resources/app-icon-dev@2x.png: PNG image data, 1024 x 1024, 8-bit/color RGBA, non-interlaced
```

If the dimensions or color type changed, restore from `/tmp/*.orig` and retry — cargo-bundle rejects non-square or non-RGBA input.

- [ ] **Step 5: Confirm the images actually differ from upstream**

Run: `git diff --stat crates/zed/resources/`

Expected: both PNGs listed as modified. If neither changed, the recolor silently no-opped; redo Step 3.

- [ ] **Step 6: Commit**

```bash
git add crates/zed/resources/app-icon-dev.png crates/zed/resources/app-icon-dev@2x.png
git commit -m "Add a distinct Zed Echo app icon"
```

---

### Task 5: Bundle, install, and verify isolation

This is the task that proves Phase 0 worked. It is the first point at which running the binary is safe.

`crates/zed/RELEASE_CHANNEL` already contains `dev`, so `script/bundle-mac` picks up `[package.metadata.bundle-dev]` with no extra flags.

**Files:**
- No source changes. This task verifies Tasks 1–4 and sets up the shared-settings symlinks.

**Interfaces:**
- Consumes: everything from Tasks 1–4.
- Produces: `/Applications/Zed Echo.app`, and `~/.config/zedecho/{settings.json,keymap.json}` symlinked to the real Zed's copies.

- [ ] **Step 1: Confirm the release channel is `dev`**

Run: `cat crates/zed/RELEASE_CHANNEL`

Expected: `dev`. If it says anything else, `bundle-mac` will read the wrong `[package.metadata.bundle-*]` block and the rebrand will not apply. Stop and fix before continuing.

- [ ] **Step 2: Build and install the bundle**

Run: `./script/bundle-mac -d -i`

`-d` builds in debug (much faster; a release build is not needed to validate identity). `-i` installs into `/Applications`.

Expected: the script completes and `/Applications/Zed Echo.app` exists.

Note: `script/bundle-mac` hardcodes `Contents/MacOS/zed` at lines 206–207 for codesigning. Those lines are guarded by `can_code_sign`, which is false without the Zed Industries certificate, so they are skipped on this machine. If the script errors referencing `Contents/MacOS/zed`, the binary rename from Task 1 is the cause — change those paths to `zedecho`.

- [ ] **Step 3: Verify bundle identity**

```bash
/usr/libexec/PlistBuddy -c "Print :CFBundleIdentifier" "/Applications/Zed Echo.app/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Print :CFBundleName" "/Applications/Zed Echo.app/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Print :CFBundleURLTypes:0:CFBundleURLSchemes:0" "/Applications/Zed Echo.app/Contents/Info.plist"
```

Expected, in order:
```
dev.jrb.ZedEcho
Zed Echo
zedecho
```

- [ ] **Step 4: Verify the real Zed is untouched**

```bash
ls -d /Applications/Zed.app 2>/dev/null && echo "real Zed still present"
mdls -name kMDItemCFBundleIdentifier "/Applications/Zed.app" 2>/dev/null
```

Expected: the real Zed is still present with identifier `dev.zed.Zed`. If `/Applications/Zed.app` is missing, it was never installed there — that is fine, but confirm before proceeding.

- [ ] **Step 5: Symlink settings and keymap, keep state separate**

Settings are shared so theme, keybindings, and `agent_servers` stay in sync. The database, workspace state, and thread history stay separate because they live under `data_dir()`, which Task 1 already isolated.

```bash
mkdir -p ~/.config/zedecho
ln -sfn ~/.config/zed/settings.json ~/.config/zedecho/settings.json
ln -sfn ~/.config/zed/keymap.json ~/.config/zedecho/keymap.json
ls -la ~/.config/zedecho/
```

Expected: both entries shown as symlinks pointing into `~/.config/zed/`.

If `~/.config/zed/` does not exist on this machine, find the real config directory first with `ls -d ~/Library/Application\ Support/Zed ~/.config/zed 2>/dev/null` and adjust the link targets. Do not create empty files — a broken symlink is more obvious than a silently empty config.

- [ ] **Step 6: Launch and confirm data isolation**

Run: `open "/Applications/Zed Echo.app"`

Then, with the app running:

```bash
ls -d ~/Library/Application\ Support/ZedEcho && echo "isolated data dir created"
```

Expected: `isolated data dir created`.

- [ ] **Step 7: Confirm the real Zed's database was not written**

```bash
ls -la ~/Library/Application\ Support/Zed/db 2>/dev/null | head
```

Compare the modification timestamps against when you launched Zed Echo. Nothing under the real Zed's `db/` should have been touched. **If anything was modified, stop — Task 1 did not take effect, and continuing risks the user's thread history.**

- [ ] **Step 8: Confirm settings came through the symlink**

In the running Zed Echo, confirm your usual theme and keybindings are active. This proves Step 5's symlinks resolve.

- [ ] **Step 9: Install the CLI as `zedecho` without clobbering `zed`**

Zed's `cli` crate builds a binary literally named `cli`, which the bundle installs into `Zed Echo.app/Contents/MacOS/cli`. Zed's own "install CLI" command symlinks that to `/usr/local/bin/zed` — which **would overwrite the real Zed's CLI**. Do not use that command from inside Zed Echo.

Link it by hand under a distinct name instead:

```bash
ls -la /usr/local/bin/zed 2>/dev/null && echo "real Zed CLI present — do not overwrite"
ln -sfn "/Applications/Zed Echo.app/Contents/MacOS/cli" /usr/local/bin/zedecho
```

Verify both exist and point at different apps:

```bash
ls -la /usr/local/bin/zed /usr/local/bin/zedecho 2>/dev/null
```

Expected: `zedecho` resolves into `Zed Echo.app`; `zed`, if it exists, still resolves into the real `Zed.app`. If `zed` now points at `Zed Echo.app`, the in-app install command was used — repoint it at the real Zed before continuing.

- [ ] **Step 10: Commit**

No source files changed in this task. If Step 2 required editing `script/bundle-mac` for the binary rename:

```bash
git add script/bundle-mac
git commit -m "Point the macOS bundle script at the renamed zedecho binary"
```

Otherwise there is nothing to commit; note in the handoff that Phase 0 is verified installed.

---

## Done criteria

- `/Applications/Zed Echo.app` launches with a distinct Dock icon.
- Its bundle identifier is `dev.jrb.ZedEcho` and it owns `zedecho://`, not `zed://`.
- It reads settings and keymap from the real Zed via symlink.
- It writes threads, database, and workspace state to `~/Library/Application Support/ZedEcho`, and the real Zed's data directory is untouched.
- `/usr/local/bin/zedecho` exists and `/usr/local/bin/zed` still points at the real Zed.
- `~/.cargo/bin/cargo test -p paths -p release_channel` passes.

Phase 1 (`docs/superpowers/plans/2026-08-06-agent-panel-read-aloud.md`) may begin once all of the above hold.
