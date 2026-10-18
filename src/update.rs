//! Self-update.
//!
//! Blocking helpers — call on the background pool.
//!
//! The latest version comes from a manifest on downloads.omnipty.com
//! (see RELEASING.md) with one entry per platform build.
//!
//! macOS: the entry points at a DMG in the same bucket. The DMG must carry a
//! minisign signature that verifies against the public key compiled in
//! (`update.pub`) before it is kept; the install step then swaps the bundle
//! from a detached shell after the app quits, and relaunches.
//!
//! Linux: there is no in-place install (packages come from the AUR or a
//! tarball), so a release is only announced, and only once the manifest has a
//! `linux-<arch>` entry: the pill opens the release page.

#[cfg(any(target_os = "macos", test))]
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

/// Where the updater looks for the latest release. `scripts/release.sh`
/// publishes it after the DMG it points at; `release-linux.sh` adds the
/// Linux entry after the tarball is up.
const MANIFEST_URL: &str = "https://downloads.omnipty.com/releases/stable.json";

/// `OMNIPTY_MANIFEST_URL` overrides the manifest, so an upgrade can be
/// rehearsed against a staging copy before the real one is published.
fn manifest_url() -> String {
    std::env::var("OMNIPTY_MANIFEST_URL").unwrap_or_else(|_| MANIFEST_URL.into())
}

/// The minisign public key updates must be signed with (`minisign -G`). While
/// the file holds the placeholder text, macOS updates are refused.
#[cfg(any(target_os = "macos", test))]
const UPDATE_PUBLIC_KEY: &str = include_str!("../update.pub");

pub struct ReleaseInfo {
    pub version: String,
    /// macOS: the DMG to download. Linux: the release page to open.
    pub url: String,
    /// macOS: the contents of the DMG's `.minisig` file. Linux: None.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub signature: Option<String>,
}

fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.trim().trim_start_matches('v');
    let mut parts = v.splitn(3, '.').map(|p| {
        p.chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse::<u64>()
            .ok()
    });
    Some((parts.next()??, parts.next()??, parts.next()??))
}

pub fn is_newer(remote: &str, local: &str) -> bool {
    match (parse_version(remote), parse_version(local)) {
        (Some(r), Some(l)) => r > l,
        _ => false,
    }
}

/// Query the latest release. Returns None when the manifest has no entry for
/// this platform yet: on Linux the tarball is built on a separate machine
/// after the macOS release, so a release only counts once it's up.
pub fn fetch_latest() -> Result<Option<ReleaseInfo>, String> {
    let out = Command::new("curl")
        .args([
            "-sSL",
            "--max-time",
            "20",
            "-w",
            "\n%{http_code}",
            "-A",
            concat!("omnipty/", env!("CARGO_PKG_VERSION")),
            &manifest_url(),
        ])
        .output()
        .map_err(|e| format!("update check failed: {e}"))?;
    if !out.status.success() {
        return Err("update check failed: network unreachable".into());
    }
    let (status, body) = split_status(&out.stdout).ok_or("update check failed: bad response")?;
    match status {
        200 => {}
        // No manifest published yet — not an error.
        404 => return Ok(None),
        // The manifest is ours: anything else is a release or hosting
        // problem, not the user's network.
        s => {
            crate::telemetry::report("update manifest HTTP error", s);
            return Err(format!("update check failed: server said {s}"));
        }
    }
    let json: serde_json::Value = serde_json::from_slice(body).map_err(|e| {
        crate::telemetry::report("update manifest unreadable", &e);
        format!("update check failed: {e}")
    })?;
    Ok(parse_manifest(
        &json,
        std::env::consts::OS,
        std::env::consts::ARCH,
    ))
}

/// Split curl's `-w '\n%{http_code}'` trailer off the body.
fn split_status(out: &[u8]) -> Option<(u16, &[u8])> {
    let cut = out.iter().rposition(|&b| b == b'\n')?;
    let status = std::str::from_utf8(&out[cut + 1..])
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some((status, &out[..cut]))
}

