use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use clap::{Parser, Subcommand};

const TARGET_NAME: &str = "x86_64-unknown-none";
const PART_START: u64 = 2048;
const PART_SECTORS: u64 = 128991;

const CORE_DAEMONS: &[&str] = &[
    "auth",
	"hesper",
	"vreg",
	"vlog"
];

const BUNDLE_APPS: &[&str] = &[
    "details",
	"dt",
	"dusk",
	"ns",
	"nyx",
	"select",
	"stream",
	"sys",
	"table",
	"terminal",
];

static ASM_FILES: &[(&str, &str)] = &[
    ("lib/vk-hal/src/x86_64/fpu/fpu.asm", "fpu.o"),
    ("lib/vk-hal/src/x86_64/cpu/gdt.asm", "gdt.o"),
    ("lib/vk-hal/src/x86_64/interrupts/idt.asm", "idt.o"),
    ("lib/vk-hal/src/x86_64/task/switch.asm", "switch.o"),
    ("lib/vk-hal/src/x86_64/task/syscall.asm", "syscall.o"),
];

#[derive(Parser)]
#[command(name = "xtask", about = "vespertine build & run toolchain")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// build os components
    Build {
        /// builds only kernel
        #[arg(long)]
        kernel: bool,

        /// builds only userland
        #[arg(long)]
        userland: bool,

        /// builds ports
        #[arg(long)]
        ports: bool,

        /// builds specific package
        #[arg(short, long)]
        package: Option<String>,
    },
    /// generate C ABI headers 
    Headers,
    /// builds ports
    Ports,
    /// rebuild ext2 partition image and update target/disk.img
    UpdateDisk,
    /// build bootable ISO image
    Iso,
    /// build everything
    Prep, 
    Run {
        /// launch QEMU with gdb stub and interrupt logging
        #[arg(long)]
        debug: bool,

        /// num cores for qemu
        #[arg(long, default_value_t = 2)]
        smp: u32,

        /// mem for qemu
        #[arg(long, default_value = "2G")]
        mem: String,

        /// disable kvm acceleration
        #[arg(long)]
        no_kvm: bool,
    },
    /// clean build artifacts
    Clean,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("[ERROR] xtask failed: {}", e);
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let cli = Cli::parse();
    let root = repo_root();

    let res = match cli.command {
        Commands::Build { kernel, userland, ports, package } => {
            let build_all = !kernel && !userland && !ports && package.is_none();

            if build_all || kernel {
                build_kernel_asm(&root);
                build_kernel(&root);
            }

            if ports {
                build_headers(&root)?;
                build_ports(&root)?;
            }

            if build_all || userland {
                build_userland(&root, None);
            } else if let Some(pkg) = package {
                build_userland(&root, Some(&pkg));
            }
            Ok(())
        }
        Commands::Headers => build_headers(&root),
        Commands::Ports => {
            build_headers(&root)?;
            build_ports(&root)
        }
        Commands::UpdateDisk => {
            build_userland(&root, None);
            stage_disk(&root)?;
            update_disk_image(&root)
        }
        Commands::Iso => {
            build_kernel_asm(&root);
            build_kernel(&root);
            build_iso(&root)
        }
        Commands::Prep => {
            build_kernel_asm(&root);
            build_kernel(&root);
            build_userland(&root, None);
            stage_disk(&root)?;
            update_disk_image(&root)?;
            build_iso(&root)?;
            Ok(())
        }
        Commands::Run { debug, smp, mem, no_kvm } => {
            run_qemu(&root, debug, smp, &mem, no_kvm)
        }
        Commands::Clean => clean(&root),
    };
    Ok(())
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("failed to locate repo root")
        .to_path_buf()
}

fn run_cmd(mut cmd: Command, desc: &str) -> Result<(), String> {
    println!("[INFO] {}", desc);
    let status = cmd.status().map_err(|e| format!("failed to execute {}: {}", desc, e))?;
    if !status.success() {
        return Err(format!("{} exited with status {}", desc, status));
    }
    Ok(())
}

fn build_headers(root: &Path) -> Result<(), String> {
    let mut cmd = Command::new("make");
    cmd.current_dir(root).args(["-C", "lib"]);
    run_cmd(cmd, "generating ABI headers")
}

fn build_ports(root: &Path) -> Result<(), String> {
    let mut cmd = Command::new("make");
    cmd.current_dir(root).args(["-C", "ports"]);
    run_cmd(cmd, "building ports")
}

fn build_kernel_asm(root: &Path) {
    let obj_dir = root.join("target/build");
    fs::create_dir_all(&obj_dir).unwrap();

    for (src, out) in ASM_FILES {
        let mut cmd = Command::new("nasm");
        cmd.args(["-f", "elf64"])
            .arg(root.join(src))
            .arg("-o")
            .arg(obj_dir.join(out));
        run_cmd(cmd, &format!("assembling {}", src)).unwrap();
    }
}

fn build_kernel(root: &Path) {
    let mut cmd = Command::new("cargo");
    cmd.current_dir(root).args([
        "build", "-p", "kernel", "--release", "--target", TARGET_NAME,
    ]);
    run_cmd(cmd, "building kernel").unwrap();
}

