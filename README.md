# kctl

do not use in production.

A Rust CLI for building upstream Linux kernels alongside an existing Debian or Ubuntu (for now) installation with GRUB 2 and initramfs-tools. Run it as your regular user with sudo access. It uses the Rust standard library and the distribution's system tools.

## First run on the target machine

Build with Rust 1.89 or newer.

```sh
cargo install --path . --locked
kctl help
kctl current
kctl config init
kctl deps
kctl keys status
kctl keys refresh
kctl kernels
kctl precheck
```

Use bare `kctl` at a terminal for the interactive prompt, or run each command directly. The prompt accepts whitespace-separated arguments. Use the direct CLI for paths containing spaces.

## Install and test a kernel

```sh
kctl backup create
kctl install latest-longterm
kctl boot list
```

Choose `stable`, `stable-eol`, `latest-longterm` or an explicit upstream version. The EOL channel requires a currently listed EOL stable release. You can inspect release candidates with `kctl kernels` and `kctl check mainline`. Installation verifies a detached archive signature by default. To install a release candidate whose archive has no signature, pass `--skip-signature-check` to that invocation, for example `kctl install mainline --skip-signature-check`. The option also works with `--hardware`. [Upstream release catalog](https://www.kernel.org/releases.json)

By default, kctl verifies the decoded archive with GnuPG before extraction. The full primary fingerprints for Linus Torvalds, Greg Kroah-Hartman, Sasha Levin, and Ben Hutchings are visible in `src/keys.rs` and in `kctl keys status`. Compare them with kernel.org's published fingerprints. [Kernel.org signing keys and fingerprints](https://www.kernel.org/signature.html#_important_fingerprints)

`kctl keys refresh` fetches public keys directly from the kernel.org Web Key Directory and checks every fingerprint before replacing `~/.kctl/kernel-release-keyring.gpg`. It merges the cached keys to retain any known revocations. Run it before the first verified kernel install and again to pick up updated key material. A new primary fingerprint requires a reviewed kctl update.

The build copies the running configuration, clears distro-specific certificate paths, applies upstream defaults and builds versioned image and header packages as your user. The build log lives at `~/.kctl/build.log`. Prechecks cover the current boot files, expected mounts, EFI Secure Boot state, build tools and disk capacity. Debug-enabled distribution configs can consume much more than the 12 GiB minimum build-space check.

Before package installation kctl pins the running kernel's GRUB entry using `/etc/default/grub.d/zzzz-kctl.cfg`. It checks the resulting default and clears pending one-shot selection. It retains the built packages until the image, headers, boot files and GRUB entry pass verification. If installation fails, `kctl recover repair RELEASE` can retry with the retained packages. Use `kctl recover discard-install RELEASE` to delete only those packages. Use `kctl clean-work` to preview and remove interrupted build trees.

Use the exact release printed by `kctl boot list` in place of RELEASE below.

```sh
kctl boot once RELEASE
# Reboot manually when ready. After the test boot:
kctl current
kctl boot default RELEASE
```

Default and one-shot selection use version-specific GRUB IDs. The persistent default also sets GRUB_TOP_LEVEL to promote that image on GRUB versions that support it. One-shot selection requires the running kernel as the persistent default and supports GRUB environments on ext2, ext3 or ext4 backed by a plain disk. Select the entry manually in GRUB on other layouts. On a remote machine, verify access to the boot console before the first test boot. A hung kernel can require a console reset. [GRUB configuration manual](https://www.gnu.org/software/grub/manual/grub/html_node/Simple-configuration.html)

## Maintenance

- `kctl check [CHANNEL]` compares the upstream version with the running kernel's upstream version. `kctl update [CHANNEL]` installs a newer release. Distro package revisions and backported patches are outside this comparison.
- `kctl clean` previews old kernels. `kctl clean RELEASE` confirms the matching distro image package before removal, verifies a safe default and keeps another intact fallback. kctl can reclaim `/boot/.kctl-grub-reserve` during low-space cleanup. If ordinary free space is exhausted on an ext filesystem, cleanup tests whether root can allocate enough temporary space to pin the running kernel safely.
- `kctl modules list` shows loaded modules. `modules available`, `modules activate NAME` and `modules setup NAME` list installed modules, load a module immediately and configure loading at boot, respectively.
- `kctl hardware SOURCE` checks detected PCI and USB vendor/device pairs against common driver table macros in an extracted source tree. Use `kctl install stable --hardware` to scan before compilation. A match is a source hint. Class drivers, firmware, external modules and kernel config can affect actual support.
- `kctl backup create [RELEASE]` saves the image, initramfs, config and System.map under `~/.kctl/backups/RELEASE` with SHA-256 checksums. These boot-file backups require the matching module tree to remain installed. Keep a separate disk backup for partition work.
- `kctl recover` explains how to boot a previous kernel and lists backups and pending recovery work. From the working kernel, use `kctl recover repair RELEASE` to retry interrupted package installation or configuration and regenerate its initramfs. Use `kctl backup restore RELEASE` to restore another release's files or `kctl recover restore RELEASE` to roll back an interrupted restore.
- `kctl boot resize grow DEVICE` and `kctl boot resize shrink DEVICE` open GParted on the parent disk of an unmounted partition. Use a graphical live environment with GParted installed, identify the boot partition with `lsblk -f`, then review and apply the new size in the editor. Growing requires suitable adjacent free space. [GParted manual](https://gparted.org/display-doc.php?name=help-manual)

## Configuration

`kctl config init` creates `~/.kctl/config.toml`. Defaults apply when the file is absent. Supported settings are flat TOML assignments.

```toml
channel = "latest-longterm"
min_boot_mib = 512
hardware_check = false
jobs = "auto"
```

`jobs = "auto"` uses the CPU parallelism available to kctl, capped at 64 jobs. Omitting `jobs` uses the same default. Set an integer from 1 to 64 for a fixed limit. `kctl precheck` prints the selected job count. The shell and `current` warn below the configured boot-space threshold. Installation also estimates boot payload space from the built image and the current initramfs.

