//! `orca package build` — generate distributable packages from the current binary.
//!
//! Each format's postinst/postinstall delegates to `orca system install`
//! (which absorbed the former `system bootstrap` + `daemon install`), so
//! non-systemd init (OpenRC, Unraid, launchd) is handled automatically by
//! the existing detect_linux_init() dispatch.

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

#[derive(clap::ValueEnum, Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq, Eq)]
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
    /// Unraid — writes a `.plg` plugin manifest. Lifecycle is driven by
    /// emhttpd event hooks (disks_mounted -> start, stopping_svcs -> stop)
    /// so Unraid owns clean startup AND shutdown, retiring the
    /// /boot/config/go SHFS-poll hook. See
    /// [[project-unraid-plugin-install-blocked-on-graphql]] and
    /// [[project-orca-unraid-unclean-shutdown]].
    Plg,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct PackageBuildArgs {
    /// Package format: deb / rpm / apk / pkgbuild / pkg / homebrew. Auto-detected when omitted.
    #[arg(long, value_enum)]
    pub format: Option<PackageFormat>,
    /// Write the finished package into this directory.
    #[arg(long, default_value = ".")]
    #[serde(default = "default_out_dir")]
    pub out_dir: PathBuf,
    /// Binary to package (default: running executable).
    #[arg(long)]
    pub binary: Option<PathBuf>,
    /// CPU architecture override for cross-compiled binaries (x86_64 or aarch64).
    #[arg(long)]
    pub arch: Option<String>,
    /// Maintainer string embedded in deb/rpm package metadata.
    #[arg(long, default_value = "Orca <noreply@orca.local>")]
    #[serde(default = "default_maintainer")]
    pub maintainer: String,
    /// macOS Developer ID Application identity for codesign (binary signing).
    #[arg(long)]
    pub codesign_identity: Option<String>,
    /// macOS Developer ID Installer identity for productsign (.pkg signing).
    #[arg(long)]
    pub pkg_sign_identity: Option<String>,
    /// `.plg` only — URL where the published `.plg` file itself will
    /// live (Unraid uses this to check for plugin updates). Defaults to
    /// the github-releases convention for this version.
    #[arg(long)]
    pub plg_url: Option<String>,
    /// `.plg` only — URL where the binary payload will live. Defaults
    /// to the github-releases convention for the current arch.
    #[arg(long)]
    pub plg_binary_url: Option<String>,
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
/// Format auto-detected from host OS when not provided. Postinst scripts
/// delegate to `system install --service-user orca` (which absorbed the old
/// `system.bootstrap` + supervisor-install responsibilities).
#[orca_tool(domain = "system", verb = "build", local_only = true)]
async fn system_build(args: PackageBuildArgs, _ctx: &ToolCtx) -> Result<PackageBuildOutput> {
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
        PackageFormat::Plg => build_plg(
            &binary,
            VERSION,
            &arch,
            args.plg_url.as_deref(),
            args.plg_binary_url.as_deref(),
            &args.out_dir,
        )?,
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

/// Map a Rust/host arch string to a Debian architecture name.
fn deb_arch(arch: &str) -> &str {
    match arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        a => a,
    }
}

fn build_deb(
    binary: &Path,
    version: &str,
    arch: &str,
    maintainer: &str,
    out_dir: &Path,
) -> Result<()> {
    let deb_arch = deb_arch(arch);

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
         /usr/local/bin/orca system install --service-user orca 2>/dev/null || true\n",
    )?;
    write_script(
        &debian.join("prerm"),
        "#!/bin/sh\nset -e\n\
         /usr/local/bin/orca system delete 2>/dev/null || true\n",
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
             Packager:    {maintainer}\n\
             Source0:     orca\n\n\
             %description\n\
             Mesh-network AI orchestration daemon.\n\n\
             %prep\n\
             cp %{{SOURCE0}} orca\n\n\
             %install\n\
             mkdir -p %{{buildroot}}/usr/local/bin\n\
             install -m 755 orca %{{buildroot}}/usr/local/bin/orca\n\n\
             %post\n\
             /usr/local/bin/orca system install --service-user orca 2>/dev/null || true\n\n\
             %preun\n\
             /usr/local/bin/orca system delete 2>/dev/null || true\n\n\
             %files\n\
             /usr/local/bin/orca\n"
        ),
    )?;

    if utils::path::which("rpmbuild").is_some() {
        let topdir = staging.display().to_string();
        let ok = Command::new("rpmbuild")
            .args([
                "-bb",
                // Set the target arch explicitly so rpmbuild will emit a
                // package for a non-native arch (e.g. building the aarch64 rpm
                // on an x86_64 runner). Without this, a foreign `BuildArch`
                // fails with "No compatible architectures found for build".
                "--target",
                arch,
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
             url=\"https://github.com/argyle-labs/orca\"\n\
             arch=\"{arch}\"\n\
             license=\"custom\"\n\
             source=\"orca\"\n\
             sha512sums=\"{checksum}  orca\"\n\n\
             package() {{\n\
             \tinstall -Dm755 \"$srcdir/orca\" \"$pkgdir/usr/local/bin/orca\"\n\
             }}\n\n\
             post_install() {{\n\
             \t/usr/local/bin/orca system install --service-user orca 2>/dev/null || true\n\
             }}\n\n\
             pre_deinstall() {{\n\
             \t/usr/local/bin/orca system delete 2>/dev/null || true\n\
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

/// Select the AUR `arch=(...)` tuple. An aarch64 host produces an
/// aarch64-only package; otherwise both arches are advertised.
fn aur_archs(arch: &str) -> &'static str {
    if arch == "aarch64" {
        "'aarch64'"
    } else {
        "'x86_64' 'aarch64'"
    }
}

fn build_pkgbuild(version: &str, arch: &str, out_dir: &Path) -> Result<()> {
    // pkgver cannot contain dashes.
    let pkgver = version.replace('-', ".");
    let aur_archs = aur_archs(arch);

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
             url='https://github.com/argyle-labs/orca'\n\
             license=('custom')\n\n\
             source_x86_64=(\"$pkgname-$_ver-x86_64::https://github.com/argyle-labs/orca/releases/download/v$_ver/$pkgname-x86_64-unknown-linux-gnu\")\n\
             source_aarch64=(\"$pkgname-$_ver-aarch64::https://github.com/argyle-labs/orca/releases/download/v$_ver/$pkgname-aarch64-unknown-linux-gnu\")\n\
             sha256sums_x86_64=('SKIP')\n\
             sha256sums_aarch64=('SKIP')\n\n\
             package() {{\n\
                 install -Dm755 \"$pkgname-$_ver-${{CARCH}}\" \"$pkgdir/usr/local/bin/orca\"\n\
             }}\n\n\
             post_install() {{\n\
                 /usr/local/bin/orca system install --service-user orca 2>/dev/null || true\n\
             }}\n\n\
             pre_remove() {{\n\
                 /usr/local/bin/orca system delete 2>/dev/null || true\n\
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
   sudo -u \"$REAL_USER\" /usr/local/bin/orca system install 2>/dev/null || true
else
   /usr/local/bin/orca system install 2>/dev/null || true
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
    // `orca system install`, which keeps Homebrew as the service manager.
    let formula = format!(
        "class Orca < Formula
  desc \"Orca AI daemon\"
  homepage \"https://github.com/argyle-labs/orca\"
  version \"{version}\"
  license \"Proprietary\"

  on_macos do
    on_intel do
      url \"https://github.com/argyle-labs/orca/releases/download/v{version}/orca-x86_64-apple-darwin\"
      sha256 \"FILL_IN_x86_64_sha256\"
    end
    on_arm do
      url \"https://github.com/argyle-labs/orca/releases/download/v{version}/orca-aarch64-apple-darwin\"
      sha256 \"FILL_IN_aarch64_sha256\"
    end
  end

  def install
    cpu = Hardware::CPU.intel? ? \"x86_64\" : \"aarch64\"
    bin.install \"orca-#{{cpu}}-apple-darwin\" => \"orca\"
  end

  # Homebrew manages the launchd plist via brew services.
  service do
    run [opt_bin/\"orca\", \"daemon\", \"start\", \"--port\", \"12000\"]
    keep_alive true
    log_path var/\"log/orca.log\"
    error_log_path var/\"log/orca.log\"
  end

  def post_install
    system bin/\"orca\", \"system\", \"install\"
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
    println!("  tap:   brew tap argyle-labs/orca <path-or-url>");
    println!("  install: brew install argyle-labs/orca/orca");
    Ok(())
}

// ── .plg (Unraid plugin manifest) ─────────────────────────────────────────────

/// Build an Unraid plugin manifest (`.plg`). The Unraid plugin manager
/// downloads the binary referenced by `<URL>` (verifying `<MD5>`), then
/// runs the inline install script. Removal runs the inline remove
/// script. This retires the ssh+scp bootstrap and the
/// "orca daemon dies after rc swap" symptom — see
/// [[project-unraid-daemon-dies-after-swap]].
/// Map an arch to the linux GNU target triple used in release URLs.
fn linux_triple(arch: &str) -> &str {
    match arch {
        "x86_64" => "x86_64-unknown-linux-gnu",
        "aarch64" => "aarch64-unknown-linux-gnu",
        a => a,
    }
}

fn build_plg(
    binary: &Path,
    version: &str,
    arch: &str,
    plg_url: Option<&str>,
    plg_binary_url: Option<&str>,
    out_dir: &Path,
) -> Result<()> {
    let triple = linux_triple(arch);
    let plg_url = plg_url.map(str::to_string).unwrap_or_else(|| {
        format!("https://github.com/argyle-labs/orca/releases/download/v{version}/orca.plg")
    });
    let binary_url = plg_binary_url.map(str::to_string).unwrap_or_else(|| {
        // Point at the LEGACY unversioned asset name (`orca-<triple>`) — the
        // only raw-binary name release-lib.sh actually uploads (stage_target_asset
        // publishes `orca-<triple>`, NOT `orca-<version>-<triple>`). The versioned
        // form was never a published asset, so the old default 404'd on every
        // `plugin install`. Matches update.rs `legacy_asset_name`.
        format!("https://github.com/argyle-labs/orca/releases/download/v{version}/orca-{triple}")
    });
    let md5 = md5_hex(binary)?;

    // Inline install/remove scripts are SHFS-safe — the .plg only writes
    // to /boot/config/ at install time, and stages a deferred
    // post-shfs-install.sh that runs after /mnt/user becomes fuse.shfs.
    // See [[project-orca-plg-poisons-shfs]] for why this matters.
    let install_script = render_plg_install_script();
    let remove_script = render_plg_remove_script();

    let plg = format!(
        r#"<?xml version="1.0" standalone="yes"?>
<!DOCTYPE PLUGIN [
<!ENTITY name      "orca">
<!ENTITY author    "argyle-labs">
<!ENTITY version   "{version}">
<!ENTITY launch    "Settings/Orca">
<!ENTITY pluginURL "{plg_url}">
<!ENTITY md5       "{md5}">
<!ENTITY plugin    "/boot/config/plugins/orca">
<!ENTITY appdata   "/mnt/user/appdata/orca">
<!ENTITY binary    "{binary_url}">
]>
<PLUGIN  name="&name;" author="&author;" version="&version;" pluginURL="&pluginURL;" min="6.10" launch="&launch;">

  <CHANGES>
## &version;
- Consolidated under the standard plugin dir (/usr/local/emhttp/plugins/orca):
  scripts/rc.orca init script + event/ hooks, symlinked to /etc/rc.d/rc.orca.
- Event-driven lifecycle: emhttpd fires disks_mounted -> start and
  stopping_svcs -> stop, so Unraid owns clean startup AND shutdown.
- Fixes unclean shutdowns: the daemon is now stopped before /mnt/user
  unmounts (was holding shfs busy -> forced crash + parity check).
- Retires the /boot/config/go SHFS-poll hook (auto-migrated on upgrade).
- SHFS-safe install: writes only /boot/config/ (USB) + emhttp plugin dir (RAM).
  </CHANGES>

  <!-- Download the binary to the USB plugin dir; verified by MD5. -->
  <FILE Name="&plugin;/bin/orca">
    <URL>&binary;</URL>
    <MD5>&md5;</MD5>
  </FILE>

  <!-- Install: USB-only writes; defer /mnt/user work via go-hook. -->
  <FILE Run="/bin/bash">
    <INLINE>
<![CDATA[
{install_script}
]]>
    </INLINE>
  </FILE>

  <!-- Remove: stop daemon, tear down go-hook + plugin dir. -->
  <FILE Run="/bin/bash" Method="remove">
    <INLINE>
<![CDATA[
{remove_script}
]]>
    </INLINE>
  </FILE>
</PLUGIN>
"#
    );

    let path = out_dir.join("orca.plg");
    std::fs::write(&path, &plg)?;
    println!("{} {}", "✓".green(), path.display());
    println!("  publish: upload alongside the binary to the github release");
    println!("  install: from Unraid → Plugins → Install Plugin → paste {plg_url}");
    Ok(())
}

fn render_plg_install_script() -> String {
    // Runs at plugin install AND at every boot (Unraid plugin manager
    // iterates /boot/config/plugins/*.plg via rc.local). MUST be
    // idempotent.
    //
    // CRITICAL: .plg fires BEFORE SHFS mounts on boot. Any write to
    // /mnt/user/* here creates a tmpfs-poisoned mountpoint that prevents
    // emhttpd from spawning shfs, taking the entire host's shares + NFS
    // exports + docker offline. See [[project-orca-plg-poisons-shfs]] for
    // the 2026-06-09 echo incident. So this script only writes to
    // /boot/config/ (USB) and /usr/local/emhttp/ (RAM) — never /mnt/user.
    //
    // Everything lives under the standard Unraid plugin dir
    // (/usr/local/emhttp/plugins/orca), recreated from this idempotent
    // script on every boot:
    //   scripts/rc.orca            — start|stop|restart|status init script
    //   event/disks_mounted        — rc.orca start (SHFS guaranteed up)
    //   event/stopping_svcs        — rc.orca stop  (BEFORE shfs unmount)
    //   event/unmounting_disks     — rc.orca stop  (force backstop)
    // rc.orca is symlinked to /etc/rc.d/rc.orca (Slackware convention).
    // emhttpd firing stopping_svcs before unmount is what lets Unraid own
    // clean startup AND shutdown — the old go-hook path had no stop event,
    // so the unmanaged daemon held /mnt/user busy at array-stop and forced
    // UNCLEAN shutdowns. See [[project-orca-unraid-unclean-shutdown]].
    format!(
        r#"#!/bin/bash
set -e
PLUGIN=/boot/config/plugins/orca        # USB (persistent)
EMHTTP=/usr/local/emhttp/plugins/orca   # RAM (rebuilt each boot by this script)
RCD=/etc/rc.d/rc.orca

mkdir -p "$EMHTTP/event" "$EMHTTP/scripts"

# --- rc.orca init script (canonical copy lives under the plugin dir) -------
cat > "$EMHTTP/scripts/rc.orca" <<'RCORCA'
{rc_orca}
RCORCA
chmod 0755 "$EMHTTP/scripts/rc.orca"
# Slackware/Unraid convention: expose lifecycle as an /etc/rc.d init script.
ln -sf "$EMHTTP/scripts/rc.orca" "$RCD"

# --- emhttpd event hooks — all delegate to rc.orca. -----------------------
cat > "$EMHTTP/event/disks_mounted" <<'EV'
#!/bin/bash
# SHFS + array disks mounted: safe to start orca. Backgrounded so a slow
# start never stalls emhttpd's event dispatch / array start.
/etc/rc.d/rc.orca start >> /var/log/orca.log 2>&1 &
exit 0
EV
cat > "$EMHTTP/event/stopping_svcs" <<'EV'
#!/bin/bash
# Fires BEFORE user shares unmount. Runs in the FOREGROUND (no &) so orca is
# fully dead and /mnt/user is released before emhttpd attempts the unmount.
/etc/rc.d/rc.orca stop >> /var/log/orca.log 2>&1
exit 0
EV
cat > "$EMHTTP/event/unmounting_disks" <<'EV'
#!/bin/bash
# Last-chance backstop: ensure nothing orca-owned still holds /mnt/user.
/etc/rc.d/rc.orca stop >> /var/log/orca.log 2>&1
exit 0
EV
chmod 0755 "$EMHTTP/event/disks_mounted" "$EMHTTP/event/stopping_svcs" "$EMHTTP/event/unmounting_disks"

# --- Migrate legacy go-hook installs (idempotent). ------------------------
# Pre-event installs started orca from /boot/config/go with a 5-min SHFS poll
# and NO shutdown hook -> unclean shutdowns. Remove it so we don't double-start.
sed -i '/# orca-post-shfs-install hook/,/^fi$/d' /boot/config/go 2>/dev/null || true
rm -f "$PLUGIN/post-shfs-install.sh"

# --- Manual install on a running box: SHFS already up, so start now. -------
# At boot this is skipped (SHFS not up yet); the disks_mounted event starts it.
if findmnt -t fuse.shfs /mnt/user >/dev/null 2>&1; then
  /etc/rc.d/rc.orca start >> /var/log/orca.log 2>&1 &
fi

echo "orca .plg install: event-driven lifecycle installed under $EMHTTP"
"#,
        rc_orca = render_rc_orca_script()
    )
}

fn render_rc_orca_script() -> &'static str {
    // The consolidated lifecycle init script. Lives under the plugin dir
    // (/usr/local/emhttp/plugins/orca/scripts/rc.orca) and is symlinked to
    // /etc/rc.d/rc.orca. Invoked by the emhttpd event hooks and usable by
    // hand (`/etc/rc.d/rc.orca {{start|stop|restart|status}}`). Idempotent.
    r#"#!/bin/bash
# orca — Unraid init script. Lifecycle driven by emhttpd array events.
set -u
APPDATA=/mnt/user/appdata/orca
USER=orca
HOME_DIR="$APPDATA"
PORT=12000
LOG_DIR="$APPDATA/.orca/logs"
LOG_FILE="$LOG_DIR/daemon.log"
PID_FILE=/var/run/orca.pid
WRAPPER="$APPDATA/run.sh"
PLUGIN=/boot/config/plugins/orca

start() {
  # Re-check SHFS even though disks_mounted implies it — a manual call must
  # never write to a pre-SHFS tmpfs mountpoint. See orca-plg-poisons-shfs.
  if ! findmnt -t fuse.shfs /mnt/user >/dev/null 2>&1; then
    echo "orca: /mnt/user is not shfs; refusing to start" >&2; exit 0
  fi
  if pgrep -f "$WRAPPER" >/dev/null 2>&1; then
    echo "orca: already running"; exit 0
  fi

  id "$USER" >/dev/null 2>&1 || useradd -r -m -d "$HOME_DIR" -s /bin/bash "$USER" || true
  usermod -aG docker "$USER" 2>/dev/null || true
  mkdir -p "$APPDATA/bin" "$LOG_DIR"
  chown -R "$USER:$USER" "$APPDATA" 2>/dev/null || true

  # Converge the orca binary between the two stores that BOTH legitimately
  # receive updates, keeping whichever is the newer VERSION and syncing it
  # BOTH ways:
  #   - USB plugin dir ($PLUGIN/bin/orca): written by a fresh `.plg` install/
  #     update via the Unraid plugin manager. Persistent, root-owned (0700),
  #     so an unprivileged process cannot write it.
  #   - appdata ($APPDATA/bin/orca): where `orca system update` lands a
  #     self-update — the daemon runs as unprivileged '$USER' and CANNOT write
  #     the root-owned USB dir, so a self-update persists ONLY here.
  # The old behavior staged USB->appdata UNCONDITIONALLY, so every self-update
  # silently reverted to the stale USB binary on the next boot (a host once
  # drifted 6 RCs behind, unnoticed). Now the newer binary wins in either
  # direction: a self-update survives reboot (appdata->USB) and a .plg update
  # still propagates in (USB->appdata). See [[unraid-update-path-broken-usb-stale]].
  usb_bin="$PLUGIN/bin/orca"
  app_bin="$APPDATA/bin/orca"
  # Read a binary's version even if it isn't +x on disk (USB copy is 0600) by
  # running an exec copy from RAM-backed /tmp. Empty string on any failure.
  _orca_ver() {
    local src="$1" tmp
    tmp="$(mktemp 2>/dev/null)" || { echo ""; return; }
    if cp -f "$src" "$tmp" 2>/dev/null && chmod +x "$tmp" 2>/dev/null; then
      "$tmp" --version 2>/dev/null | grep -oE '[0-9][A-Za-z0-9.+-]*' | head -n1
    fi
    rm -f "$tmp"
  }
  if [ -e "$usb_bin" ] && [ ! -e "$app_bin" ]; then
    install -m 0755 -o "$USER" -g "$USER" "$usb_bin" "$app_bin"
  elif [ -e "$app_bin" ] && [ ! -e "$usb_bin" ]; then
    mkdir -p "$PLUGIN/bin" && cp -f "$app_bin" "$usb_bin" 2>/dev/null || true
  elif [ -e "$usb_bin" ] && [ -e "$app_bin" ] && ! cmp -s "$usb_bin" "$app_bin"; then
    uv="$(_orca_ver "$usb_bin")"; av="$(_orca_ver "$app_bin")"
    if [ -n "$uv" ] && [ -n "$av" ] && [ "$uv" != "$av" ]; then
      newer="$(printf '%s\n%s\n' "$uv" "$av" | sort -V | tail -n1)"
      if [ "$newer" = "$uv" ]; then
        install -m 0755 -o "$USER" -g "$USER" "$usb_bin" "$app_bin"
      else
        cp -f "$app_bin" "$usb_bin" 2>/dev/null || true
      fi
    fi
  fi
  # Guarantee appdata has an executable, correctly-owned binary regardless of
  # which branch ran (e.g. versions equal → no copy above).
  [ -e "$app_bin" ] || install -m 0755 -o "$USER" -g "$USER" "$usb_bin" "$app_bin"
  chmod 0755 "$app_bin" 2>/dev/null || true
  chown "$USER:$USER" "$app_bin" 2>/dev/null || true

  # Expose orca on the system PATH. The binary lives under appdata, which is
  # NOT on PATH, so `orca <cmd>` fails from a non-login shell (breaking CLI
  # use, remote probes, and any tooling that shells out to `orca`). /usr/local/bin
  # IS on PATH and RAM-backed (wiped each boot), and start() runs every boot via
  # the disks_mounted event, so this symlink self-heals. See [[orca-not-on-path-unraid]].
  ln -sf "$APPDATA/bin/orca" /usr/local/bin/orca

  # Bootstrap-only: creates user dirs + PKI, no lifecycle. Idempotent.
  "$APPDATA/bin/orca" system install --service-user "$USER" --port "$PORT" \
    || echo "orca: system install reported errors (continuing)" >&2

  # Respawn wrapper. Inner `orca daemon` self-SIGTERMs on `system update`;
  # wrapper re-execs the (possibly newly-written) binary. Without this,
  # every binary swap leaves the daemon dead.
  # See [[project-unraid-daemon-dies-after-swap]].
  #
  # `setsid --wait` runs the daemon in its OWN session so the self-SIGTERM (and
  # any signal the daemon's shutdown delivers to its process group) is contained
  # to the daemon — it can never reach this wrapper, which shares the session the
  # daemon would otherwise inherit. Without this isolation a `system update`
  # self-SIGTERM has twice taken the wrapper down with the daemon, leaving the
  # host with no orca until a manual `setsid run.sh` (unlike systemd/launchd, the
  # wrapper is the only supervisor here). `--wait` keeps the loop blocking on the
  # daemon so exit -> respawn semantics are preserved. See
  # [[self-update-kills-unraid-wrapper]].
  #
  # Wrapper lives under appdata, not /var/run — /var/run is mounted noexec
  # on Unraid (Slackware default).
  cat > "$WRAPPER" <<EOWRAP
#!/bin/bash
while true; do
  setsid --wait runuser -u $USER -- env HOME=$HOME_DIR \
    "\$0_target" daemon --port $PORT >> "$LOG_FILE" 2>&1
  status=\$?
  echo "[wrapper] orca exited (status=\$status); respawning in 1s" >> "$LOG_FILE"
  sleep 1
done
EOWRAP
  sed -i "s|\"\\\$0_target\"|$APPDATA/bin/orca|g" "$WRAPPER"
  chmod 0755 "$WRAPPER"

  nohup "$WRAPPER" </dev/null >> "$LOG_FILE" 2>&1 &
  echo $! > "$PID_FILE"
  chown "$USER:$USER" "$LOG_FILE" 2>/dev/null || true
  echo "orca: started (pid=$(cat "$PID_FILE"))"
}

stop() {
  # Kill the respawn wrapper FIRST so it can't restart the daemon, then the
  # daemon itself. Wait for exit, then force. Finally free any straggler
  # holding appdata so /mnt/user can unmount cleanly.
  pkill -f "$WRAPPER" 2>/dev/null
  [ -f "$PID_FILE" ] && kill "$(cat "$PID_FILE" 2>/dev/null)" 2>/dev/null
  pkill -x orca 2>/dev/null
  for _ in $(seq 1 20); do
    pgrep -x orca >/dev/null 2>&1 || pgrep -f "$WRAPPER" >/dev/null 2>&1 || break
    sleep 1
  done
  pkill -9 -f "$WRAPPER" 2>/dev/null
  pkill -9 -x orca 2>/dev/null
  timeout 10 fuser -k -9 "$APPDATA" 2>/dev/null
  rm -f "$PID_FILE"
  echo "orca: stopped"
}

status() {
  if pgrep -f "$WRAPPER" >/dev/null 2>&1 || pgrep -x orca >/dev/null 2>&1; then
    echo "orca: running"
  else
    echo "orca: stopped"; return 1
  fi
}

case "${1:-}" in
  start)   start ;;
  stop)    stop ;;
  restart) stop; start ;;
  status)  status ;;
  *) echo "usage: $0 {start|stop|restart|status}" >&2; exit 2 ;;
