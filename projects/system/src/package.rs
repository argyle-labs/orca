//! `orca package build` — generate distributable packages from the current binary.
//!
//! Each format's postinst/postinstall delegates to `orca system bootstrap` +
//! `orca daemon install`, so non-systemd init (OpenRC, Unraid, launchd) is
//! handled automatically by the existing detect_linux_init() dispatch.

use anyhow::Result;
use colored::Colorize;
use contract::ToolCtx;
use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg_attr(feature = "cli", derive(clap::ValueEnum))]
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PackageFormat {
    /// Debian/Ubuntu — requires dpkg-deb
    Deb,
    /// RHEL/Fedora/Unraid — requires rpmbuild
    Rpm,
    /// Alpine — writes APKBUILD, requires abuild
    Apk,
    /// Arch/AUR — writes PKGBUILD, no build tool required
    Pkgbuild,
    /// macOS Installer — requires pkgbuild (Xcode CLT), optional productsign
    Pkg,
    /// Homebrew — writes a formula .rb file, no build tool required
    Homebrew,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PackageBuildArgs {
    /// Package format: deb / rpm / apk / pkgbuild / pkg / homebrew. Auto-detected when omitted.
    #[cfg_attr(feature = "cli", arg(long, value_enum))]
    pub format: Option<PackageFormat>,
    /// Write the finished package into this directory.
    #[cfg_attr(feature = "cli", arg(long, default_value = "."))]
    #[serde(default = "default_out_dir")]
    pub out_dir: PathBuf,
    /// Binary to package (default: running executable).
    #[cfg_attr(feature = "cli", arg(long))]
    pub binary: Option<PathBuf>,
    /// CPU architecture override for cross-compiled binaries (x86_64 or aarch64).
    #[cfg_attr(feature = "cli", arg(long))]
    pub arch: Option<String>,
    /// Maintainer string embedded in deb/rpm package metadata.
    #[cfg_attr(
        feature = "cli",
        arg(long, default_value = "Orca <noreply@orca.local>")
    )]
    #[serde(default = "default_maintainer")]
    pub maintainer: String,
    /// macOS Developer ID Application identity for codesign (binary signing).
    #[cfg_attr(feature = "cli", arg(long))]
    pub codesign_identity: Option<String>,
    /// macOS Developer ID Installer identity for productsign (.pkg signing).
    #[cfg_attr(feature = "cli", arg(long))]
    pub pkg_sign_identity: Option<String>,
}

