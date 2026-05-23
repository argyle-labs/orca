//! `orca package build` — generate distributable packages from the current binary.
//!
//! Each format's postinst delegates to `orca system bootstrap` +
//! `orca daemon install --service-user orca`, so non-systemd support
//! (OpenRC, Unraid, launchd) is free via the existing detect_linux_init() dispatch.

use anyhow::Result;
use clap::{Subcommand, ValueEnum};
use colored::Colorize;
use sha2::Digest;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Subcommand, Debug)]
pub enum PackageAction {
    /// Build a distributable package from the current orca binary.
    /// Format is auto-detected from the host OS when --format is omitted.
    Build {
        /// Package format (deb / rpm / apk / pkgbuild).
        #[arg(long, value_enum)]
        format: Option<PackageFormat>,
        /// Write the finished package into this directory.
        #[arg(long, default_value = ".")]
        out_dir: PathBuf,
        /// Binary to package (default: running executable).
        #[arg(long)]
        binary: Option<PathBuf>,
        /// CPU architecture override for cross-compiled binaries (x86_64 or aarch64).
        #[arg(long)]
        arch: Option<String>,
        /// Maintainer string embedded in package metadata.
        #[arg(long, default_value = "Orca <noreply@orca.local>")]
        maintainer: String,
    },
}

#[derive(ValueEnum, Clone, Debug)]
pub enum PackageFormat {
    Deb,
    Rpm,
    Apk,
    Pkgbuild,
}

pub fn cmd_package(action: PackageAction) -> Result<()> {
    let PackageAction::Build { format, out_dir, binary, arch, maintainer } = action;

    let binary = binary.map(Ok).unwrap_or_else(|| std::env::current_exe())?;
    if !binary.exists() {
        anyhow::bail!("binary not found: {}", binary.display());
    }

    let format = format.map(Ok).unwrap_or_else(detect_format)?;
    let arch = arch.unwrap_or_else(|| std::env::consts::ARCH.to_string());
    std::fs::create_dir_all(&out_dir)?;

    match format {
        PackageFormat::Deb => build_deb(&binary, VERSION, &arch, &maintainer, &out_dir),
        PackageFormat::Rpm => build_rpm(&binary, VERSION, &arch, &maintainer, &out_dir),
        PackageFormat::Apk => build_apk(&binary, VERSION, &arch, &out_dir),
        PackageFormat::Pkgbuild => build_pkgbuild(VERSION, &arch, &out_dir),
    }
}

fn detect_format() -> Result<PackageFormat> {
    #[cfg(target_os = "linux")]
    {
        // Prefer tool presence over OS hints — more reliable on minimal images.
        if tool_available("dpkg") {
            return Ok(PackageFormat::Deb);
        }
        if tool_available("rpm") {
            return Ok(PackageFormat::Rpm);
        }
        if tool_available("apk") {
            return Ok(PackageFormat::Apk);
        }
        let os = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
        if os.contains("ID=arch") || os.contains("ID=manjaro") || os.contains("ID=endeavouros") {
            return Ok(PackageFormat::Pkgbuild);
        }
    }
    anyhow::bail!("could not auto-detect package format — pass --format deb|rpm|apk|pkgbuild")
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

    if tool_available("dpkg-deb") {
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
    println!("{} dpkg-deb not found — staging: {}", "!".yellow(), keep.display());
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

    if tool_available("rpmbuild") {
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
        if ok {
            if let Some(rpm) = find_file_ext(&staging.join("RPMS"), "rpm")? {
                let dest = out_dir.join(rpm.file_name().unwrap());
                std::fs::copy(&rpm, &dest)?;
                std::fs::remove_dir_all(&staging)?;
                println!("{} {}", "✓".green(), dest.display());
                return Ok(());
            }
        }
        anyhow::bail!("rpmbuild failed");
    }

    let keep = out_dir.join("orca-rpm-staging");
    if keep.exists() {
        std::fs::remove_dir_all(&keep)?;
    }
    std::fs::rename(&staging, &keep)?;
    println!("{} rpmbuild not found — staging: {}", "!".yellow(), keep.display());
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

    if tool_available("abuild") {
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

// ── helpers ───────────────────────────────────────────────────────────────────

fn tool_available(name: &str) -> bool {
    Command::new("which")
        .arg(name)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

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
    Ok(format!("{:x}", sha2::Sha512::digest(&buf)))
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
        if path.is_dir() {
            if let Some(found) = find_file_ext(&path, ext)? {
                return Ok(Some(found));
            }
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
        assert!(!s.contains("pkgver=0.0.4-rc.7"), "pkgver must not contain dashes");
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
    fn rpm_splits_version_at_dash() {
        let (ver, rel) = "0.0.4-rc.7".split_once('-').unwrap_or(("0.0.4-rc.7", "1"));
        assert_eq!(ver, "0.0.4");
        assert_eq!(rel, "rc.7");
        // No dash in version part
        assert!(!ver.contains('-'));
    }
}