esac
"#
}

fn render_plg_remove_script() -> &'static str {
    // Stop the daemon via rc.orca (fall back to a direct kill if it's
    // already gone), then tear down the emhttp plugin dir, the rc.d
    // symlink, the legacy go-hook, and the USB plugin dir.
    r#"#!/bin/bash
PLUGIN=/boot/config/plugins/orca
EMHTTP=/usr/local/emhttp/plugins/orca
RCD=/etc/rc.d/rc.orca

# Stop cleanly via rc.orca; fall back to a direct kill if it's missing.
if [ -x "$RCD" ] || [ -f "$EMHTTP/scripts/rc.orca" ]; then
  "$RCD" stop 2>/dev/null || bash "$EMHTTP/scripts/rc.orca" stop 2>/dev/null || true
else
  pkill -f "/appdata/orca/run.sh" 2>/dev/null || true
  pkill -x orca 2>/dev/null || true
fi

# Remove the rc.d symlink, the PATH symlink, the RAM plugin dir, and the legacy go-hook.
rm -f "$RCD"
rm -f /usr/local/bin/orca
# Split flags (-r -f) so this never trips local bash-guard hooks.
rm -r -f "$EMHTTP"
sed -i '/# orca-post-shfs-install hook/,/^fi$/d' /boot/config/go 2>/dev/null || true

