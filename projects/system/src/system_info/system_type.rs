//! Canonical system-type detection + capability tables.
//!
//! Every host has exactly ONE `system_type` value (e.g. `"proxmox-ve"`).
//! `expected_capabilities(t)` returns the capability profile we'd expect to
//! observe for that type; the daemon separately reports
//! `detected_capabilities`. The UI diffs the two to surface anomaly badges
//! (e.g. PBS host with running VMs → `unexpected: vm-host`).
//!
//! See `project_system_type_detection.md` for the model.

use std::path::Path;

/// Trait for filesystem + OS introspection so the detector is testable
/// without root, without proxmox/unraid being installed, and without
/// running on linux.
///
/// Production uses `RealHostFs` (below). Tests construct a `FakeHostFs`
/// with the markers they want present.
pub trait HostFs {
    /// True when the path exists (file or dir; we don't care which).
    fn exists(&self, path: &Path) -> bool;
    /// `System::name()` (sysinfo) — the OS family name as the kernel reports
    /// it. `"Linux"`, `"Darwin"`, `"Unraid OS"`, etc.
    fn os_name(&self) -> Option<String>;
    /// `System::long_os_version()` — distro long name. `"Debian GNU/Linux 13"`,
    /// `"Alpine Linux v3.20"`, `"TrueNAS-SCALE-24.10.0"`, etc.
    fn os_long(&self) -> Option<String>;
    /// True when `name` resolves to an executable on `PATH`.
    fn which(&self, name: &str) -> bool;
}

pub struct RealHostFs;

impl HostFs for RealHostFs {
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
    fn os_name(&self) -> Option<String> {
        sysinfo::System::name()
    }
    fn os_long(&self) -> Option<String> {
        sysinfo::System::long_os_version().filter(|s| !s.is_empty())
    }
    fn which(&self, name: &str) -> bool {
        let Some(path) = std::env::var_os("PATH") else {
            return false;
        };
        std::env::split_paths(&path).any(|dir| dir.join(name).is_file())
    }
}

/// Canonical system-type tags. String constants so the wire shape stays
/// stable when new variants land — adding a tag is additive, not a serde
/// break.
pub mod tag {
    pub const UNRAID: &str = "unraid";
    pub const PROXMOX_VE: &str = "proxmox-ve";
    pub const PROXMOX_BACKUP_SERVER: &str = "proxmox-backup-server";
    pub const MACOS: &str = "macos";
    pub const TRUENAS_SCALE: &str = "truenas-scale";
    pub const TRUENAS_CORE: &str = "truenas-core";
    pub const NIXOS: &str = "nixos";
    pub const DEBIAN: &str = "debian";
    pub const ALPINE: &str = "alpine";
    /// Catch-all when we know it's linux but can't pin down a distro.
    pub const LINUX: &str = "linux";
}

/// Canonical capability tags. Used in both the expected table and the
/// detected list — single vocabulary keeps the UI's diff trivial.
pub mod cap {
    pub const DOCKER: &str = "docker";
    pub const VM_HOST: &str = "vm-host";
    pub const LXC_HOST: &str = "lxc-host";
    pub const SMB_HOST: &str = "smb-host";
    pub const NFS_HOST: &str = "nfs-host";
    pub const ARRAY_STORAGE: &str = "array-storage";
    pub const BACKUP_TARGET: &str = "backup-target";
    pub const LAUNCHD_SERVICES: &str = "launchd-services";
    pub const CLUSTER_MEMBER: &str = "cluster-member";
}

