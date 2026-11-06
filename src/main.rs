use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

const IO_CHUNK_SIZE: usize = 4 * 1024 * 1024; // 4MB Buffer
const SYNC_INTERVAL: u64 = 32 * 1024 * 1024;  // Flush hardware cache every 32MB

#[derive(Parser)]
#[command(
name = "nextburn",
version,
about = "Production-Grade Linux & Windows 10/11 UEFI USB Creator"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// סריקת כונני USB נשלפים ובטוחים לכתיבה
    List,
    /// צריבת קובץ ISO (לינוקס או Windows) לכונן USB
    Burn {
        /// נתיב לקובץ ה-ISO
        #[arg(short, long)]
        iso: PathBuf,

        /// כונן יעד (למשל /dev/sdd)
        #[arg(short, long)]
        device: PathBuf,

        /// אילוץ מצב Windows UEFI (אם לא צוין, המערכת תזהה אוטומטית לפי תוכן ה-ISO)
        #[arg(short, long)]
        windows: bool,

        /// תווית הכונן (Volume Label)
        #[arg(short, long, default_value = "NEXTBURN")]
        label: String,

        /// עקיפת דרישות TPM 2.0 / SecureBoot / RAM בהתקנת Windows 11
        #[arg(long)]
        win11_bypass: bool,
    },
}

enum MountKind {
    IsoLoop,
    Vfat,
    Ntfs,
}

/// RAII Guard שמבטיח שחרור עיגון (Unmount) ומחיקת תיקייה זמנית גם במקרה של שגיאה
struct MountGuard {
    mount_point: PathBuf,
}

impl MountGuard {
    fn new(prefix: &str, source: &Path, kind: MountKind) -> Result<Self> {
        let pid = std::process::id();
        let mount_point = PathBuf::from(format!("/tmp/nextburn_{}_{}", prefix, pid));
        fs::create_dir_all(&mount_point)?;

        let success = match kind {
            MountKind::IsoLoop => Command::new("mount")
            .args(["-o", "ro,loop"])
            .arg(source)
            .arg(&mount_point)
            .status()?
            .success(),
            MountKind::Vfat => Command::new("mount")
            .args(["-t", "vfat"])
            .arg(source)
            .arg(&mount_point)
            .status()?
            .success(),
            MountKind::Ntfs => {
                // ניסיון ראשון: דרייבר הקרנל המודרני ntfs3 (Linux 5.15+ / 6.x)
                let _ = Command::new("modprobe").arg("ntfs3").status();
                let try_ntfs3 = Command::new("mount")
                .args(["-t", "ntfs3"])
                .arg(source)
                .arg(&mount_point)
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);

                if try_ntfs3 {
                    true
                } else {
                    // ניסיון שני: דרייבר FUSE ntfs-3g
                    Command::new("mount")
                    .args(["-t", "ntfs-3g"])
                    .arg(source)
                    .arg(&mount_point)
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false)
                }
            }
        };

        if !success {
            let _ = fs::remove_dir(&mount_point);
            bail!("Failed to mount {:?} at {:?}", source, mount_point);
        }

        Ok(Self { mount_point })
    }

    fn path(&self) -> &Path {
        &self.mount_point
    }
}

impl Drop for MountGuard {
    fn drop(&mut self) {
        let _ = Command::new("sync").status();
        let ok = Command::new("umount")
        .arg(&self.mount_point)
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

        if !ok {
            let _ = Command::new("umount")
            .args(["-l", &self.mount_point.to_string_lossy()])
            .stderr(Stdio::null())
            .status();
        }
        let _ = fs::remove_dir(&self.mount_point);
    }
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Commands::List => list_usb_drives()?,
        Commands::Burn {
            iso,
            device,
            windows,
            label,
            win11_bypass,
        } => {
            ensure_root()?;
            verify_safe_usb(&device)?;
            unmount_all_partitions(&device);

            let is_win = windows || detect_windows_iso(&iso).unwrap_or(false);

            if is_win {
                println!("🪟 Detected Mode: Windows 10/11 UEFI Dual-Partition (FAT32 Boot + NTFS Data)");
                burn_windows_iso(&iso, &device, &label, win11_bypass)?;
            } else {
                println!("🐧 Detected Mode: Raw Hybrid Image (O_SYNC Direct Write)");
                burn_raw_iso(&iso, &device)?;
            }
        }
    }
    Ok(())
}

// ============================================================================
// 1. בטיחות חומרה, הרשאות וזיהוי כוננים
// ============================================================================

fn ensure_root() -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        bail!("Root privileges required! Please run with sudo.");
    }
    Ok(())
}