fn build_userland(root: &Path, specific_package: Option<&str>) {
    let mut cmd = Command::new("cargo");
    cmd.current_dir(root)
        .env(
            "RUSTFLAGS",
            format!(
                "-C relocation-model=static -C link-arg=-T{}",
                root.join("scripts/userland.ld").display()
            ),
        )
        .args(["build", "--release", "--target", TARGET_NAME]);

    if let Some(pkg) = specific_package {
        cmd.arg("-p").arg(pkg);
    } else {
        for pkg in CORE_DAEMONS.iter().chain(BUNDLE_APPS.iter()) {
            cmd.arg("-p").arg(pkg);
        }
    }
    run_cmd(cmd, "building userland package(s)").unwrap();

    let release_dir = root.join("target").join(TARGET_NAME).join("release");
    let assets_disk = root.join("assets/disk");

    // Core daemons -> /System/Core/<name>
    let sys_core = assets_disk.join("System/Core");
    fs::create_dir_all(&sys_core).unwrap();
    for &daemon in CORE_DAEMONS {
        if specific_package.is_none() || specific_package == Some(daemon) {
            let src = release_dir.join(daemon);
            let dst = sys_core.join(daemon);
            fs::copy(&src, &dst).unwrap();
        }
    }

    // App bundles -> /Programs/<app>.app/
    let programs_dir = assets_disk.join("Programs");
    for &app in BUNDLE_APPS {
        if specific_package.is_none() || specific_package == Some(app) {
            let bundle_dir = programs_dir.join(format!("{}.app", app));
            let bin_dir = bundle_dir.join("bin");
            fs::create_dir_all(&bin_dir).unwrap();

            let src_bin = release_dir.join(app);
            let dst_bin = bin_dir.join(app);
            fs::copy(&src_bin, &dst_bin).unwrap();

            let src_manifest = root.join("userland").join(app).join("manifest.toml");
            if src_manifest.exists() {
                let dst_manifest = bundle_dir.join("manifest.toml");
                fs::copy(&src_manifest, &dst_manifest).unwrap();
            }
        }
    }
}

fn stage_disk(root: &Path) -> Result<(), String> {
    let stage = root.join("target/build_deps/disk");
    let assets = root.join("assets/disk");

    let _ = fs::remove_dir_all(&stage);
    fs::create_dir_all(&stage).unwrap();

    let mut cp = Command::new("cp");
    cp.arg("-a").arg(format!("{}/.", assets.display())).arg(&stage);
    run_cmd(cp, "mirroring assets/disk to staging tree")?;

    fs::create_dir_all(stage.join("Devices")).unwrap();
    fs::create_dir_all(stage.join("System/Services")).unwrap();
    fs::create_dir_all(stage.join("System/Logs")).unwrap();
    Ok(())
}

fn update_disk_image(root: &Path) -> Result<(), String> {
    let disk_img = root.join("target/disk.img");
    let build_dir = root.join("target/build");
    fs::create_dir_all(&build_dir).unwrap();
    let partition_img = build_dir.join("partition.img");
    let stage = root.join("target/build_deps/disk");

    if !disk_img.exists() {
        let mut dd = Command::new("dd");
        dd.args(["if=/dev/zero", "bs=1M", "count=64"])
            .arg(format!("of={}", disk_img.display()));
        run_cmd(dd, "creating target/disk.img")?;

        let end_sector = PART_START + PART_SECTORS - 1;
        let mut sg = Command::new("sgdisk");
        sg.arg("-n")
            .arg(format!("1:{}:{}", PART_START, end_sector))
            .args(["-t", "1:8300"])
            .arg(&disk_img);
        run_cmd(sg, "partitioning target/disk.img")?;
    }

    let mut dd_zero = Command::new("dd");
    dd_zero
        .args(["if=/dev/zero", "bs=512"])
        .arg(format!("count={}", PART_SECTORS))
        .arg(format!("of={}", partition_img.display()));
    run_cmd(dd_zero, "Zeroing partition image")?;

    let mke2fs_cmd = format!(
        "chown -R 0:0 \"{}\" && mke2fs -q -F -t ext2 -d \"{}\" \"{}\"",
        stage.display(),
        stage.display(),
        partition_img.display()
    );
    let mut fakeroot = Command::new("fakeroot");
    fakeroot.args(["sh", "-c", &mke2fs_cmd]);
    run_cmd(fakeroot, "building ext2 partition with fakeroot mke2fs")?;

    let mut dd_splice = Command::new("dd");
    dd_splice
        .arg(format!("if={}", partition_img.display()))
        .arg(format!("of={}", disk_img.display()))
        .args([
            "bs=512",
            &format!("seek={}", PART_START),
            &format!("count={}", PART_SECTORS),
            "conv=notrunc",
        ]);
    run_cmd(dd_splice, "splicing partition into target/disk.img")?;

    let _ = fs::remove_file(partition_img);
    println!("[SUCCESS] target/disk.img updated.");
    Ok(())
}