/// The release described by the manifest, for this platform (`os` and `arch`
/// as Rust's `std::env::consts` spell them; the manifest keys match). None
/// when the manifest has no build for it, or is malformed: nothing to offer
/// either way. Only https URLs are accepted; on macOS the signature check is
/// what actually vouches for the file, but there's no reason to fetch it over
/// plain http.
fn parse_manifest(manifest: &serde_json::Value, os: &str, arch: &str) -> Option<ReleaseInfo> {
    let version = manifest["version"].as_str()?.trim_start_matches('v');
    parse_version(version)?;
    fn https(v: &serde_json::Value) -> Option<&str> {
        v.as_str().filter(|u| u.starts_with("https://"))
    }
    let asset = &manifest["assets"][format!("{os}-{arch}")];
    if os == "macos" {
        let url = https(&asset["url"])?;
        let signature = asset["signature"].as_str()?;
        Some(ReleaseInfo {
            version: version.to_string(),
            url: url.to_string(),
            signature: Some(signature.to_string()),
        })
    } else {
        // Announce only: the entry's presence says the build exists; the pill
        // opens the release page rather than the tarball.
        https(&asset["url"])?;
        let page = https(&manifest["release_url"])?;
        Some(ReleaseInfo {
            version: version.to_string(),
            url: page.to_string(),
            signature: None,
        })
    }
}

/// What `release.sh` writes as the signature's trusted comment. It's covered
/// by the signature, so a validly signed DMG can't be passed off as another
/// version or architecture.
#[cfg(any(target_os = "macos", test))]
fn signed_comment(version: &str, arch: &str) -> String {
    format!("omnipty {version} macos-{arch}")
}

/// Check `path` against a minisign `signature` made with `public_key`, and
/// that the signature was issued for exactly `expected_comment`.
#[cfg(any(target_os = "macos", test))]
fn verify_signature(
    path: &Path,
    public_key: &str,
    signature: &str,
    expected_comment: &str,
) -> Result<(), String> {
    use minisign_verify::{PublicKey, Signature};
    use std::io::Read;

    let key = PublicKey::decode(public_key)
        .map_err(|_| "updates are disabled: this build has no update signing key".to_string())?;
    let sig = Signature::decode(signature)
        .map_err(|_| "update rejected: unreadable signature".to_string())?;
    let mut verifier = key
        .verify_stream(&sig)
        .map_err(|_| "update rejected: signed by a different key".to_string())?;
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("update verification failed: {e}"))?;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("update verification failed: {e}"))?;
        if n == 0 {
            break;
        }
        verifier.update(&buf[..n]);
    }
    verifier
        .finalize()
        .map_err(|_| "update rejected: signature doesn't match the download".to_string())?;
    // Checked after the signature, so the comment is known to be authentic.
    if sig.trusted_comment() != expected_comment {
        return Err("update rejected: signed for a different version".into());
    }
    Ok(())
}

/// The version that ran last time, when this binary is newer than it — i.e.
/// the first launch after an update. Records the current version either way,
/// so a second window opened at startup sees nothing.
pub fn note_launch_version() -> Option<String> {
    let path = crate::paths::cache_dir().join("last_version.txt");
    let current = env!("CARGO_PKG_VERSION");
    let previous = std::fs::read_to_string(&path)
        .ok()
        .map(|s| s.trim().to_string());
    if previous.as_deref() != Some(current) {
        let _ = std::fs::create_dir_all(path.parent()?);
        let _ = std::fs::write(&path, current);
    }
    updated_from(previous.as_deref(), current)
}

fn updated_from(previous: Option<&str>, current: &str) -> Option<String> {
    previous
        .filter(|p| is_newer(current, p))
        .map(str::to_string)
}