fn list_usb_drives() -> Result<()> {
    println!("{:<12} {:<10} {:<15} {:<22}", "DEVICE", "SIZE", "VENDOR", "MODEL");
    println!("{}", "-".repeat(62));

    let mut found = false;
    for entry in fs::read_dir("/sys/block")? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("sd") || read_sys(&entry.path().join("removable")) != "1" {
            continue;
        }

        let sectors: u64 = read_sys(&entry.path().join("size")).parse().unwrap_or(0);
        if sectors == 0 {
            continue;
        }

        found = true;
        let size_gb = (sectors * 512) as f64 / 1_000_000_000.0;
        let vendor = read_sys(&entry.path().join("device/vendor"));
        let model = read_sys(&entry.path().join("device/model"));
        println!(
            "/dev/{:<7} {:<10} {:<15} {:<22}",
            name,
            format!("{:.1} GB", size_gb),
                vendor,
                 model
        );
    }

    if !found {
        println!("No removable USB drives detected.");
    }
    Ok(())
}

fn verify_safe_usb(dev: &Path) -> Result<()> {
    if !dev.exists() {
        bail!("Device {:?} does not exist!", dev);
    }
    let name = dev.file_name().context("Invalid device path")?.to_string_lossy();
    let rem_path = PathBuf::from(format!("/sys/block/{}/removable", name));

    if read_sys(&rem_path) != "1" {
        bail!("SAFETY BLOCK: {:?} is NOT marked as a removable USB drive!", dev);
    }

    let mounts = fs::read_to_string("/proc/mounts").unwrap_or_default();
    let prefix = dev.to_string_lossy();
    for line in mounts.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 && parts[0].starts_with(prefix.as_ref()) {
            if ["/", "/boot", "/boot/efi", "/home", "/var"].contains(&parts[1]) {
                bail!("SAFETY BLOCK: {:?} contains critical system mount ({})!", dev, parts[1]);
            }
        }
    }
    Ok(())
}

fn unmount_all_partitions(dev: &Path) {
    let prefix = dev.to_string_lossy();
    if let Ok(mounts) = fs::read_to_string("/proc/mounts") {
        for line in mounts.lines() {
            if let Some(part) = line.split_whitespace().next() {
                if part.starts_with(prefix.as_ref()) {
                    let _ = Command::new("umount")
                    .args(["-l", part])
                    .stderr(Stdio::null())
                    .status();
                }
            }
        }
    }
}

fn detect_windows_iso(iso_path: &Path) -> Result<bool> {
    let iso_mount = MountGuard::new("probe", iso_path, MountKind::IsoLoop)?;
    let has_wim = iso_mount.path().join("sources/install.wim").exists()
    || iso_mount.path().join("sources/install.esd").exists()
    || iso_mount.path().join("sources/boot.wim").exists();
    Ok(has_wim)
}

// ============================================================================
// 2. מנוע צריבת לינוקס (Raw Hybrid + O_SYNC)
// ============================================================================

fn burn_raw_iso(iso_path: &Path, dev_path: &Path) -> Result<()> {
    let mut iso = File::open(iso_path).context("Cannot open ISO file")?;
    let total = iso.metadata()?.len();

    let mut dev = OpenOptions::new()
    .write(true)
    .custom_flags(libc::O_SYNC)
    .open(dev_path)
    .context("Failed to open target USB device")?;

    let pb = make_progress_bar(total, "Writing Raw ISO stream (O_SYNC)...");
    let mut buf = vec![0u8; IO_CHUNK_SIZE];
    let mut written = 0u64;

    loop {
        let n = iso.read(&mut buf)?;
        if n == 0 {
            break;
        }
        dev.write_all(&buf[..n])?;
        written += n as u64;
        pb.set_position(written);
    }

    pb.set_message("Flushing hardware cache...");
    dev.sync_all()?;
    let _ = Command::new("partprobe").arg(dev_path).status();
    pb.finish_with_message("✅ Raw ISO Burn Complete! Safe to remove USB.");
    Ok(())
}

// ============================================================================
// 3. מנוע צריבת Windows 10/11 UEFI (GPT + FAT32 Boot + NTFS Install)
// ============================================================================