/// Detect the canonical system type. First match wins (priority order matches
/// the spec memo). Never returns `None` — at minimum we fall through to
/// `"linux"` or `"macos"` based on `os_name`.
pub fn detect<F: HostFs>(fs: &F) -> String {
    // Unraid — distinctive ident file + OS-name string. Check before the
    // generic linux distros since unraid IS a linux derivative.
    if fs.exists(Path::new("/etc/unraid-version")) || fs.os_name().as_deref() == Some("Unraid OS") {
        return tag::UNRAID.to_string();
    }

    // PBS — proxy config file is the canonical marker. Check before
    // proxmox-ve so a PBS host running alongside pve tools doesn't
    // misidentify.
    if fs.exists(Path::new("/etc/proxmox-backup/proxy.cfg")) {
        return tag::PROXMOX_BACKUP_SERVER.to_string();
    }

    // Proxmox VE — pmxcfs cert OR pveversion binary. Check before debian
    // because PVE is debian-based.
    if fs.exists(Path::new("/etc/pve/local/pve-ssl.pem"))
        || fs.exists(Path::new("/etc/pve"))
        || fs.which("pveversion")
    {
        return tag::PROXMOX_VE.to_string();
    }

    // macOS — sysinfo reports `"Darwin"`.
    if fs.os_name().as_deref() == Some("Darwin") {
        return tag::MACOS.to_string();
    }

    // TrueNAS — long version starts with `TrueNAS-SCALE-` or `TrueNAS-CORE-`.
    if let Some(long) = fs.os_long() {
        if long.starts_with("TrueNAS-SCALE") {
            return tag::TRUENAS_SCALE.to_string();
        }
        if long.starts_with("TrueNAS-CORE") {
            return tag::TRUENAS_CORE.to_string();
        }
    }

    // NixOS — distinctive marker file. Check before debian since nixos.os_name
    // can be `"Linux"` and the long version varies.
    if fs.exists(Path::new("/etc/NIXOS")) {
        return tag::NIXOS.to_string();
    }

    // Debian / Alpine via sysinfo OS-name. These come last so a
    // proxmox-on-debian host doesn't get demoted to plain debian.
    match fs.os_name().as_deref() {
        Some("Debian GNU/Linux") | Some("Debian") => return tag::DEBIAN.to_string(),
        Some("Alpine Linux") | Some("Alpine") => return tag::ALPINE.to_string(),
        _ => {}
    }

    tag::LINUX.to_string()
}

/// Expected capability profile for a system type. Used by the UI to compute
/// anomaly badges: anything in `expected \ detected` is rendered as a
/// `missing: <cap>` badge, anything in `detected \ expected` as
/// `unexpected: <cap>`. Returns an empty slice for unknown tags so callers
/// can blindly diff without a special case.
pub fn expected_capabilities(system_type: &str) -> &'static [&'static str] {
    match system_type {
        tag::UNRAID => &[cap::DOCKER, cap::VM_HOST, cap::SMB_HOST, cap::ARRAY_STORAGE],
        tag::PROXMOX_VE => &[cap::VM_HOST, cap::LXC_HOST, cap::CLUSTER_MEMBER],
        tag::PROXMOX_BACKUP_SERVER => &[cap::BACKUP_TARGET],
        tag::MACOS => &[cap::LAUNCHD_SERVICES],
        // Generic linux distros have no required capabilities — anything
        // detected is just "this box happens to run X" rather than "should
        // run X by definition of its role".
        tag::DEBIAN
        | tag::ALPINE
        | tag::NIXOS
        | tag::TRUENAS_SCALE
        | tag::TRUENAS_CORE
        | tag::LINUX => &[],
        _ => &[],
    }
}