#[cfg(target_os = "macos")]
fn updates_dir() -> Option<PathBuf> {
    let dir = crate::paths::cache_dir().join("updates");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Download the DMG and keep it only if its signature verifies. A copy
/// already in the cache is checked the same way (it may predate signing, or
/// have been damaged) and fetched again if it fails.
#[cfg(target_os = "macos")]
pub fn download(info: &ReleaseInfo) -> Result<PathBuf, String> {
    let Some(signature) = info.signature.as_deref() else {
        crate::telemetry::report("update signature rejected", "no signature in the manifest");
        return Err("update rejected: no signature".into());
    };
    let comment = signed_comment(&info.version, std::env::consts::ARCH);
    let verify = |path: &Path| verify_signature(path, UPDATE_PUBLIC_KEY, signature, &comment);

    let dir = updates_dir().ok_or("no cache directory")?;
    let dest = dir.join(format!("OmniPTY-{}.dmg", info.version));
    let partial = dir.join(format!("OmniPTY-{}.dmg.partial", info.version));
    if dest.exists() {
        if verify(&dest).is_ok() {
            return Ok(dest);
        }
        let _ = std::fs::remove_file(&dest);
    }
    let status = Command::new("curl")
        .args(["-fsSL", "--max-time", "600", "-o"])
        .arg(&partial)
        .arg(&info.url)
        .status()
        .map_err(|e| format!("download failed: {e}"))?;
    if !status.success() {
        let _ = std::fs::remove_file(&partial);
        // The check just succeeded, so this is most likely a missing or
        // misnamed DMG, not the user's network.
        crate::telemetry::report(
            "update download failed",
            format_args!("curl exit {:?}", status.code()),
        );
        return Err("update download failed".into());
    }
    // A fresh download that fails its signature is a broken release or a
    // tampered one. Either way, hear about it before users do.
    if let Err(e) = verify(&partial) {
        let _ = std::fs::remove_file(&partial);
        crate::telemetry::report("update signature rejected", &e);
        return Err(e);
    }
    std::fs::rename(&partial, &dest).map_err(|e| format!("download failed: {e}"))?;
    Ok(dest)
}

/// Called just before quitting into the installer script. The install runs
/// in a detached shell after OmniPTY quits, so its failures are invisible:
/// this leaves a marker for `check_install_attempt` at the next launch.
#[cfg(target_os = "macos")]
pub fn note_install_attempt(version: &str) {
    let _ = std::fs::write(
        crate::paths::cache_dir().join("pending_update.txt"),
        version,
    );
}

/// At launch: did the last install attempt take?
#[cfg(target_os = "macos")]
pub fn check_install_attempt() {
    let path = crate::paths::cache_dir().join("pending_update.txt");
    let Ok(target) = std::fs::read_to_string(&path) else {
        return;
    };
    let _ = std::fs::remove_file(&path);
    let current = env!("CARGO_PKG_VERSION");
    if target.trim() != current {
        crate::telemetry::report(
            "update didn't install",
            format_args!("still {current}, wanted {}", target.trim()),
        );
    }
}

/// The .app bundle this process is running from, if any.
pub fn installed_bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let bundle = exe.parent()?.parent()?.parent()?;
    (bundle.extension().and_then(|e| e.to_str()) == Some("app")).then(|| bundle.to_path_buf())
}

/// Whether this is an installed copy rather than a development build: the
/// gate for the automatic update check. macOS: running from a `.app`
/// bundle. Linux: a release build whose executable isn't under a cargo
/// `target/` directory (a package or a tarball install).
pub fn is_installed() -> bool {
    if cfg!(target_os = "macos") {
        return installed_bundle().is_some();
    }
    if cfg!(debug_assertions) {
        return false;
    }
    std::env::current_exe()
        .ok()
        .is_some_and(|exe| !exe.components().any(|c| c.as_os_str() == "target"))
}

#[cfg(target_os = "macos")]
fn sh_quote(p: &Path) -> String {
    format!("'{}'", p.to_string_lossy().replace('\'', r"'\''"))
}