fn burn_windows_iso(
    iso_path: &Path,
    dev_path: &Path,
    label: &str,
    win11_bypass: bool,
) -> Result<()> {
    println!("-> [1/5] Mounting Windows ISO image...");
    let iso_mount = MountGuard::new("iso", iso_path, MountKind::IsoLoop)?;

    println!("-> [2/5] Creating GPT partition table on {:?}...", dev_path);
    let _ = Command::new("wipefs").args(["-a", &dev_path.to_string_lossy()]).status();

    let sfdisk_script = "label: gpt\n\
size=1536M, type=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7, name=\"UEFI_BOOT\"\n\
type=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7, name=\"WIN_INSTALL\"\n";

    let mut sfdisk = Command::new("sfdisk")
    .arg("--force")
    .arg(dev_path)
    .stdin(Stdio::piped())
    .stdout(Stdio::null())
    .spawn()
    .context("Failed to execute sfdisk")?;

    sfdisk
    .stdin
    .as_mut()
    .context("Failed to open sfdisk stdin")?
    .write_all(sfdisk_script.as_bytes())?;

    if !sfdisk.wait()?.success() {
        bail!("sfdisk failed to partition {:?}", dev_path);
    }

    sync_kernel_partitions(dev_path)?;

    let part1 = partition_path(dev_path, 1);
    let part2 = partition_path(dev_path, 2);
    wait_for_block_nodes(&[&part1, &part2])?;

    println!("-> [3/5] Formatting partitions (FAT32 Boot + NTFS Data)...");
    let clean_label: String = label.chars().take(11).collect();

    // ניקוי חתימות ישנות על המחיצות עצמן לפני הפירמוט
    let _ = Command::new("wipefs").args(["-a", &part1.to_string_lossy()]).status();
    let _ = Command::new("wipefs").args(["-a", &part2.to_string_lossy()]).status();

    run_cmd("mkfs.vfat", &["-F", "32", "-n", "UEFI_BOOT", &part1.to_string_lossy()])?;
    run_cmd("mkfs.ntfs", &["-Q", "-F", "-L", &clean_label, &part2.to_string_lossy()])?;

    // המתנה לסיום סריקת udev ושחרור אוטומטי אם מנהל הקבצים עיגן אותן
    let _ = Command::new("udevadm").args(["settle", "--timeout=5"]).status();
    unmount_all_partitions(dev_path);

    println!("-> [4/5] Copying Windows files with hardware-synced progress...");
    let fat_mount = MountGuard::new("fat32", &part1, MountKind::Vfat)?;
    let ntfs_mount = MountGuard::new("ntfs", &part2, MountKind::Ntfs)?;

    let boot_total = calc_boot_subset_size(iso_mount.path())?;
    let full_total = calc_dir_size(iso_mount.path())?;
    let grand_total = boot_total + full_total;

    let pb = make_progress_bar(grand_total, "Copying UEFI Boot files (FAT32)...");
    let mut copied_bytes = 0u64;

    copy_boot_subset(iso_mount.path(), fat_mount.path(), &pb, &mut copied_bytes)?;

    pb.set_message("Copying Windows Installation image (NTFS)...");
    copy_tree_recursive(iso_mount.path(), ntfs_mount.path(), &pb, &mut copied_bytes)?;

    if win11_bypass {
        pb.set_message("Injecting Windows 11 TPM/RAM Bypass (autounattend.xml)...");
        write_win11_bypass(fat_mount.path())?;
        write_win11_bypass(ntfs_mount.path())?;
    }

    pb.set_message("Flushing filesystems and unmounting safely...");
    let _ = Command::new("sync").status();
    pb.finish_with_message("✅ Windows 10/11 UEFI USB Ready! Safe to remove drive.");
    Ok(())
}

// ============================================================================
// 4. מנוע העתקה מבוקר (Chunked Sync Copy) ופונקציות עזר
// ============================================================================

fn copy_boot_subset(
    iso_root: &Path,
    fat_root: &Path,
    pb: &ProgressBar,
    copied: &mut u64,
) -> Result<()> {
    for entry in fs::read_dir(iso_root)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_lower = name.to_string_lossy().to_lowercase();
        let src_path = entry.path();
        let dst_path = fat_root.join(&name);

        if name_lower == "sources" && src_path.is_dir() {
            fs::create_dir_all(&dst_path)?;
            let boot_wim = src_path.join("boot.wim");
            if boot_wim.exists() {
                copy_file_synced(&boot_wim, &dst_path.join("boot.wim"), pb, copied)?;
            }
        } else if src_path.is_dir() {
            copy_tree_recursive(&src_path, &dst_path, pb, copied)?;
        } else {
            copy_file_synced(&src_path, &dst_path, pb, copied)?;
        }
    }
    Ok(())
}

fn calc_boot_subset_size(iso_root: &Path) -> Result<u64> {
    let mut total = 0u64;
    for entry in fs::read_dir(iso_root)? {
        let entry = entry?;
        let name_lower = entry.file_name().to_string_lossy().to_lowercase();
        let path = entry.path();

        if name_lower == "sources" && path.is_dir() {
            let boot_wim = path.join("boot.wim");
            if let Ok(meta) = boot_wim.metadata() {
                total += meta.len();
            }
        } else if path.is_dir() {
            total += calc_dir_size(&path)?;
        } else if let Ok(meta) = entry.metadata() {
            total += meta.len();
        }
    }
    Ok(total)
}