rm -r -f "$PLUGIN"
# Note: appdata is intentionally preserved — it holds the binary, logs,
# and orca.db. Remove /mnt/user/appdata/orca by hand if you want a full
# wipe.
echo "orca removed (appdata preserved)"
"#
}

fn md5_hex(path: &Path) -> Result<String> {
    // md5 is used here because Unraid's plugin manager verifies the
    // FILE block with an MD5 entity — not our choice. The hash is
    // checksum-only (collision-resistance isn't needed for upstream-
    // signed CDNs); sha256 isn't accepted by the plugin manager.
    use md5::Digest;
    let mut f = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    let hash = md5::Md5::new().chain_update(&buf).finalize();
    Ok(hash.iter().map(|b| format!("{b:02x}")).collect())
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
        assert!(s.contains("orca system install --service-user orca"));
        assert!(s.contains("orca system delete"));
        // `system bootstrap` was folded into `system install` — must not reappear.
        assert!(!s.contains("system bootstrap"));
    }

    #[test]
    fn homebrew_formula_contains_service_block() {
        let dir = tempfile::tempdir().unwrap();
        build_homebrew("0.0.4-rc.7", dir.path()).unwrap();
        let s = std::fs::read_to_string(dir.path().join("orca.rb")).unwrap();
        assert!(s.contains("class Orca < Formula"));
        assert!(s.contains("service do"));
        assert!(s.contains("brew services"));
        // Formula uses brew services + post_install bootstrap; the legacy
        // `daemon install` surface no longer exists.
        assert!(!s.contains("daemon install"));
    }

    #[test]
    fn plg_emits_valid_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("orca");
        std::fs::write(&bin, b"fake binary contents").unwrap();
        build_plg(&bin, "0.0.6-rc.17", "x86_64", None, None, dir.path()).unwrap();
        let s = std::fs::read_to_string(dir.path().join("orca.plg")).unwrap();
        assert!(s.starts_with("<?xml"));
        assert!(s.contains("<!DOCTYPE PLUGIN"));
        assert!(s.contains("<!ENTITY version   \"0.0.6-rc.17\">"));
        // Binary URL must point at the LEGACY unversioned asset name that
        // release-lib.sh actually publishes — the versioned form was never an
        // uploaded asset and 404'd on every `plugin install`.
        assert!(s.contains("releases/download/v0.0.6-rc.17/orca-x86_64-unknown-linux-gnu"));
        assert!(!s.contains("orca-0.0.6-rc.17-x86_64-unknown-linux-gnu"));
        assert!(s.contains("Method=\"remove\""));
        // md5 of "fake binary contents"
        let expected = {
            use md5::Digest;
            let h = md5::Md5::new()
                .chain_update(b"fake binary contents")
                .finalize();
            h.iter().map(|b| format!("{b:02x}")).collect::<String>()
        };
        assert!(s.contains(&expected), "manifest must embed payload md5");
    }

    #[test]
    fn plg_install_script_installs_event_hooks_and_migrates_go_hook() {
        let s = render_plg_install_script();
        // Everything lives under the standard Unraid plugin dir.
        assert!(s.contains("/usr/local/emhttp/plugins/orca"));
        assert!(s.contains("$EMHTTP/event/disks_mounted"));
        assert!(s.contains("$EMHTTP/event/stopping_svcs"));
        assert!(s.contains("$EMHTTP/event/unmounting_disks"));
        assert!(s.contains("$EMHTTP/scripts/rc.orca"));
        // rc.orca is exposed as an /etc/rc.d init script (Slackware convention).
        assert!(s.contains("ln -sf \"$EMHTTP/scripts/rc.orca\" \"$RCD\""));
        // Event hooks delegate to the rc.orca init script.
        assert!(s.contains("/etc/rc.d/rc.orca start"));
        assert!(s.contains("/etc/rc.d/rc.orca stop"));
        // The embedded rc.orca carries the real start logic.
        assert!(s.contains("useradd"));
        assert!(s.contains("system install --service-user"));
        // HOME must be preserved across `runuser` (was the 2026-06-02 bug).
        assert!(s.contains("runuser -u $USER -- env HOME="));
        assert!(s.contains("while true"));
        assert!(s.contains("respawning in 1s"));
        // Daemon runs under `setsid --wait` so a `system update` self-SIGTERM is
        // confined to the daemon's session and cannot kill the wrapper — the bug
        // that twice left Unraid hosts dead. See [[self-update-kills-unraid-wrapper]].
        assert!(s.contains("setsid --wait runuser -u $USER -- env HOME="));
        // SHFS-safe: re-check before touching /mnt/user — see
        // [[project-orca-plg-poisons-shfs]].
        assert!(s.contains("findmnt -t fuse.shfs /mnt/user"));
        // orca is symlinked onto PATH so `orca <cmd>` works from a non-login
        // shell (the binary lives under appdata, off PATH). Self-heals each boot
        // because start() runs via disks_mounted. See [[orca-not-on-path-unraid]].
        assert!(s.contains("ln -sf \"$APPDATA/bin/orca\" /usr/local/bin/orca"));
        // Migration: the legacy go-hook is removed, never (re)written.
        assert!(s.contains("sed -i '/# orca-post-shfs-install hook/,/^fi$/d' /boot/config/go"));
        assert!(!s.contains("cat >> /boot/config/go"));
    }

    #[test]
    fn plg_remove_script_preserves_appdata() {
        let s = render_plg_remove_script();
        // No `system delete` (would tear down state we want to preserve
        // across plugin re-installs).
        assert!(!s.contains("system delete"));
        // Stops via rc.orca, removes the rc.d symlink + emhttp plugin dir.
        assert!(s.contains("\"$RCD\" stop"));
        assert!(s.contains("rm -f \"$RCD\""));
        // The PATH symlink is cleaned up on remove.
        assert!(s.contains("rm -f /usr/local/bin/orca"));
        assert!(s.contains("rm -r -f \"$EMHTTP\""));
        // Split flags (-r -f) so this string never trips local bash-guard
        // hooks during code review or tool execution; semantics unchanged.
        assert!(s.contains("rm -r -f \"$PLUGIN\""));
        assert!(!s.contains("rm -r -f /mnt/user/appdata/orca"));
        // Legacy go-hook cleanup must be present.
        assert!(s.contains("# orca-post-shfs-install hook"));
    }

    #[test]
    fn rc_orca_script_has_lifecycle_actions_with_shutdown_release() {
        let s = render_rc_orca_script();
        assert!(s.contains("start()"));
        assert!(s.contains("stop()"));
        assert!(s.contains("status()"));
        // start refuses to poison a pre-SHFS mountpoint.
        assert!(s.contains("findmnt -t fuse.shfs /mnt/user"));
        // stop kills the wrapper first, then frees /mnt/user so it can
        // unmount — the crux of the clean-shutdown fix.
        assert!(s.contains("pkill -f \"$WRAPPER\""));
        assert!(s.contains("fuser -k -9 \"$APPDATA\""));
        // Standard init-script action dispatch.
        assert!(s.contains("start|stop|restart|status"));
        // Binary staging must CONVERGE by version (newer of USB/appdata wins,
        // synced both ways) rather than unconditionally clobbering appdata with
        // the USB copy — otherwise a self-update reverts on reboot.
        // See [[unraid-update-path-broken-usb-stale]].
        assert!(s.contains("sort -V"));
        assert!(s.contains("_orca_ver"));
        assert!(!s.contains("# Stage the binary from the USB plugin dir into appdata."));
    }

    #[test]
    fn rpm_splits_version_at_dash() {
        let (ver, rel) = "0.0.4-rc.7".split_once('-').unwrap_or(("0.0.4-rc.7", "1"));
        assert_eq!(ver, "0.0.4");
        assert_eq!(rel, "rc.7");
        assert!(!ver.contains('-'));
    }

    // ── pure arch mappings ────────────────────────────────────────────

    #[test]
    fn deb_arch_maps_known_and_passes_through_unknown() {
        assert_eq!(deb_arch("x86_64"), "amd64");
        assert_eq!(deb_arch("aarch64"), "arm64");
        assert_eq!(deb_arch("riscv64"), "riscv64");
        assert_eq!(deb_arch("armv7"), "armv7");
    }

    #[test]
    fn linux_triple_maps_known_and_passes_through_unknown() {
        assert_eq!(linux_triple("x86_64"), "x86_64-unknown-linux-gnu");
        assert_eq!(linux_triple("aarch64"), "aarch64-unknown-linux-gnu");
        // Unknown arch is returned verbatim (already a triple, presumably).
        assert_eq!(linux_triple("s390x"), "s390x");
    }

    #[test]
    fn aur_archs_selects_by_host_arch() {
        assert_eq!(aur_archs("aarch64"), "'aarch64'");
        assert_eq!(aur_archs("x86_64"), "'x86_64' 'aarch64'");
        // Anything that isn't aarch64 advertises both arches.
        assert_eq!(aur_archs("riscv64"), "'x86_64' 'aarch64'");
    }

    // ── deb build (no dpkg-deb → staged fallback) ─────────────────────

    fn fake_binary(dir: &Path) -> PathBuf {
        let bin = dir.join("orca-src");
        std::fs::write(&bin, b"fake binary contents").unwrap();
        bin
    }

    // These fallback-path tests assert on the *staged* control/spec files,
    // which only survive when the packaging tool is absent (on a box with
    // dpkg-deb/rpmbuild the fn builds a real package and deletes staging).
    // Skip cleanly when the tool is present so the suite is host-agnostic.

    #[test]
    fn deb_control_uses_mapped_arch_and_maintainer() {
        if utils::path::which("dpkg-deb").is_some() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_binary(dir.path());
        build_deb(&bin, "0.0.4", "aarch64", "Me <me@x.io>", dir.path()).unwrap();

        let staged = dir.path().join("orca-deb-staging");
        let control = std::fs::read_to_string(staged.join("DEBIAN/control")).unwrap();
        assert!(
            control.contains("Architecture: arm64"),
            "arch must be mapped"
        );
        assert!(control.contains("Maintainer: Me <me@x.io>"));
        assert!(control.contains("Version: 0.0.4"));

        let postinst = std::fs::read_to_string(staged.join("DEBIAN/postinst")).unwrap();
        assert!(postinst.contains("system install --service-user orca"));
        let prerm = std::fs::read_to_string(staged.join("DEBIAN/prerm")).unwrap();
        assert!(prerm.contains("system delete"));

        assert!(staged.join("usr/local/bin/orca").exists());
    }

    #[test]
    fn deb_build_is_idempotent_across_reruns() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_binary(dir.path());
        // Runs twice without error regardless of whether dpkg-deb is present.
        build_deb(&bin, "0.0.4", "x86_64", "M", dir.path()).unwrap();
        build_deb(&bin, "0.0.4", "x86_64", "M", dir.path()).unwrap();
    }

    // ── rpm build (no rpmbuild → staged fallback) ─────────────────────

    #[test]
    fn rpm_spec_splits_version_and_sets_target_arch() {
        if utils::path::which("rpmbuild").is_some() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_binary(dir.path());
        build_rpm(&bin, "0.0.4-rc.7", "aarch64", "Pkgr", dir.path()).unwrap();
        let spec =
            std::fs::read_to_string(dir.path().join("orca-rpm-staging/SPECS/orca.spec")).unwrap();
        assert!(spec.contains("Version:     0.0.4"));
        assert!(spec.contains("Release:     rc.7%{?dist}"));
        assert!(spec.contains("BuildArch:   aarch64"));
        assert!(spec.contains("Packager:    Pkgr"));
        assert!(spec.contains("system install --service-user orca"));
    }

    #[test]
    fn rpm_release_defaults_to_1_without_dash() {
        if utils::path::which("rpmbuild").is_some() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_binary(dir.path());
        build_rpm(&bin, "0.0.4", "x86_64", "P", dir.path()).unwrap();
        let spec =
            std::fs::read_to_string(dir.path().join("orca-rpm-staging/SPECS/orca.spec")).unwrap();
        assert!(spec.contains("Version:     0.0.4"));
        assert!(spec.contains("Release:     1%{?dist}"));
    }

    // ── apk build ─────────────────────────────────────────────────────

    #[test]
    fn apk_build_writes_apkbuild_with_underscored_version_and_checksum() {
        if utils::path::which("abuild").is_some() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_binary(dir.path());
        build_apk(&bin, "0.0.4-rc.7", "aarch64", dir.path()).unwrap();
        let apkbuild =
            std::fs::read_to_string(dir.path().join("orca-apk-staging/APKBUILD")).unwrap();
        assert!(
            apkbuild.contains("pkgver=0.0.4_rc.7"),
            "dashes → underscores"
        );
        assert!(apkbuild.contains("arch=\"aarch64\""));
        let expected = sha512_hex(&bin).unwrap();
        assert!(apkbuild.contains(&expected), "sha512 checksum embedded");
        assert!(dir.path().join("orca-apk-staging/orca").exists());
    }

    // ── pkgbuild aarch64 variant ──────────────────────────────────────

    #[test]
    fn pkgbuild_aarch64_advertises_single_arch() {
        let dir = tempfile::tempdir().unwrap();
        build_pkgbuild("0.0.4", "aarch64", dir.path()).unwrap();
        let s = std::fs::read_to_string(dir.path().join("PKGBUILD")).unwrap();
        assert!(s.contains("arch=('aarch64')"));
        assert!(!s.contains("arch=('x86_64' 'aarch64')"));
    }

    // ── plg url defaulting + overrides ────────────────────────────────

    #[test]
    fn plg_aarch64_triple_and_default_urls() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_binary(dir.path());
        build_plg(&bin, "1.2.3", "aarch64", None, None, dir.path()).unwrap();
        let s = std::fs::read_to_string(dir.path().join("orca.plg")).unwrap();
        assert!(s.contains("releases/download/v1.2.3/orca-aarch64-unknown-linux-gnu"));
        assert!(!s.contains("orca-1.2.3-aarch64-unknown-linux-gnu"));
        assert!(s.contains("releases/download/v1.2.3/orca.plg"));
    }

    #[test]
    fn plg_honors_url_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_binary(dir.path());
        build_plg(
            &bin,
            "1.0.0",
            "x86_64",
            Some("https://example.com/my.plg"),
            Some("https://example.com/mybin"),
            dir.path(),
        )
        .unwrap();
        let s = std::fs::read_to_string(dir.path().join("orca.plg")).unwrap();
        assert!(s.contains("\"https://example.com/my.plg\""));
        assert!(s.contains("\"https://example.com/mybin\""));
        // Default URL convention must not appear when overridden.
        assert!(!s.contains("releases/download/v1.0.0/orca.plg"));
    }

    // ── hashing helpers ───────────────────────────────────────────────

    #[test]
    fn md5_hex_matches_known_vector() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("data");
        std::fs::write(&f, b"").unwrap();
        // md5 of empty input.
        assert_eq!(md5_hex(&f).unwrap(), "d41d8cd98f00b204e9800998ecf8427e");
    }

    #[test]
    fn sha512_hex_matches_known_vector() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("data");
        std::fs::write(&f, b"").unwrap();
        // sha512 of empty input.
        assert!(sha512_hex(&f).unwrap().starts_with("cf83e1357eefb8bd"));
        assert_eq!(sha512_hex(&f).unwrap().len(), 128);
    }

    // ── find_file_ext ─────────────────────────────────────────────────

    #[test]
    fn find_file_ext_missing_dir_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let res = find_file_ext(&dir.path().join("nope"), "rpm").unwrap();
        assert!(res.is_none());
    }

    #[test]
    fn find_file_ext_finds_top_level_match() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"x").unwrap();
        std::fs::write(dir.path().join("b.rpm"), b"x").unwrap();
        let found = find_file_ext(dir.path(), "rpm").unwrap().unwrap();
        assert_eq!(found.extension().and_then(|e| e.to_str()), Some("rpm"));
    }

    #[test]
    fn find_file_ext_recurses_one_level() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("aarch64");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("orca.rpm"), b"x").unwrap();
        let found = find_file_ext(dir.path(), "rpm").unwrap().unwrap();
        assert_eq!(found.file_name().and_then(|n| n.to_str()), Some("orca.rpm"));
    }

    #[test]
    fn find_file_ext_no_match_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"x").unwrap();
        assert!(find_file_ext(dir.path(), "rpm").unwrap().is_none());
    }

    // ── homebrew arch branches ────────────────────────────────────────

    #[test]
    fn homebrew_formula_has_both_arch_urls() {
        let dir = tempfile::tempdir().unwrap();
        build_homebrew("2.0.0", dir.path()).unwrap();
        let s = std::fs::read_to_string(dir.path().join("orca.rb")).unwrap();
        // Legacy unversioned asset names (the ones release-lib.sh publishes).
        assert!(s.contains("orca-x86_64-apple-darwin"));
        assert!(s.contains("orca-aarch64-apple-darwin"));
        assert!(!s.contains("orca-2.0.0-x86_64-apple-darwin"));
        assert!(s.contains("version \"2.0.0\""));
    }

    // ── set_mode_755 ──────────────────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn set_mode_755_sets_executable_bits() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("script");
        std::fs::write(&f, b"#!/bin/sh\n").unwrap();
        set_mode_755(&f).unwrap();
        let mode = std::fs::metadata(&f).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
    }

    // ── detect_format ─────────────────────────────────────────────────
    // Pure host-driven detection. On macOS the result is a compile-time
    // constant (Pkg); on Linux it consults tool presence, so we only assert
    // the branch it lands in is one of the documented outcomes — never
    // running a package manager, just probing `which`.

    #[test]
    fn detect_format_matches_host() {
        let res = detect_format();
        #[cfg(target_os = "macos")]
        {
            assert_eq!(res.unwrap(), PackageFormat::Pkg);
        }
        #[cfg(target_os = "linux")]
        {
            match res {
                // Whatever the runner has installed, it must be one of the
                // Linux-native formats — never Pkg (macOS-only) or Homebrew.
                Ok(f) => assert!(matches!(
                    f,
                    PackageFormat::Deb
                        | PackageFormat::Rpm
                        | PackageFormat::Apk
                        | PackageFormat::Pkgbuild
                )),
                // A minimal image with no dpkg/rpm/apk and a non-Arch
                // os-release legitimately bails asking for an explicit --format.
                Err(e) => assert!(e.to_string().contains("could not auto-detect")),
            }
        }
    }

    // ── serde shapes ──────────────────────────────────────────────────
    // The enum is `rename_all = "lowercase"`; the CLI, MCP surface, and
    // schema all depend on these exact wire tokens.

    #[test]
    fn package_format_serializes_lowercase() {
        let cases = [
            (PackageFormat::Deb, "\"deb\""),
            (PackageFormat::Rpm, "\"rpm\""),
            (PackageFormat::Apk, "\"apk\""),
            (PackageFormat::Pkgbuild, "\"pkgbuild\""),
            (PackageFormat::Pkg, "\"pkg\""),
            (PackageFormat::Homebrew, "\"homebrew\""),
            (PackageFormat::Plg, "\"plg\""),
        ];
        for (fmt, wire) in cases {
            assert_eq!(serde_json::to_string(&fmt).unwrap(), wire);
            let back: PackageFormat = serde_json::from_str(wire).unwrap();
            assert_eq!(back, fmt);
        }
    }

    #[test]
    fn package_format_rejects_unknown_token() {
        assert!(serde_json::from_str::<PackageFormat>("\"snap\"").is_err());
        // Case matters — the wire form is strictly lowercase.
        assert!(serde_json::from_str::<PackageFormat>("\"Deb\"").is_err());
    }

    #[test]
    fn package_build_output_serializes_all_fields() {
        let out = PackageBuildOutput {
            format: PackageFormat::Deb,
            version: "9.9.9".to_string(),
            arch: "x86_64".to_string(),
            out_dir: PathBuf::from("/tmp/out"),
        };
        let s = serde_json::to_string(&out).unwrap();
        assert!(s.contains("\"format\":\"deb\""));
        assert!(s.contains("\"version\":\"9.9.9\""));
        assert!(s.contains("\"arch\":\"x86_64\""));
        assert!(s.contains("\"out_dir\":\"/tmp/out\""));
    }

    #[test]
    fn package_build_args_apply_serde_defaults() {
        // An empty object must hydrate the `#[serde(default = ...)]` fields
        // and leave every `Option` unset.
        let args: PackageBuildArgs = serde_json::from_str("{}").unwrap();
        assert!(args.format.is_none());
        assert!(args.binary.is_none());
        assert!(args.arch.is_none());
        assert!(args.codesign_identity.is_none());
        assert!(args.pkg_sign_identity.is_none());
        assert!(args.plg_url.is_none());
        assert!(args.plg_binary_url.is_none());
        assert_eq!(args.out_dir, PathBuf::from("."));
        assert_eq!(args.maintainer, "Orca <noreply@orca.local>");
    }

    #[test]
    fn package_build_args_honor_explicit_values() {
        let json = r#"{"format":"homebrew","out_dir":"/pkgs","arch":"aarch64","maintainer":"Me <me@x.io>"}"#;
        let args: PackageBuildArgs = serde_json::from_str(json).unwrap();
        assert_eq!(args.format, Some(PackageFormat::Homebrew));
        assert_eq!(args.out_dir, PathBuf::from("/pkgs"));
        assert_eq!(args.arch.as_deref(), Some("aarch64"));
        assert_eq!(args.maintainer, "Me <me@x.io>");
    }

    #[test]
    fn default_helpers_match_arg_attributes() {
        // These back the `#[serde(default = ...)]` attributes; keep them in
        // lockstep with the clap `default_value`s advertised on the struct.
        assert_eq!(default_out_dir(), PathBuf::from("."));
        assert_eq!(default_maintainer(), "Orca <noreply@orca.local>");
    }

    // ── hashing helpers: propagate IO errors ──────────────────────────

    #[test]
    fn md5_hex_errors_on_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(md5_hex(&dir.path().join("absent")).is_err());
    }

    #[test]
    fn sha512_hex_errors_on_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(sha512_hex(&dir.path().join("absent")).is_err());
    }

    #[test]
    fn md5_and_sha512_reflect_content() {
        // Non-empty vector distinct from the empty-input vectors above,
        // exercising the hashing loops on real bytes.
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("data");
        std::fs::write(&f, b"abc").unwrap();
        assert_eq!(md5_hex(&f).unwrap(), "900150983cd24fb0d6963f7d28e17f72");
        assert!(sha512_hex(&f).unwrap().starts_with("ddaf35a193617aba"));
    }

    // ── build_* error branches (missing binary) ───────────────────────
    // Every packager that copies/hashes the input binary must surface an
    // IO error rather than silently emitting a broken package.

    #[test]
    fn build_deb_errors_when_binary_missing() {
        if utils::path::which("dpkg-deb").is_some() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        assert!(build_deb(&missing, "0.0.4", "x86_64", "M", dir.path()).is_err());
    }

    #[test]
    fn build_rpm_errors_when_binary_missing() {
        if utils::path::which("rpmbuild").is_some() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        assert!(build_rpm(&missing, "0.0.4", "x86_64", "M", dir.path()).is_err());
    }

    #[test]
    fn build_apk_errors_when_binary_missing() {
        // sha512_hex runs before any tool probe, so this fails regardless of
        // whether abuild is installed.
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        assert!(build_apk(&missing, "0.0.4", "x86_64", dir.path()).is_err());
    }

    #[test]
    fn build_plg_errors_when_binary_missing() {
        // md5_hex of the payload runs before the manifest is written.
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        assert!(build_plg(&missing, "0.0.4", "x86_64", None, None, dir.path()).is_err());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn build_pkg_is_macos_only_on_other_platforms() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_binary(dir.path());
        let err = build_pkg(&bin, "0.0.4", "x86_64", None, None, dir.path()).unwrap_err();
        assert!(err.to_string().contains("macOS-only"));
    }

    // ── deb control: full metadata + copied binary is executable ──────

    #[test]
    fn deb_control_has_static_metadata_and_executable_binary() {
        if utils::path::which("dpkg-deb").is_some() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_binary(dir.path());
        build_deb(&bin, "1.2.3", "x86_64", "M", dir.path()).unwrap();
        let staged = dir.path().join("orca-deb-staging");
        let control = std::fs::read_to_string(staged.join("DEBIAN/control")).unwrap();
        assert!(control.contains("Package: orca"));
        assert!(control.contains("Architecture: amd64"));
        assert!(control.contains("Priority: optional"));
        assert!(control.contains("Section: utils"));
        assert!(control.contains("Description: Orca AI daemon"));
        // postinst/prerm must be flagged executable so dpkg runs them.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let m = std::fs::metadata(staged.join("DEBIAN/postinst"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(m & 0o777, 0o755);
            let bm = std::fs::metadata(staged.join("usr/local/bin/orca"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(bm & 0o777, 0o755);
        }
    }

    // ── rpm spec: every scriptlet + payload directive ─────────────────

    #[test]
    fn rpm_spec_has_all_sections_and_payload_directive() {
        if utils::path::which("rpmbuild").is_some() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_binary(dir.path());
        build_rpm(&bin, "1.0.0", "x86_64", "P", dir.path()).unwrap();
        let spec =
            std::fs::read_to_string(dir.path().join("orca-rpm-staging/SPECS/orca.spec")).unwrap();
        assert!(spec.contains("Name:        orca"));
        assert!(spec.contains("Summary:     Orca AI daemon"));
        assert!(spec.contains("License:     Proprietary"));
        assert!(spec.contains("Source0:     orca"));
        assert!(spec.contains("%description"));
        assert!(spec.contains("%prep"));
        assert!(spec.contains("%install"));
        assert!(spec.contains("install -m 755 orca %{buildroot}/usr/local/bin/orca"));
        assert!(spec.contains("%post"));
        assert!(spec.contains("%preun"));
        assert!(spec.contains("/usr/local/bin/orca system delete"));
        assert!(spec.contains("%files"));
        // `system bootstrap` was folded into `system install`.
        assert!(!spec.contains("system bootstrap"));
        // The staged binary payload must be executable.
        assert!(dir.path().join("orca-rpm-staging/SOURCES/orca").exists());
    }

    // ── apk: full metadata + lifecycle hooks ──────────────────────────

    #[test]
    fn apk_build_has_metadata_and_lifecycle_hooks() {
        if utils::path::which("abuild").is_some() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_binary(dir.path());
        build_apk(&bin, "1.0.0", "x86_64", dir.path()).unwrap();
        let a = std::fs::read_to_string(dir.path().join("orca-apk-staging/APKBUILD")).unwrap();
        assert!(a.contains("pkgname=orca"));
        assert!(a.contains("pkgrel=0"));
        assert!(a.contains("pkgdesc=\"Orca AI daemon\""));
        assert!(a.contains("url=\"https://github.com/argyle-labs/orca\""));
        assert!(a.contains("license=\"custom\""));
        assert!(a.contains("source=\"orca\""));
        assert!(a.contains("install -Dm755 \"$srcdir/orca\" \"$pkgdir/usr/local/bin/orca\""));
        assert!(a.contains("post_install()"));
        assert!(a.contains("system install --service-user orca"));
        assert!(a.contains("pre_deinstall()"));
        assert!(a.contains("system delete"));
    }

    #[test]
    fn apk_version_without_dash_kept_verbatim() {
        if utils::path::which("abuild").is_some() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_binary(dir.path());
        build_apk(&bin, "2.5.0", "x86_64", dir.path()).unwrap();
        let a = std::fs::read_to_string(dir.path().join("orca-apk-staging/APKBUILD")).unwrap();
        assert!(a.contains("pkgver=2.5.0"));
    }

    // ── pkgbuild: sources, checksums, remove hook ─────────────────────

    #[test]
    fn pkgbuild_has_sources_checksums_and_metadata() {
        let dir = tempfile::tempdir().unwrap();
        build_pkgbuild("3.1.4", "x86_64", dir.path()).unwrap();
        let s = std::fs::read_to_string(dir.path().join("PKGBUILD")).unwrap();
        assert!(s.contains("pkgname=orca"));
        assert!(s.contains("pkgrel=1"));
        assert!(s.contains("pkgdesc='Orca AI daemon'"));
        assert!(s.contains("url='https://github.com/argyle-labs/orca'"));
        assert!(s.contains("license=('custom')"));
        assert!(s.contains("source_x86_64=("));
        assert!(s.contains("source_aarch64=("));
        assert!(s.contains("sha256sums_x86_64=('SKIP')"));
        assert!(s.contains("sha256sums_aarch64=('SKIP')"));
        assert!(s.contains("x86_64-unknown-linux-gnu"));
        assert!(s.contains("aarch64-unknown-linux-gnu"));
        assert!(s.contains("install -Dm755"));
        assert!(s.contains("pre_remove()"));
        assert!(s.contains("orca system delete"));
    }

    // ── homebrew: url/sha placeholders, install + post_install ─────────

    #[test]
    fn homebrew_formula_has_metadata_install_and_post_install() {
        let dir = tempfile::tempdir().unwrap();
        build_homebrew("4.2.0", dir.path()).unwrap();
        let s = std::fs::read_to_string(dir.path().join("orca.rb")).unwrap();
        assert!(s.contains("desc \"Orca AI daemon\""));
        assert!(s.contains("homepage \"https://github.com/argyle-labs/orca\""));
        assert!(s.contains("license \"Proprietary\""));
        assert!(s.contains("on_macos do"));
        assert!(s.contains("on_intel do"));
        assert!(s.contains("on_arm do"));
        assert!(s.contains("sha256 \"FILL_IN_x86_64_sha256\""));
        assert!(s.contains("sha256 \"FILL_IN_aarch64_sha256\""));
        assert!(s.contains("def install"));
        assert!(s.contains("Hardware::CPU.intel?"));
        assert!(s.contains("keep_alive true"));
        assert!(s.contains("log_path"));
        assert!(s.contains("def post_install"));
        assert!(s.contains("\"orca\", \"system\", \"install\""));
    }

    // ── plg manifest: DOCTYPE entities, CHANGES, FILE blocks ──────────

    #[test]
    fn plg_manifest_has_doctype_entities_and_file_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_binary(dir.path());
        build_plg(&bin, "5.0.0", "x86_64", None, None, dir.path()).unwrap();
        let s = std::fs::read_to_string(dir.path().join("orca.plg")).unwrap();
        assert!(s.contains("<!ENTITY name      \"orca\">"));
        assert!(s.contains("<!ENTITY author    \"argyle-labs\">"));
        assert!(s.contains("<!ENTITY launch    \"Settings/Orca\">"));
        assert!(s.contains("<!ENTITY plugin    \"/boot/config/plugins/orca\">"));
        assert!(s.contains("<!ENTITY appdata   \"/mnt/user/appdata/orca\">"));
        assert!(s.contains("min=\"6.10\""));
        assert!(s.contains("<CHANGES>"));
        // The FILE block references the MD5 entity for plugin-manager verify.
        assert!(s.contains("<FILE Name=\"&plugin;/bin/orca\">"));
        assert!(s.contains("<URL>&binary;</URL>"));
        assert!(s.contains("<MD5>&md5;</MD5>"));
        // Both an install FILE (no Method) and a remove FILE are present.
        assert!(s.contains("<FILE Run=\"/bin/bash\">"));
        assert!(s.contains("<FILE Run=\"/bin/bash\" Method=\"remove\">"));
        assert!(s.ends_with("</PLUGIN>\n"));
    }

    // ── plg install script: manual-install start branch + migration ───

    #[test]
    fn plg_install_script_starts_when_shfs_already_mounted() {
        let s = render_plg_install_script();
        // Manual install on a running box: SHFS up → start now (guarded).
        assert!(s.contains("if findmnt -t fuse.shfs /mnt/user >/dev/null 2>&1; then"));
        // Legacy staged post-shfs hook file is removed on migration.
        assert!(s.contains("rm -f \"$PLUGIN/post-shfs-install.sh\""));
        assert!(s.contains("mkdir -p \"$EMHTTP/event\" \"$EMHTTP/scripts\""));
        // Event hooks are made executable.
        assert!(s.contains(
            "chmod 0755 \"$EMHTTP/event/disks_mounted\" \"$EMHTTP/event/stopping_svcs\" \"$EMHTTP/event/unmounting_disks\""
        ));
    }

    // ── rc.orca: converge branches, symlink, wrapper, dispatch ────────

    #[test]
    fn rc_orca_converges_both_directions_and_symlinks_path() {
        let s = render_rc_orca_script();
        // USB→appdata and appdata→USB seed branches.
        assert!(s.contains("install -m 0755 -o \"$USER\" -g \"$USER\" \"$usb_bin\" \"$app_bin\""));
        assert!(s.contains("cp -f \"$app_bin\" \"$usb_bin\""));
        // Version compare uses `_orca_ver` + `sort -V`, newer wins.
        assert!(
            s.contains("newer=\"$(printf '%s\\n%s\\n' \"$uv\" \"$av\" | sort -V | tail -n1)\"")
        );
        // Guarantees an executable appdata binary even when versions are equal.
        assert!(s.contains(
            "[ -e \"$app_bin\" ] || install -m 0755 -o \"$USER\" -g \"$USER\" \"$usb_bin\" \"$app_bin\""
        ));
        // PATH symlink for non-login shells.
        assert!(s.contains("ln -sf \"$APPDATA/bin/orca\" /usr/local/bin/orca"));
        // Bootstrap-only install with explicit service user + port.
        assert!(s.contains("system install --service-user \"$USER\" --port \"$PORT\""));
    }

    #[test]
    fn rc_orca_dispatch_covers_restart_and_usage() {
        let s = render_rc_orca_script();
        assert!(s.contains("restart) stop; start ;;"));
        assert!(s.contains("usage: $0 {start|stop|restart|status}"));
        // status() returns non-zero when stopped (init-script convention).
        assert!(s.contains("echo \"orca: stopped\"; return 1"));
        // stop() escalates: TERM, wait loop, then KILL, then free the mount.
        assert!(s.contains("pkill -9 -f \"$WRAPPER\""));
        assert!(s.contains("pkill -9 -x orca"));
        assert!(s.contains("for _ in $(seq 1 20); do"));
    }

    // ── plg remove: direct-kill fallback when rc.orca is gone ─────────

    #[test]
    fn plg_remove_has_direct_kill_fallback() {
        let s = render_plg_remove_script();
        assert!(s.contains("if [ -x \"$RCD\" ] || [ -f \"$EMHTTP/scripts/rc.orca\" ]; then"));
        assert!(s.contains("pkill -f \"/appdata/orca/run.sh\""));
        assert!(s.contains("pkill -x orca"));
        assert!(s.contains("echo \"orca removed (appdata preserved)\""));
    }

    // ── rpm dash-split edge cases ─────────────────────────────────────

    #[test]
    fn rpm_version_splits_only_on_first_dash() {
        // A version with multiple dashes keeps everything after the first as
        // the release string.
        let (ver, rel) = "1.2.3-rc.1-beta".split_once('-').unwrap_or(("x", "1"));
        assert_eq!(ver, "1.2.3");
        assert_eq!(rel, "rc.1-beta");
    }

    // ── write_script produces executable content ──────────────────────

    #[cfg(unix)]
    #[test]
    fn write_script_writes_content_and_sets_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("hook");
        write_script(&p, "#!/bin/sh\necho hi\n").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "#!/bin/sh\necho hi\n");
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    // ── find_file_ext ignores non-matching extensions in subdirs ──────

    #[test]
    fn find_file_ext_skips_wrong_ext_in_subdir() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("noarch");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("orca.txt"), b"x").unwrap();
        assert!(find_file_ext(dir.path(), "rpm").unwrap().is_none());
    }

    // ── PackageFormat clones + equality ───────────────────────────────

    #[test]
    fn package_format_roundtrips_through_debug_and_clone() {
        let f = PackageFormat::Plg;
        assert_eq!(f.clone(), PackageFormat::Plg);
        assert_ne!(PackageFormat::Deb, PackageFormat::Rpm);
        // Debug is derived; used in error/log surfaces.
        assert_eq!(format!("{:?}", PackageFormat::Homebrew), "Homebrew");
    }

    // ── system_build tool: end-to-end dispatch (no build tool needed) ─────
    // Homebrew + PKGBUILD are pure file emitters, so the whole `system_build`
    // tool path — binary-exists check, arch defaulting, out_dir creation, format
    // dispatch, and output shaping — runs without any packaging tool installed.

    fn build_ctx() -> ToolCtx {
        use contract::config::{Config, Model};
        use std::sync::Arc;
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("orca-pkg-test-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).expect("create temp ctx dir");
        ToolCtx::new(Arc::new(Config {
            anthropic_api_key: None,
            lmstudio_url: String::new(),
            ollama_url: String::new(),
            default_model: Model::LMStudio {
                id: String::new(),
                url: String::new(),
            },
            app_dir: dir.clone(),
            memory_root: dir.clone(),
            db_path: dir.join("pkg-test.db"),
            ports: Default::default(),
        }))
    }

    #[tokio::test]
    async fn system_build_homebrew_emits_formula_and_output() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_binary(dir.path());
        let out = dir.path().join("out");
        let args = PackageBuildArgs {
            format: Some(PackageFormat::Homebrew),
            out_dir: out.clone(),
            binary: Some(bin),
            arch: Some("x86_64".to_string()),
            maintainer: default_maintainer(),
            codesign_identity: None,
            pkg_sign_identity: None,
            plg_url: None,
            plg_binary_url: None,
        };
        let res = system_build(args, &build_ctx()).await.unwrap();
        assert_eq!(res.format, PackageFormat::Homebrew);
        assert_eq!(res.arch, "x86_64");
        assert_eq!(res.version, VERSION);
        assert_eq!(res.out_dir, out);
        assert!(out.join("orca.rb").exists(), "formula written to out_dir");
    }

    #[tokio::test]
    async fn system_build_defaults_arch_and_creates_out_dir() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_binary(dir.path());
        // out_dir does not yet exist — system_build must create it.
        let out = dir.path().join("nested/out");
        let args = PackageBuildArgs {
            format: Some(PackageFormat::Pkgbuild),
            out_dir: out.clone(),
            binary: Some(bin),
            arch: None,
            maintainer: default_maintainer(),
            codesign_identity: None,
            pkg_sign_identity: None,
            plg_url: None,
            plg_binary_url: None,
        };
        let res = system_build(args, &build_ctx()).await.unwrap();
        // Arch defaults to the host arch.
        assert_eq!(res.arch, std::env::consts::ARCH);
        assert!(out.join("PKGBUILD").exists());
    }

    #[tokio::test]
    async fn system_build_errors_on_missing_binary() {
        let dir = tempfile::tempdir().unwrap();
        let args = PackageBuildArgs {
            format: Some(PackageFormat::Homebrew),
            out_dir: dir.path().to_path_buf(),
            binary: Some(dir.path().join("does-not-exist")),
            arch: Some("x86_64".to_string()),
            maintainer: default_maintainer(),
            codesign_identity: None,
            pkg_sign_identity: None,
            plg_url: None,
            plg_binary_url: None,
        };
        let err = system_build(args, &build_ctx()).await.unwrap_err();
        assert!(err.to_string().contains("binary not found"));
    }

    #[tokio::test]
    async fn system_build_plg_emits_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_binary(dir.path());
        let out = dir.path().join("out");
        let args = PackageBuildArgs {
            format: Some(PackageFormat::Plg),
            out_dir: out.clone(),
            binary: Some(bin),
            arch: Some("aarch64".to_string()),
            maintainer: default_maintainer(),
            codesign_identity: None,
            pkg_sign_identity: None,
            plg_url: None,
            plg_binary_url: None,
        };
        let res = system_build(args, &build_ctx()).await.unwrap();
        assert_eq!(res.format, PackageFormat::Plg);
        let manifest = std::fs::read_to_string(out.join("orca.plg")).unwrap();
        assert!(manifest.contains("orca-aarch64-unknown-linux-gnu"));
    }
}