fn default_out_dir() -> PathBuf {
    PathBuf::from(".")
}
fn default_maintainer() -> String {
    "Orca <noreply@orca.local>".to_string()
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
pub struct PackageBuildOutput {
    pub format: PackageFormat,
    pub version: String,
    pub arch: String,
    pub out_dir: PathBuf,
}

/// Build a distributable package (deb/rpm/apk/PKGBUILD/pkg/homebrew) from the current orca binary.
/// Format auto-detected from host OS when not provided. Postinst scripts delegate to
/// `system bootstrap` + `daemon install`.
#[orca_tool(domain = "system.package", verb = "build", local_only = true)]
async fn package_build(args: PackageBuildArgs, _ctx: &ToolCtx) -> Result<PackageBuildOutput> {
    let binary = args.binary.map(Ok).unwrap_or_else(std::env::current_exe)?;
    if !binary.exists() {
        anyhow::bail!("binary not found: {}", binary.display());
    }

    let format = args.format.map(Ok).unwrap_or_else(detect_format)?;
    let arch = args
        .arch
        .unwrap_or_else(|| std::env::consts::ARCH.to_string());
    std::fs::create_dir_all(&args.out_dir)?;

    match &format {
        PackageFormat::Deb => build_deb(&binary, VERSION, &arch, &args.maintainer, &args.out_dir)?,
        PackageFormat::Rpm => build_rpm(&binary, VERSION, &arch, &args.maintainer, &args.out_dir)?,
        PackageFormat::Apk => build_apk(&binary, VERSION, &arch, &args.out_dir)?,
        PackageFormat::Pkgbuild => build_pkgbuild(VERSION, &arch, &args.out_dir)?,
        PackageFormat::Pkg => build_pkg(
            &binary,
            VERSION,
            &arch,
            args.codesign_identity.as_deref(),
            args.pkg_sign_identity.as_deref(),
            &args.out_dir,
        )?,
        PackageFormat::Homebrew => build_homebrew(VERSION, &args.out_dir)?,
    }

    Ok(PackageBuildOutput {
        format,
        version: VERSION.to_string(),
        arch,
        out_dir: args.out_dir,
    })
}

fn detect_format() -> Result<PackageFormat> {
    #[cfg(target_os = "macos")]
    {
        Ok(PackageFormat::Pkg)
    }
    #[cfg(target_os = "linux")]
    {
        // Prefer tool presence over OS hints — more reliable on minimal images.
        if utils::path::which("dpkg").is_some() {
            return Ok(PackageFormat::Deb);
        }
        if utils::path::which("rpm").is_some() {
            return Ok(PackageFormat::Rpm);
        }
        if utils::path::which("apk").is_some() {
            return Ok(PackageFormat::Apk);
        }
        let os = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
        if os.contains("ID=arch") || os.contains("ID=manjaro") || os.contains("ID=endeavouros") {
            return Ok(PackageFormat::Pkgbuild);
        }
    }
    #[cfg(not(target_os = "macos"))]
    anyhow::bail!(
        "could not auto-detect package format — pass --format deb|rpm|apk|pkgbuild|pkg|homebrew"
    )
}

// ── .deb ──────────────────────────────────────────────────────────────────────

fn build_deb(
    binary: &Path,
    version: &str,
    arch: &str,
    maintainer: &str,
    out_dir: &Path,
) -> Result<()> {
    let deb_arch = match arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        a => a,
    };

    let staging = out_dir.join(".orca-deb-staging");
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }

    let debian = staging.join("DEBIAN");
    std::fs::create_dir_all(&debian)?;

    std::fs::write(
        debian.join("control"),
        format!(
            "Package: orca\nVersion: {version}\nArchitecture: {deb_arch}\n\
             Maintainer: {maintainer}\nPriority: optional\nSection: utils\n\
             Description: Orca AI daemon\n Mesh-network AI orchestration daemon.\n"
        ),
    )?;
    write_script(
        &debian.join("postinst"),
        "#!/bin/sh\nset -e\n\
         /usr/local/bin/orca system bootstrap 2>/dev/null || true\n\
         /usr/local/bin/orca daemon install --service-user orca 2>/dev/null || true\n",
    )?;
    write_script(
        &debian.join("prerm"),
        "#!/bin/sh\nset -e\n\
         /usr/local/bin/orca daemon uninstall 2>/dev/null || true\n",
    )?;

    let bin_dir = staging.join("usr/local/bin");
    std::fs::create_dir_all(&bin_dir)?;
    let staged_bin = bin_dir.join("orca");
    std::fs::copy(binary, &staged_bin)?;
    set_mode_755(&staged_bin)?;

    let pkg_name = format!("orca_{version}_{deb_arch}.deb");
    let out = out_dir.join(&pkg_name);

    if utils::path::which("dpkg-deb").is_some() {
        let ok = Command::new("dpkg-deb")
            .args(["--build", "--root-owner-group"])
            .arg(&staging)
            .arg(&out)
            .status()?
            .success();
        std::fs::remove_dir_all(&staging)?;
        if ok {
            println!("{} {}", "✓".green(), out.display());
            return Ok(());
        }
        anyhow::bail!("dpkg-deb failed");
    }

    // No dpkg-deb — keep staging for manual build.
    let keep = out_dir.join("orca-deb-staging");
    if keep.exists() {
        std::fs::remove_dir_all(&keep)?;
    }
    std::fs::rename(&staging, &keep)?;
    println!(
        "{} dpkg-deb not found — staging: {}",
        "!".yellow(),
        keep.display()
    );
    println!(
        "  build: dpkg-deb --build --root-owner-group {} {}",
        keep.display(),
        out.display()
    );
    Ok(())
}

// ── .rpm ──────────────────────────────────────────────────────────────────────