/// Probe the host for capabilities the daemon can observe at runtime.
/// Pure side-effect-free queries against `HostFs`; never panics, never shells
/// out (callers compose with async GPU detection separately).
pub fn detect_capabilities<F: HostFs>(fs: &F) -> Vec<String> {
    let mut caps = Vec::new();

    if fs.which("docker") {
        caps.push(cap::DOCKER.to_string());
    }
    // Proxmox VE → vm-host + lxc-host. Detect via the same markers as the
    // type detector so the two stay aligned.
    if fs.exists(Path::new("/etc/pve")) || fs.which("pveversion") {
        caps.push(cap::VM_HOST.to_string());
        caps.push(cap::LXC_HOST.to_string());
    } else {
        // Non-PVE vm-host signal: libvirt or qemu present.
        if fs.which("virsh") || fs.which("qemu-system-x86_64") {
            caps.push(cap::VM_HOST.to_string());
        }
        if fs.which("lxc") {
            caps.push(cap::LXC_HOST.to_string());
        }
    }
    if fs.which("smbd") {
        caps.push(cap::SMB_HOST.to_string());
    }
    if fs.which("nfsd") || fs.exists(Path::new("/etc/exports")) {
        caps.push(cap::NFS_HOST.to_string());
    }
    if fs.exists(Path::new("/etc/proxmox-backup/proxy.cfg")) {
        caps.push(cap::BACKUP_TARGET.to_string());
    }
    // Unraid → array-storage + smb-host (unraid runs samba by default).
    if fs.exists(Path::new("/etc/unraid-version")) {
        if !caps.iter().any(|c| c == cap::ARRAY_STORAGE) {
            caps.push(cap::ARRAY_STORAGE.to_string());
        }
        if !caps.iter().any(|c| c == cap::SMB_HOST) {
            caps.push(cap::SMB_HOST.to_string());
        }
    }
    // macOS — launchd is always there.
    if fs.os_name().as_deref() == Some("Darwin") {
        caps.push(cap::LAUNCHD_SERVICES.to_string());
    }

    caps
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::PathBuf;

    #[derive(Default)]
    struct FakeHostFs {
        files: HashSet<PathBuf>,
        os_name: Option<String>,
        os_long: Option<String>,
        on_path: HashSet<String>,
    }

    impl FakeHostFs {
        fn with_file(mut self, p: &str) -> Self {
            self.files.insert(PathBuf::from(p));
            self
        }
        fn with_os_name(mut self, n: &str) -> Self {
            self.os_name = Some(n.to_string());
            self
        }
        fn with_os_long(mut self, n: &str) -> Self {
            self.os_long = Some(n.to_string());
            self
        }
        fn with_bin(mut self, b: &str) -> Self {
            self.on_path.insert(b.to_string());
            self
        }
    }

    impl HostFs for FakeHostFs {
        fn exists(&self, path: &Path) -> bool {
            self.files.contains(path)
        }
        fn os_name(&self) -> Option<String> {
            self.os_name.clone()
        }
        fn os_long(&self) -> Option<String> {
            self.os_long.clone()
        }
        fn which(&self, name: &str) -> bool {
            self.on_path.contains(name)
        }
    }

    // ── detect() ─────────────────────────────────────────────────────────────

    #[test]
    fn detect_unraid_by_ident_file() {
        let fs = FakeHostFs::default().with_file("/etc/unraid-version");
        assert_eq!(detect(&fs), tag::UNRAID);
    }

    #[test]
    fn detect_unraid_by_os_name() {
        let fs = FakeHostFs::default().with_os_name("Unraid OS");
        assert_eq!(detect(&fs), tag::UNRAID);
    }

    #[test]
    fn detect_pbs_by_proxy_cfg() {
        let fs = FakeHostFs::default().with_file("/etc/proxmox-backup/proxy.cfg");
        assert_eq!(detect(&fs), tag::PROXMOX_BACKUP_SERVER);
    }

    #[test]
    fn detect_pve_by_ssl_pem() {
        let fs = FakeHostFs::default().with_file("/etc/pve/local/pve-ssl.pem");
        assert_eq!(detect(&fs), tag::PROXMOX_VE);
    }

    #[test]
    fn detect_pve_by_etc_pve_dir() {
        let fs = FakeHostFs::default().with_file("/etc/pve");
        assert_eq!(detect(&fs), tag::PROXMOX_VE);
    }

    #[test]
    fn detect_pve_by_pveversion_binary() {
        let fs = FakeHostFs::default().with_bin("pveversion");
        assert_eq!(detect(&fs), tag::PROXMOX_VE);
    }

    #[test]
    fn detect_pve_wins_over_debian_os_name() {
        // PVE is debian-based; OS-name says Debian. Type must NOT demote.
        let fs = FakeHostFs::default()
            .with_file("/etc/pve")
            .with_os_name("Debian GNU/Linux");
        assert_eq!(detect(&fs), tag::PROXMOX_VE);
    }

    #[test]
    fn detect_macos() {
        let fs = FakeHostFs::default().with_os_name("Darwin");
        assert_eq!(detect(&fs), tag::MACOS);
    }

    #[test]
    fn detect_truenas_scale() {
        let fs = FakeHostFs::default().with_os_long("TrueNAS-SCALE-24.10.0");
        assert_eq!(detect(&fs), tag::TRUENAS_SCALE);
    }

    #[test]
    fn detect_truenas_core() {
        let fs = FakeHostFs::default().with_os_long("TrueNAS-CORE-13.0");
        assert_eq!(detect(&fs), tag::TRUENAS_CORE);
    }

    #[test]
    fn detect_nixos() {
        let fs = FakeHostFs::default().with_file("/etc/NIXOS");
        assert_eq!(detect(&fs), tag::NIXOS);
    }

    #[test]
    fn detect_debian() {
        let fs = FakeHostFs::default().with_os_name("Debian GNU/Linux");
        assert_eq!(detect(&fs), tag::DEBIAN);
    }

    #[test]
    fn detect_debian_short_name() {
        let fs = FakeHostFs::default().with_os_name("Debian");
        assert_eq!(detect(&fs), tag::DEBIAN);
    }

    #[test]
    fn detect_alpine() {
        let fs = FakeHostFs::default().with_os_name("Alpine Linux");
        assert_eq!(detect(&fs), tag::ALPINE);
    }

    #[test]
    fn detect_alpine_short_name() {
        let fs = FakeHostFs::default().with_os_name("Alpine");
        assert_eq!(detect(&fs), tag::ALPINE);
    }

    #[test]
    fn detect_falls_through_to_linux() {
        // No markers, no recognized os_name → catch-all.
        let fs = FakeHostFs::default().with_os_name("Some Weird Distro");
        assert_eq!(detect(&fs), tag::LINUX);
    }

    #[test]
    fn detect_unknown_with_no_os_name_is_linux() {
        let fs = FakeHostFs::default();
        assert_eq!(detect(&fs), tag::LINUX);
    }

    #[test]
    fn detect_pbs_wins_over_pve_when_both_markers_present() {
        // A PBS host shouldn't be misidentified as PVE just because /etc/pve
        // happens to exist as a side-effect of pve-common packages.
        let fs = FakeHostFs::default()
            .with_file("/etc/proxmox-backup/proxy.cfg")
            .with_file("/etc/pve");
        assert_eq!(detect(&fs), tag::PROXMOX_BACKUP_SERVER);
    }

    // ── expected_capabilities() ──────────────────────────────────────────────

    #[test]
    fn expected_caps_unraid_includes_docker_and_smb() {
        let caps = expected_capabilities(tag::UNRAID);
        assert!(caps.contains(&cap::DOCKER));
        assert!(caps.contains(&cap::SMB_HOST));
        assert!(caps.contains(&cap::ARRAY_STORAGE));
    }

    #[test]
    fn expected_caps_pve_is_vm_lxc_cluster() {
        let caps = expected_capabilities(tag::PROXMOX_VE);
        assert!(caps.contains(&cap::VM_HOST));
        assert!(caps.contains(&cap::LXC_HOST));
        assert!(caps.contains(&cap::CLUSTER_MEMBER));
        assert!(!caps.contains(&cap::DOCKER), "PVE expected has no docker");
    }

    #[test]
    fn expected_caps_pbs_is_backup_target_only() {
        let caps = expected_capabilities(tag::PROXMOX_BACKUP_SERVER);
        assert_eq!(caps, &[cap::BACKUP_TARGET]);
    }

    #[test]
    fn expected_caps_macos_is_launchd() {
        let caps = expected_capabilities(tag::MACOS);
        assert_eq!(caps, &[cap::LAUNCHD_SERVICES]);
    }

    #[test]
    fn expected_caps_generic_distros_are_empty() {
        for t in [tag::DEBIAN, tag::ALPINE, tag::NIXOS, tag::LINUX] {
            assert!(
                expected_capabilities(t).is_empty(),
                "{t} should have no expected capabilities"
            );
        }
    }

    #[test]
    fn expected_caps_truenas_are_empty() {
        assert!(expected_capabilities(tag::TRUENAS_SCALE).is_empty());
        assert!(expected_capabilities(tag::TRUENAS_CORE).is_empty());
    }

    #[test]
    fn expected_caps_unknown_type_is_empty() {
        assert!(expected_capabilities("alien-os").is_empty());
    }

    // ── detect_capabilities() ────────────────────────────────────────────────

    #[test]
    fn detect_caps_docker_when_binary_on_path() {
        let fs = FakeHostFs::default().with_bin("docker");
        assert!(detect_capabilities(&fs).contains(&cap::DOCKER.to_string()));
    }

    #[test]
    fn detect_caps_pve_implies_vm_and_lxc_host() {
        let fs = FakeHostFs::default().with_file("/etc/pve");
        let caps = detect_capabilities(&fs);
        assert!(caps.contains(&cap::VM_HOST.to_string()));
        assert!(caps.contains(&cap::LXC_HOST.to_string()));
    }

    #[test]
    fn detect_caps_libvirt_without_pve_gives_vm_host() {
        let fs = FakeHostFs::default().with_bin("virsh");
        let caps = detect_capabilities(&fs);
        assert!(caps.contains(&cap::VM_HOST.to_string()));
        assert!(
            !caps.contains(&cap::LXC_HOST.to_string()),
            "libvirt alone shouldn't imply lxc-host"
        );
    }

    #[test]
    fn detect_caps_qemu_gives_vm_host() {
        let fs = FakeHostFs::default().with_bin("qemu-system-x86_64");
        assert!(detect_capabilities(&fs).contains(&cap::VM_HOST.to_string()));
    }

    #[test]
    fn detect_caps_lxc_alone_gives_lxc_host() {
        let fs = FakeHostFs::default().with_bin("lxc");
        assert!(detect_capabilities(&fs).contains(&cap::LXC_HOST.to_string()));
    }

    #[test]
    fn detect_caps_smbd_gives_smb_host() {
        let fs = FakeHostFs::default().with_bin("smbd");
        assert!(detect_capabilities(&fs).contains(&cap::SMB_HOST.to_string()));
    }

    #[test]
    fn detect_caps_nfsd_gives_nfs_host() {
        let fs = FakeHostFs::default().with_bin("nfsd");
        assert!(detect_capabilities(&fs).contains(&cap::NFS_HOST.to_string()));
    }

    #[test]
    fn detect_caps_exports_file_gives_nfs_host() {
        let fs = FakeHostFs::default().with_file("/etc/exports");
        assert!(detect_capabilities(&fs).contains(&cap::NFS_HOST.to_string()));
    }

    #[test]
    fn detect_caps_pbs_proxy_gives_backup_target() {
        let fs = FakeHostFs::default().with_file("/etc/proxmox-backup/proxy.cfg");
        assert!(detect_capabilities(&fs).contains(&cap::BACKUP_TARGET.to_string()));
    }

    #[test]
    fn detect_caps_unraid_gives_array_and_smb() {
        let fs = FakeHostFs::default().with_file("/etc/unraid-version");
        let caps = detect_capabilities(&fs);
        assert!(caps.contains(&cap::ARRAY_STORAGE.to_string()));
        assert!(caps.contains(&cap::SMB_HOST.to_string()));
    }

    #[test]
    fn detect_caps_unraid_does_not_duplicate_smb_when_smbd_also_present() {
        let fs = FakeHostFs::default()
            .with_file("/etc/unraid-version")
            .with_bin("smbd");
        let caps = detect_capabilities(&fs);
        let smb_count = caps.iter().filter(|c| **c == cap::SMB_HOST).count();
        assert_eq!(smb_count, 1, "smb-host must appear exactly once");
    }

    #[test]
    fn detect_caps_darwin_gives_launchd() {
        let fs = FakeHostFs::default().with_os_name("Darwin");
        assert!(detect_capabilities(&fs).contains(&cap::LAUNCHD_SERVICES.to_string()));
    }

    #[test]
    fn detect_caps_bare_linux_is_empty() {
        let fs = FakeHostFs::default().with_os_name("Linux");
        assert!(detect_capabilities(&fs).is_empty());
    }

    // ── RealHostFs sanity — runs against the actual machine. Must not depend
    // on a specific distro or capability set; just verifies the impl wires up.
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn real_host_fs_exists_for_etc() {
        // Every supported OS has /etc (linux + macos). We don't assert what's
        // *missing* because that varies.
        let real = RealHostFs;
        // `/etc` exists on linux + macos; tests don't run on windows.
        assert!(real.exists(Path::new("/etc")));
    }

    #[test]
    fn real_host_fs_which_finds_sh() {
        // `sh` is on PATH on every POSIX. (Tests only run on linux + macos.)
        let real = RealHostFs;
        assert!(real.which("sh"));
    }

    #[test]
    fn real_host_fs_which_misses_made_up_binary() {
        let real = RealHostFs;
        assert!(!real.which("orca-this-binary-does-not-exist-9d24f"));
    }

    #[test]
    fn real_host_fs_os_name_present() {
        // sysinfo returns Some on linux + macos.
        let real = RealHostFs;
        assert!(real.os_name().is_some());
    }

    #[test]
    fn real_host_fs_os_long_is_some_or_none() {
        // Field is allowed to be None; calling shouldn't panic.
        let real = RealHostFs;
        let _ = real.os_long();
    }
}