fn build_iso(root: &Path) -> Result<(), String> {
    ensure_limine(root)?;

    let iso_root = root.join("iso_root");
    let _ = fs::remove_dir_all(&iso_root);
    fs::create_dir_all(iso_root.join("boot/limine")).unwrap();
    fs::create_dir_all(iso_root.join("EFI/BOOT")).unwrap();

    let kernel_elf = root.join("target").join(TARGET_NAME).join("release/kernel");
    fs::copy(&kernel_elf, iso_root.join("boot/kernel")).unwrap();
    fs::copy(
        root.join("assets/limine.conf"),
        iso_root.join("boot/limine/limine.conf"),
    )
    .unwrap();

    let limine_dir = root.join("target/build_deps/limine");
    fs::copy(
        limine_dir.join("limine-uefi-cd.bin"),
        iso_root.join("boot/limine/limine-uefi-cd.bin"),
    )
    .unwrap();
    fs::copy(
        limine_dir.join("BOOTX64.EFI"),
        iso_root.join("EFI/BOOT/BOOTX64.EFI"),
    )
    .unwrap();

    let iso_path = root.join("target/build/kernel-x86_64.iso");
    let mut xorriso = Command::new("xorriso");
    xorriso
        .args([
            "-report_about",
            "FAILURE",
            "-as",
            "mkisofs",
            "--efi-boot",
            "boot/limine/limine-uefi-cd.bin",
            "-efi-boot-part",
            "--efi-boot-image",
            "--protective-msdos-label",
        ])
        .arg(&iso_root)
        .arg("-o")
        .arg(&iso_path);
    run_cmd(xorriso, "generating bootable ISO with xorriso")?;

    let _ = fs::remove_dir_all(&iso_root);
    Ok(())
}

fn ensure_limine(root: &Path) -> Result<(), String> {
    let limine_dir = root.join("target/build_deps/limine");
    let uefi_cd = limine_dir.join("limine-uefi-cd.bin");
    let bootx64 = limine_dir.join("BOOTX64.EFI");

    if uefi_cd.exists() && bootx64.exists() {
        return Ok(());
    }

    fs::create_dir_all(&limine_dir).unwrap();
    let sh_cmd = format!(
        "curl -sL https://github.com/limine-bootloader/limine/releases/latest/download/limine-binary.tar.gz | tar -xz --strip-components=1 -C \"{}\"",
        limine_dir.display()
    );
    let mut sh = Command::new("sh");
    sh.args(["-c", &sh_cmd]);
    run_cmd(sh, "Downloading Limine UEFI binaries")
}

fn ensure_ovmf(root: &Path) -> Result<(), String> {
    let ovmf_file = root.join("target/build_deps/edk2-ovmf/ovmf-code-x86_64.fd");
    if ovmf_file.exists() {
        return Ok(());
    }

    let deps_dir = root.join("target/build_deps");
    fs::create_dir_all(&deps_dir).unwrap();
    let sh_cmd = format!(
        "curl -L https://github.com/osdev0/edk2-ovmf-nightly/releases/latest/download/edk2-ovmf.tar.gz | tar -xzf - -C \"{}\"",
        deps_dir.display()
    );
    let mut sh = Command::new("sh");
    sh.args(["-c", &sh_cmd]);
    run_cmd(sh, "Extracting OVMF x86_64 firmware")
}

fn run_qemu(root: &Path, debug: bool, smp: u32, mem: &str, no_kvm: bool) -> Result<(), String> {
    ensure_ovmf(root)?;

    let mut qemu = Command::new("qemu-system-x86_64");
    qemu.args([
        "-M",
        "q35",
        "-drive",
        "if=pflash,unit=0,format=raw,file=target/build_deps/edk2-ovmf/ovmf-code-x86_64.fd,readonly=on",
        "-cdrom",
        "target/build/kernel-x86_64.iso",
        "-drive",
        "file=target/disk.img,format=raw,id=disk0,if=none",
        "-device",
        "virtio-blk-pci,drive=disk0,disable-legacy=on",
        "-cpu",
        "host,migratable=no,+invtsc",
        "-smp",
        &smp.to_string(),
        "-m",
        mem,
        "-serial",
        "stdio",
    ]);

    if !no_kvm {
        qemu.arg("-accel").arg("kvm");
    }

    if debug {
        qemu.args([
            "-no-reboot",
            "-no-shutdown",
            "-d",
            "int",
            "-D",
            "qemu_idt.log",
            "-s",
            "-S",
        ]);
    }

    println!("[INFO] Launching QEMU...");
    let mut child = qemu.spawn().map_err(|e| format!("Failed to spawn QEMU: {}", e))?;
    let _ = child.wait();
    Ok(())
}

fn clean(root: &Path) -> Result<(), String> {
    let mut cargo = Command::new("cargo");
    cargo.current_dir(root).arg("clean");
    let _ = cargo.status();
    let _ = fs::remove_dir_all(root.join("iso_root"));
    let _ = fs::remove_dir_all(root.join("target/build"));
    println!("[SUCCESS] Clean complete.");
    Ok(())
}