fn build_rpm(
    binary: &Path,
    version: &str,
    arch: &str,
    maintainer: &str,
    out_dir: &Path,
) -> Result<()> {
    // RPM version strings cannot contain dashes.
    let (rpm_ver, rpm_rel) = version.split_once('-').unwrap_or((version, "1"));

    let staging = out_dir.join(".orca-rpm-staging");
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    for d in &["BUILD", "RPMS", "SOURCES", "SPECS", "SRPMS"] {
        std::fs::create_dir_all(staging.join(d))?;
    }

    let src = staging.join("SOURCES/orca");
    std::fs::copy(binary, &src)?;
    set_mode_755(&src)?;

    std::fs::write(
        staging.join("SPECS/orca.spec"),
        format!(
            "Name:        orca\n\
             Version:     {rpm_ver}\n\
             Release:     {rpm_rel}%{{?dist}}\n\
             Summary:     Orca AI daemon\n\
             License:     Proprietary\n\
             BuildArch:   {arch}\n\
             Packager:    {maintainer}\n\n\
             %description\n\
             Mesh-network AI orchestration daemon.\n\n\
             %prep\n\
             cp %{{SOURCE0}} orca\n\n\
             %install\n\
             mkdir -p %{{buildroot}}/usr/local/bin\n\
             install -m 755 orca %{{buildroot}}/usr/local/bin/orca\n\n\
             %post\n\
             /usr/local/bin/orca system bootstrap 2>/dev/null || true\n\
             /usr/local/bin/orca daemon install --service-user orca 2>/dev/null || true\n\n\
             %preun\n\
             /usr/local/bin/orca daemon uninstall 2>/dev/null || true\n\n\
             %files\n\
             /usr/local/bin/orca\n"
        ),
    )?;

    if utils::path::which("rpmbuild").is_some() {
        let topdir = staging.display().to_string();
        let ok = Command::new("rpmbuild")
            .args([
                "-bb",
                "--define",
                &format!("_topdir {topdir}"),
                "--define",
                "_binary_payload w9.gzdio",
            ])
            .arg(staging.join("SPECS/orca.spec").to_str().unwrap())
            .status()?
            .success();
        if ok && let Some(rpm) = find_file_ext(&staging.join("RPMS"), "rpm")? {
            let dest = out_dir.join(rpm.file_name().unwrap());
            std::fs::copy(&rpm, &dest)?;
            std::fs::remove_dir_all(&staging)?;
            println!("{} {}", "✓".green(), dest.display());
            return Ok(());
        }
        anyhow::bail!("rpmbuild failed");
    }

    let keep = out_dir.join("orca-rpm-staging");
    if keep.exists() {
        std::fs::remove_dir_all(&keep)?;
    }
    std::fs::rename(&staging, &keep)?;
    println!(
        "{} rpmbuild not found — staging: {}",
        "!".yellow(),
        keep.display()
    );
    println!(
        "  build: rpmbuild -bb --define '_topdir {}' {}/SPECS/orca.spec",
        keep.display(),
        keep.display()
    );
    Ok(())
}

// ── .apk (Alpine) ─────────────────────────────────────────────────────────────

fn build_apk(binary: &Path, version: &str, arch: &str, out_dir: &Path) -> Result<()> {
    let apk_ver = version.replace('-', "_");
    let checksum = sha512_hex(binary)?;

    let staging = out_dir.join("orca-apk-staging");
    std::fs::create_dir_all(&staging)?;

    std::fs::write(
        staging.join("APKBUILD"),
        format!(
            "# Maintainer: Orca <noreply@orca.local>\n\
             pkgname=orca\n\
             pkgver={apk_ver}\n\
             pkgrel=0\n\
             pkgdesc=\"Orca AI daemon\"\n\
             url=\"https://github.com/scottdkey/orca\"\n\
             arch=\"{arch}\"\n\
             license=\"custom\"\n\
             source=\"orca\"\n\
             sha512sums=\"{checksum}  orca\"\n\n\
             package() {{\n\
             \tinstall -Dm755 \"$srcdir/orca\" \"$pkgdir/usr/local/bin/orca\"\n\
             }}\n\n\
             post_install() {{\n\
             \t/usr/local/bin/orca system bootstrap 2>/dev/null || true\n\
             \t/usr/local/bin/orca daemon install --service-user orca 2>/dev/null || true\n\
             }}\n\n\
             pre_deinstall() {{\n\
             \t/usr/local/bin/orca daemon uninstall 2>/dev/null || true\n\
             }}\n"
        ),
    )?;
    std::fs::copy(binary, staging.join("orca"))?;
    set_mode_755(&staging.join("orca"))?;

    if utils::path::which("abuild").is_some() {
        let ok = Command::new("abuild")
            .arg("-r")
            .current_dir(&staging)
            .status()?
            .success();
        if ok {
            println!("{} apk built in {}", "✓".green(), staging.display());
            return Ok(());
        }
        anyhow::bail!("abuild failed");
    }

    println!("{} APKBUILD → {}", "✓".green(), staging.display());
    println!("  build: cd {} && abuild -r", staging.display());
    Ok(())
}