fn calc_dir_size(dir: &Path) -> Result<u64> {
    let mut total = 0u64;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            total += calc_dir_size(&path)?;
        } else {
            total += entry.metadata()?.len();
        }
    }
    Ok(total)
}

fn copy_tree_recursive(
    src: &Path,
    dst: &Path,
    pb: &ProgressBar,
    copied: &mut u64,
) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if src_path.is_dir() {
            copy_tree_recursive(&src_path, &dst_path, pb, copied)?;
        } else {
            copy_file_synced(&src_path, &dst_path, pb, copied)?;
        }
    }
    Ok(())
}

fn copy_file_synced(
    src: &Path,
    dst: &Path,
    pb: &ProgressBar,
    copied: &mut u64,
) -> Result<()> {
    let mut in_file = File::open(src)?;
    let file_len = in_file.metadata()?.len();
    let mut out_file = File::create(dst)?;
    let mut buf = vec![0u8; IO_CHUNK_SIZE];
    let mut since_last_sync = 0u64;

    loop {
        let n = in_file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out_file.write_all(&buf[..n])?;
        *copied += n as u64;
        since_last_sync += n as u64;
        pb.set_position(*copied);

        // מבצעים Flush תוך כדי ריצה רק בקבצים ענקיים (כמו install.wim / boot.wim)
        if since_last_sync >= SYNC_INTERVAL {
            out_file.sync_data()?;
            since_last_sync = 0;
        }
    }

    // לא מבצעים sync_data פרטני על אלפי קבצים קטנים כדי לא לחנוק את בקר ה-USB
    if file_len >= SYNC_INTERVAL {
        out_file.sync_data()?;
    }
    Ok(())
}

fn write_win11_bypass(root: &Path) -> Result<()> {
    let xml = r#"<?xml version="1.0" encoding="utf-8"?>
    <unattend xmlns="urn:schemas-microsoft-com:unattend">
    <settings pass="windowsPE">
    <component name="Microsoft-Windows-Setup" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS">
    <RunSynchronous>
    <RunSynchronousCommand wcm:action="add" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">
    <Order>1</Order>
    <Path>reg add HKLM\SYSTEM\Setup\LabConfig /v BypassTPMCheck /t REG_DWORD /d 1 /f</Path>
    </RunSynchronousCommand>
    <RunSynchronousCommand wcm:action="add" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">
    <Order>2</Order>
    <Path>reg add HKLM\SYSTEM\Setup\LabConfig /v BypassSecureBootCheck /t REG_DWORD /d 1 /f</Path>
    </RunSynchronousCommand>
    <RunSynchronousCommand wcm:action="add" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">
    <Order>3</Order>
    <Path>reg add HKLM\SYSTEM\Setup\LabConfig /v BypassRAMCheck /t REG_DWORD /d 1 /f</Path>
    </RunSynchronousCommand>
    </RunSynchronous>
    </component>
    </settings>
    </unattend>"#;
    fs::write(root.join("autounattend.xml"), xml)?;
    Ok(())
}

fn partition_path(dev: &Path, num: u8) -> PathBuf {
    let s = dev.to_string_lossy();
    if s.chars().last().map(|c| c.is_ascii_digit()).unwrap_or(false) {
        PathBuf::from(format!("{}p{}", s, num))
    } else {
        PathBuf::from(format!("{}{}", s, num))
    }
}

fn sync_kernel_partitions(dev: &Path) -> Result<()> {
    let _ = Command::new("blockdev").args(["--rereadpt", &dev.to_string_lossy()]).status();
    let _ = Command::new("partprobe").arg(dev).status();
    let _ = Command::new("udevadm").args(["settle", "--timeout=5"]).status();
    Ok(())
}

fn wait_for_block_nodes(nodes: &[&Path]) -> Result<()> {
    for _ in 0..30 {
        if nodes.iter().all(|n| n.exists()) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(200));
    }
    bail!("Timed out waiting for kernel partition nodes: {:?}", nodes)
}

fn run_cmd(bin: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(bin)
    .args(args)
    .stdout(Stdio::null())
    .status()
    .with_context(|| format!("Failed to launch {}", bin))?;
    if !status.success() {
        bail!("Command {} {:?} exited with error", bin, args);
    }
    Ok(())
}

fn read_sys(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default().trim().to_string()
}

fn make_progress_bar(total: u64, msg: &str) -> ProgressBar {
    let pb = ProgressBar::new(total);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} [{elapsed_precise}] [{bar:28.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, ETA: {eta}) {msg}",
        )
        .unwrap()
        .progress_chars("█▓░"),
    );
    pb.set_message(msg.to_string());
    pb
}
