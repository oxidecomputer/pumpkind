// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Copyright 2024 Oxide Computer Company

use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::Command;

use anyhow::anyhow;
use anyhow::Context;
use anyhow::Result;
use camino::Utf8Path;
use clap::Parser;
use clap::ValueEnum;
use omicron_zone_package::config::PackageName;
use omicron_zone_package::package::BuildConfig;

const PACKAGE_NAME: PackageName = PackageName::new_const("pumpkind");

/// Possible formats for a bundled pumpkind distro.
#[derive(Clone, Copy, PartialEq, Debug, ValueEnum)]
pub enum DistFormat {
    /// package to run under Omicron control in the switch zone
    Switch,
    /// package to run standalone in the global zone
    Global,
}

#[derive(Debug, Parser)]
#[command(name = "xtask", about = "pumpkind xtask support")]
enum Xtasks {
    Dist {
        #[arg(short, long, help = "package release bits ")]
        release: bool,

        #[arg(short, long, value_enum, help = "which package format to use")]
        format: DistFormat,
    },
}

fn collect_misc(dst: &str) -> Result<()> {
    let bin_dir = format!("{dst}/opt/oxide/bin");
    let svc_xml = format!("{dst}/opt/oxide/lib/svc/manifest/system");

    collect("./daemon/misc", &bin_dir, vec!["svc-pumpkind"])?;
    collect("./daemon/misc", &svc_xml, vec!["pumpkind.xml"])
}

fn project_root() -> Result<String> {
    match Path::new(&std::env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(1)
        .unwrap()
        .to_path_buf()
        .to_str()
    {
        Some(p) => Ok(p.to_string()),
        _ => Err(anyhow!("bad path")),
    }
}

fn collect<T: ToString>(src: &str, dst: &str, files: Vec<T>) -> Result<()> {
    let src_dir = Path::new(src);
    if !src_dir.is_dir() {
        return Err(anyhow!("source isn't a directory: {src}",));
    }

    let dst_dir = Path::new(&dst);
    if !dst_dir.is_dir() {
        fs::create_dir_all(dst_dir)?;
    }

    for f in files {
        let f = f.to_string();
        let src_file = src_dir.join(&f);
        let dst_file = dst_dir.join(&f);
        println!("-- Installing: {dst_file:?}");
        std::fs::copy(src_file, dst_file).with_context(|| {
            format!(
                "copying {f:?} from {src} to {dst}, \
                    was it built with the same --release / \
                    --debug flag passed to `cargo xtask`?"
            )
        })?;
    }
    Ok(())
}

fn collect_binaries(release: bool, dst: &str) -> Result<()> {
    let src = match release {
        true => "./target/release",
        false => "./target/debug",
    };

    let binaries = vec!["pumpkind".to_string(), "pumpkinadm".to_string()];

    collect(src, dst, binaries)
}

// Build a package suitable for omicron-package to bundle into a switch zone
async fn switch_package() -> Result<()> {
    let manifest_file = "pumpkind-manifest.toml";
    let manifest_path = format!("{}/misc/{}", project_root()?, manifest_file);
    let mut file = fs::File::open(manifest_path)
        .with_context(|| "attempting to open omicron manifest")?;
    let mut manifest = String::new();
    file.read_to_string(&mut manifest)
        .with_context(|| "reading manifest")?;
    let cfg = omicron_zone_package::config::parse_manifest(&manifest)?;

    let output_dir = Utf8Path::new("out");
    fs::create_dir_all(output_dir)?;
    let build_config = BuildConfig::default();

    for package in cfg.packages.values() {
        if let Err(e) = package
            .create(&PACKAGE_NAME, output_dir, &build_config)
            .await
        {
            eprintln!("omicron packaging failed: {e:?}");
            return Err(e);
        }
    }

    Ok(())
}

// Build a package suitable for omicron-package to install into the global zone
fn global_package() -> Result<()> {
    let output_dir = Path::new("out");
    fs::create_dir_all(output_dir)?;
    let tgt_path = "out/pumpkind-gz.tar".to_string();
    let mut tar_args = vec!["cf".to_string(), tgt_path.clone()];

    // cd into the proto area before collecting everything under opt/
    tar_args.push("-C".into());
    tar_args.push("target/proto/opt/oxide".into());
    tar_args.push("bin".into());
    tar_args.push("-C".into());
    tar_args.push("target/proto/opt/oxide".into());
    tar_args.push("lib".into());

    println!("building global zone dist in {tgt_path}");
    let status = Command::new("tar").args(&tar_args).status()?;
    match status.success() {
        true => Ok(()),
        false => Err(anyhow!("tarball construction failed")),
    }
}

pub async fn dist(release: bool, format: DistFormat) -> Result<()> {
    let proto_root = "target/proto";
    let opt_root = format!("{}/opt/oxide", &proto_root);
    let bin_root = format!("{opt_root}/bin");

    println!("cleaning up proto area");
    let _ = std::fs::remove_dir_all(proto_root);

    // populate the proto area
    collect_binaries(release, &bin_root)?;
    collect_misc(proto_root)?;
    match format {
        DistFormat::Switch => switch_package().await,
        DistFormat::Global => global_package(),
    }
}

#[tokio::main]
async fn main() {
    let task = Xtasks::parse();
    if let Err(e) = match task {
        Xtasks::Dist { release, format } => dist(release, format).await,
    } {
        eprintln!("failed: {e}");
        std::process::exit(-1);
    }
}