// ── PKGBUILD (AUR / Arch) ─────────────────────────────────────────────────────

fn build_pkgbuild(version: &str, arch: &str, out_dir: &Path) -> Result<()> {
    // pkgver cannot contain dashes.
    let pkgver = version.replace('-', ".");
    let aur_archs = if arch == "aarch64" {
        "'aarch64'"
    } else {
        "'x86_64' 'aarch64'"
    };

    std::fs::write(
        out_dir.join("PKGBUILD"),
        format!(
            "# Maintainer: Orca <noreply@orca.local>\n\
             # NOTE: update sha256sums_* with real hashes before publishing to AUR.\n\
             _ver={version}\n\
             pkgname=orca\n\
             pkgver={pkgver}\n\
             pkgrel=1\n\
             pkgdesc='Orca AI daemon'\n\
             arch=({aur_archs})\n\
             url='https://github.com/scottdkey/orca'\n\
             license=('custom')\n\n\
             source_x86_64=(\"$pkgname-$_ver-x86_64::https://github.com/scottdkey/orca/releases/download/v$_ver/$pkgname-$_ver-x86_64-unknown-linux-gnu\")\n\
             source_aarch64=(\"$pkgname-$_ver-aarch64::https://github.com/scottdkey/orca/releases/download/v$_ver/$pkgname-$_ver-aarch64-unknown-linux-gnu\")\n\
             sha256sums_x86_64=('SKIP')\n\
             sha256sums_aarch64=('SKIP')\n\n\
             package() {{\n\
                 install -Dm755 \"$pkgname-$_ver-${{CARCH}}\" \"$pkgdir/usr/local/bin/orca\"\n\
             }}\n\n\
             post_install() {{\n\
                 /usr/local/bin/orca system bootstrap 2>/dev/null || true\n\
                 /usr/local/bin/orca daemon install --service-user orca 2>/dev/null || true\n\
             }}\n\n\
             pre_remove() {{\n\
                 /usr/local/bin/orca daemon uninstall 2>/dev/null || true\n\
             }}\n"
        ),
    )?;

    println!("{} {}", "✓".green(), out_dir.join("PKGBUILD").display());
    println!(
        "  build: cd {} && makepkg -si --skipinteg",
        out_dir.display()
    );
    println!("  note: replace SKIP checksums before publishing to AUR");
    Ok(())
}