/// Kick off the swap-and-relaunch script. The caller should quit the app
/// immediately after this returns Ok — the script waits for us to exit,
/// replaces the bundle, and reopens it.
#[cfg(target_os = "macos")]
pub fn install_and_restart(dmg: &Path) -> Result<(), String> {
    let Some(bundle) = installed_bundle() else {
        // Not running from an installed bundle (e.g. cargo run): hand the DMG
        // to the user for a drag install instead of guessing a destination.
        Command::new("open")
            .arg(dmg)
            .status()
            .map_err(|e| e.to_string())?;
        return Ok(());
    };
    let script = format!(
        r#"
sleep 1
MOUNT=$(mktemp -d)
hdiutil attach -nobrowse -readonly -mountpoint "$MOUNT" {dmg} || exit 1
APP=$(/bin/ls -d "$MOUNT"/*.app 2>/dev/null | head -1)
if [ -n "$APP" ]; then
  rm -rf {staging}
  ditto "$APP" {staging} && rm -rf {bundle} && mv {staging} {bundle}
fi
hdiutil detach "$MOUNT" -quiet || hdiutil detach "$MOUNT" -force || true
open {bundle}
"#,
        dmg = sh_quote(dmg),
        bundle = sh_quote(&bundle),
        staging = sh_quote(&bundle.with_extension("app.updating")),
    );
    Command::new("/bin/bash")
        .arg("-c")
        .arg(script)
        .spawn()
        .map_err(|e| format!("couldn't start installer: {e}"))?;
    Ok(())
}

/// Finish the rename for a copy updated in place by Oxide 0.8.x: its
/// updater copied this build's contents into `/Applications/Oxide.app`,
/// where the folder name no longer matches the app inside. Move it to
/// `OmniPTY.app` and relaunch from there; true means the caller should
/// exit. If an `OmniPTY.app` is already next to it, leave both alone.
#[cfg(target_os = "macos")]
pub fn relocate_renamed_bundle() -> bool {
    let Some(bundle) = installed_bundle() else {
        return false;
    };
    if bundle.file_name().and_then(|n| n.to_str()) != Some("Oxide.app") {
        return false;
    }
    let target = bundle.with_file_name("OmniPTY.app");
    if target.exists() || std::fs::rename(&bundle, &target).is_err() {
        return false;
    }
    // `open` on a bundle whose id is already running just activates us, so
    // the relaunch has to come after we've exited.
    Command::new("/bin/bash")
        .arg("-c")
        .arg(format!("sleep 1; open {}", sh_quote(&target)))
        .spawn()
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updated_from_only_reports_upgrades() {
        assert_eq!(updated_from(Some("0.5.0"), "0.5.1"), Some("0.5.0".into()));
        assert_eq!(updated_from(Some("0.5.1"), "0.5.1"), None, "same version");
        assert_eq!(
            updated_from(Some("0.6.0"), "0.5.1"),
            None,
            "dev build older than installed"
        );
        assert_eq!(updated_from(None, "0.5.1"), None, "fresh install");
    }

    #[test]
    fn manifest_picks_this_platform() {
        let m = serde_json::json!({
            "version": "1.2.3",
            "release_url": "https://example.com/releases/v1.2.3",
            "assets": {
                "macos-aarch64": {"url": "https://example.com/arm.dmg", "signature": "sig-arm"},
                "macos-x86_64": {"url": "https://example.com/x86.dmg", "signature": "sig-x86"},
                "linux-x86_64": {"url": "https://example.com/linux.tar.gz"},
            }
        });
        let info = parse_manifest(&m, "macos", "aarch64").unwrap();
        assert_eq!(info.version, "1.2.3");
        assert_eq!(info.url, "https://example.com/arm.dmg");
        assert_eq!(info.signature.as_deref(), Some("sig-arm"));
        let info = parse_manifest(&m, "macos", "x86_64").unwrap();
        assert_eq!(info.url, "https://example.com/x86.dmg");
        assert!(
            parse_manifest(&m, "macos", "riscv64").is_none(),
            "no build for this arch"
        );

        // Linux announces the release page, and only once its tarball is up.
        let info = parse_manifest(&m, "linux", "x86_64").unwrap();
        assert_eq!(info.url, "https://example.com/releases/v1.2.3");
        assert!(info.signature.is_none());
        assert!(
            parse_manifest(&m, "linux", "aarch64").is_none(),
            "other arch"
        );
    }

    #[test]
    fn linux_release_needs_its_tarball_and_a_page() {
        let mac_only = serde_json::json!({
            "version": "1.0.0",
            "release_url": "https://e.com/r",
            "assets": {"macos-aarch64": {"url": "https://e.com/a.dmg", "signature": "s"}}
        });
        assert!(parse_manifest(&mac_only, "linux", "x86_64").is_none());
        let no_page = serde_json::json!({
            "version": "1.0.0",
            "assets": {"linux-x86_64": {"url": "https://e.com/a.tar.gz"}}
        });
        assert!(parse_manifest(&no_page, "linux", "x86_64").is_none());
    }

    #[test]
    fn manifest_rejects_malformed_entries() {
        let good = |v: &str, url: &str, sig: serde_json::Value| {
            serde_json::json!({
                "version": v,
                "assets": {"macos-aarch64": {"url": url, "signature": sig}}
            })
        };
        let ok = good("v1.0.0", "https://e.com/a.dmg", "s".into());
        assert_eq!(
            parse_manifest(&ok, "macos", "aarch64").unwrap().version,
            "1.0.0",
            "v prefix"
        );
        assert!(
            parse_manifest(
                &good("nope", "https://e.com/a.dmg", "s".into()),
                "macos",
                "aarch64"
            )
            .is_none()
        );
        assert!(
            parse_manifest(
                &good("1.0.0", "http://e.com/a.dmg", "s".into()),
                "macos",
                "aarch64"
            )
            .is_none()
        );
        assert!(
            parse_manifest(
                &good("1.0.0", "https://e.com/a.dmg", serde_json::Value::Null),
                "macos",
                "aarch64"
            )
            .is_none()
        );
        assert!(parse_manifest(&serde_json::json!({}), "macos", "aarch64").is_none());
    }

    #[test]
    fn splits_curl_status_trailer() {
        assert_eq!(
            split_status(b"{\"a\":1}\n200"),
            Some((200, &b"{\"a\":1}"[..]))
        );
        assert_eq!(
            split_status(b"multi\nline\nbody\n404"),
            Some((404, &b"multi\nline\nbody"[..]))
        );
        assert_eq!(split_status(b"\n503"), Some((503, &b""[..])));
        assert_eq!(split_status(b"no trailer"), None);
        assert_eq!(split_status(b"body\nnot-a-number"), None);
    }

    // Fixtures made with `minisign -G -W` / `minisign -S` on a throwaway key
    // that signs nothing real. The payload's trusted
    // comment is "omnipty 1.2.3 macos-aarch64".
    const TEST_KEY: &str = "untrusted comment: minisign public key D66F6FFD24B82CD5
RWTVLLgk/W9v1rlHSasnEHv8tFv6Prnst2FH8BwgIQ/CBIqizJ62LKRM
";
    const OTHER_KEY: &str = "untrusted comment: minisign public key: 7217595898669E5F\nRWRfnmaYWFkXcmJ8XGPgA2+3eIhaLK4dvQMaVK6qNJlRkosec2ccVm8w\n";
    const TEST_PAYLOAD: &[u8] = b"omnipty test payload\n";
    const TEST_SIG: &str = "untrusted comment: test
RUTVLLgk/W9v1nUQH5kYnGCioeFAbCOyutVnqcXZ0tHIHrK0whI5JZpol3N6NwOn4HsWpkpC8oGAfNPvtBSDJW+7FlO80Tll4wg=
trusted comment: omnipty 1.2.3 macos-aarch64
/enZOCFwf9rVQvX29OOrywlcIJEB85LBuyDaKlnnRSB2Tq7q8cI7JjcnMdy5OtsQVoY0Ju1uZxyz1nVB1anCBQ==
";

    fn temp_file(name: &str, bytes: &[u8]) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("omnipty-update-test-{}-{name}", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn signature_accepts_the_signed_file_only() {
        let comment = signed_comment("1.2.3", "aarch64");
        let good = temp_file("good", TEST_PAYLOAD);
        assert_eq!(
            verify_signature(&good, TEST_KEY, TEST_SIG, &comment),
            Ok(())
        );

        let tampered = temp_file("tampered", b"omnipty test payload!\n");
        let err = verify_signature(&tampered, TEST_KEY, TEST_SIG, &comment).unwrap_err();
        assert!(err.contains("doesn't match"), "{err}");

        let _ = std::fs::remove_file(good);
        let _ = std::fs::remove_file(tampered);
    }

    #[test]
    fn signature_is_bound_to_key_version_and_arch() {
        let file = temp_file("bound", TEST_PAYLOAD);
        let err =
            verify_signature(&file, OTHER_KEY, TEST_SIG, "omnipty 1.2.3 macos-aarch64").unwrap_err();
        assert!(err.contains("different key"), "{err}");
        // A genuine signature can't be replayed as another version or arch.
        for wrong in ["omnipty 1.2.4 macos-aarch64", "omnipty 1.2.3 macos-x86_64", ""] {
            let err = verify_signature(&file, TEST_KEY, TEST_SIG, wrong).unwrap_err();
            assert!(err.contains("different version"), "{wrong}: {err}");
        }
        let _ = std::fs::remove_file(file);
    }

    #[test]
    fn signature_refuses_bad_inputs() {
        let file = temp_file("bad-inputs", TEST_PAYLOAD);
        let comment = signed_comment("1.2.3", "aarch64");
        let err = verify_signature(&file, "PLACEHOLDER", TEST_SIG, &comment).unwrap_err();
        assert!(err.contains("no update signing key"), "{err}");
        let err = verify_signature(&file, TEST_KEY, "garbage", &comment).unwrap_err();
        assert!(err.contains("unreadable"), "{err}");
        let missing = file.with_extension("missing");
        assert!(verify_signature(&missing, TEST_KEY, TEST_SIG, &comment).is_err());
        let _ = std::fs::remove_file(file);
    }

    #[test]
    fn version_comparison() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("v0.1.0", "0.2.0"));
        assert!(!is_newer("garbage", "0.1.0"));
    }
}