// ── .pkg (macOS Installer) ────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn build_pkg(
    binary: &Path,
    version: &str,
    arch: &str,
    codesign_identity: Option<&str>,
    pkg_sign_identity: Option<&str>,
    out_dir: &Path,
) -> Result<()> {
    let staging = out_dir.join(".orca-pkg-staging");
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }

    let root = staging.join("root");
    let scripts = staging.join("scripts");
    std::fs::create_dir_all(root.join("usr/local/bin"))?;
    std::fs::create_dir_all(&scripts)?;

    // Place binary in payload root.
    let bin = root.join("usr/local/bin/orca");
    std::fs::copy(binary, &bin)?;
    set_mode_755(&bin)?;

    // Sign binary: real identity → hardened runtime; ad-hoc for local use.
    let sign = codesign_identity.unwrap_or("-");
    let mut codesign_cmd = Command::new("codesign");
    codesign_cmd.args(["--force", "--sign", sign]);
    if sign != "-" {
        codesign_cmd.args(["--options", "runtime"]);
    }
    match codesign_cmd.arg(&bin).status() {
        Ok(s) if s.success() => {
            if sign == "-" {
                println!("{} binary: ad-hoc signed (local use only)", "!".yellow());
            } else {
                println!("{} binary: codesigned with '{sign}'", "✓".green());
            }
        }
        _ => eprintln!("warn: codesign failed — binary will be unsigned"),
    }

    // postinstall: install for the logged-in user, not root running the installer.
    write_script(
        &scripts.join("postinstall"),
        "#!/bin/sh
set -e
# Detect the actual logged-in user (the installer runs as root).
REAL_USER=$(stat -f \"%Su\" /dev/console 2>/dev/null || echo \"$USER\")
if [ -n \"$REAL_USER\" ] && [ \"$REAL_USER\" != \"root\" ]; then
   sudo -u \"$REAL_USER\" /usr/local/bin/orca daemon install 2>/dev/null || true
else
   /usr/local/bin/orca daemon install 2>/dev/null || true
fi
",
    )?;

    let unsigned_pkg = staging.join(format!("orca_{version}_{arch}_unsigned.pkg"));
    let final_pkg = out_dir.join(format!("orca_{version}_{arch}.pkg"));
    const IDENTIFIER: &str = "com.orca.daemon";

    if utils::path::which("pkgbuild").is_none() {
        let keep = out_dir.join("orca-pkg-staging");
        if keep.exists() {
            std::fs::remove_dir_all(&keep)?;
        }
        std::fs::rename(&staging, &keep)?;
        println!(
            "{} pkgbuild not found — install Xcode CLT: xcode-select --install",
            "!".yellow()
        );
        println!(
            "  build: pkgbuild --root {keep}/root --scripts {keep}/scripts \
             --identifier {IDENTIFIER} --version {version} {final}",
            keep = keep.display(),
            final = final_pkg.display()
        );
        return Ok(());
    }

    let ok = Command::new("pkgbuild")
        .arg("--root")
        .arg(&root)
        .arg("--scripts")
        .arg(&scripts)
        .args(["--identifier", IDENTIFIER])
        .args(["--version", version])
        .arg(&unsigned_pkg)
        .status()?
        .success();

    if !ok {
        std::fs::remove_dir_all(&staging)?;
        anyhow::bail!("pkgbuild failed");
    }

    // productsign if installer identity provided.
    if let Some(identity) = pkg_sign_identity {
        if utils::path::which("productsign").is_some() {
            let ok = Command::new("productsign")
                .args(["--sign", identity])
                .arg(&unsigned_pkg)
                .arg(&final_pkg)
                .status()?
                .success();
            std::fs::remove_dir_all(&staging)?;
            if ok {
                println!("{} {}", "✓".green(), final_pkg.display());
                println!(
                    "  notarize: xcrun notarytool submit {} \\\n    --apple-id <id> --team-id <team> --password <app-specific-pwd>\n  staple:   xcrun stapler staple {}",
                    final_pkg.display(),
                    final_pkg.display()
                );
                return Ok(());
            }
            anyhow::bail!("productsign failed");
        }
        eprintln!("warn: productsign not found — package will be unsigned");
    }

    // Move unsigned pkg to final path.
    std::fs::rename(&unsigned_pkg, &final_pkg)?;
    std::fs::remove_dir_all(&staging)?;
    println!("{} {} (unsigned)", "✓".green(), final_pkg.display());
    if pkg_sign_identity.is_none() {
        println!(
            "  sign:  productsign --sign 'Developer ID Installer: Name (TeamID)' \
             {unsigned} {signed}",
            unsigned = final_pkg.display(),
            signed = out_dir
                .join(format!("orca_{version}_{arch}_signed.pkg"))
                .display()
        );
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn build_pkg(
    _binary: &Path,
    _version: &str,
    _arch: &str,
    _codesign_identity: Option<&str>,
    _pkg_sign_identity: Option<&str>,
    _out_dir: &Path,
) -> Result<()> {
    anyhow::bail!("--format pkg is macOS-only — use deb/rpm/apk/pkgbuild on Linux")
}

// ── Homebrew formula ──────────────────────────────────────────────────────────

fn build_homebrew(version: &str, out_dir: &Path) -> Result<()> {
    // Homebrew formula: uses the `service` block for launchd instead of
    // `orca daemon install`, which keeps Homebrew as the service manager.
    let formula = format!(
        "class Orca < Formula
  desc \"Orca AI daemon\"
  homepage \"https://github.com/scottdkey/orca\"
  version \"{version}\"
  license \"Proprietary\"

  on_macos do
    on_intel do
      url \"https://github.com/scottdkey/orca/releases/download/v{version}/orca-{version}-x86_64-apple-darwin\"
      sha256 \"FILL_IN_x86_64_sha256\"
    end
    on_arm do
      url \"https://github.com/scottdkey/orca/releases/download/v{version}/orca-{version}-aarch64-apple-darwin\"
      sha256 \"FILL_IN_aarch64_sha256\"
    end
  end

  def install
    cpu = Hardware::CPU.intel? ? \"x86_64\" : \"aarch64\"
    bin.install \"orca-{version}-#{{cpu}}-apple-darwin\" => \"orca\"
  end

  # Homebrew manages the launchd plist via brew services.
  service do
    run [opt_bin/\"orca\", \"daemon\", \"start\", \"--port\", \"12000\"]
    keep_alive true
    log_path var/\"log/orca.log\"
    error_log_path var/\"log/orca.log\"
  end

  def post_install
    system bin/\"orca\", \"install\"
  rescue StandardError
    nil
  end
end
"
    );

    let path = out_dir.join("orca.rb");
    std::fs::write(&path, &formula)?;
    println!("{} {}", "✓".green(), path.display());
    println!("  note: update sha256 checksums before distributing");
    println!("  tap:   brew tap scottdkey/orca <path-or-url>");
    println!("  install: brew install scottdkey/orca/orca");
    Ok(())
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn write_script(path: &Path, content: &str) -> Result<()> {
    std::fs::write(path, content)?;
    set_mode_755(path)
}

#[cfg(unix)]
fn set_mode_755(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    Ok(std::fs::set_permissions(path, perms)?)
}

#[cfg(not(unix))]
fn set_mode_755(_path: &Path) -> Result<()> {
    Ok(())
}

fn sha512_hex(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    let hash = sha2::Sha512::digest(&buf);
    Ok(hash.iter().map(|b| format!("{b:02x}")).collect())
}

fn find_file_ext(dir: &Path, ext: &str) -> Result<Option<PathBuf>> {
    if !dir.exists() {
        return Ok(None);
    }
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some(ext) {
            return Ok(Some(path));
        }
        // rpmbuild puts .rpm files in arch subdirs — recurse one level.
        if path.is_dir()
            && let Some(found) = find_file_ext(&path, ext)?
        {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkgbuild_version_replaces_dash() {
        let dir = tempfile::tempdir().unwrap();
        build_pkgbuild("0.0.4-rc.7", "x86_64", dir.path()).unwrap();
        let s = std::fs::read_to_string(dir.path().join("PKGBUILD")).unwrap();
        assert!(s.contains("pkgver=0.0.4.rc.7"), "pkgver must use dots");
        assert!(
            !s.contains("pkgver=0.0.4-rc.7"),
            "pkgver must not contain dashes"
        );
        assert!(s.contains("_ver=0.0.4-rc.7"), "raw version kept in _ver");
    }

    #[test]
    fn pkgbuild_contains_postinst_hooks() {
        let dir = tempfile::tempdir().unwrap();
        build_pkgbuild("0.0.4", "x86_64", dir.path()).unwrap();
        let s = std::fs::read_to_string(dir.path().join("PKGBUILD")).unwrap();
        assert!(s.contains("orca system bootstrap"));
        assert!(s.contains("orca daemon install --service-user orca"));
        assert!(s.contains("orca daemon uninstall"));
    }

    #[test]
    fn homebrew_formula_contains_service_block() {
        let dir = tempfile::tempdir().unwrap();
        build_homebrew("0.0.4-rc.7", dir.path()).unwrap();
        let s = std::fs::read_to_string(dir.path().join("orca.rb")).unwrap();
        assert!(s.contains("class Orca < Formula"));
        assert!(s.contains("service do"));
        assert!(s.contains("brew services"));
        // Formula uses brew services, NOT orca daemon install
        assert!(!s.contains("daemon install"));
    }

    #[test]
    fn rpm_splits_version_at_dash() {
        let (ver, rel) = "0.0.4-rc.7".split_once('-').unwrap_or(("0.0.4-rc.7", "1"));
        assert_eq!(ver, "0.0.4");
        assert_eq!(rel, "rc.7");
        assert!(!ver.contains('-'));
    }
}
