use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

struct Asset {
    served_path: String,
    source_suffix: String,
}

fn main() {
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("Cargo must set CARGO_MANIFEST_DIR"),
    );
    let mut assets = Vec::new();
    for (directory, served_prefix, source_prefix) in [
        ("web/dist", "dist", "/web/dist"),
        (
            "web/client-packages",
            "client-packages",
            "/web/client-packages",
        ),
    ] {
        let source = manifest_dir.join(directory);
        if !source.is_dir() {
            panic!(
                "missing built web assets at {}; build the web client before building Tessivum",
                source.display()
            );
        }
        println!("cargo:rerun-if-changed={directory}");
        collect_assets(
            &source,
            Path::new(served_prefix),
            source_prefix,
            &mut assets,
        )
        .unwrap_or_else(|error| {
            panic!(
                "cannot package built web assets from {}: {error}",
                source.display()
            )
        });
    }
    assets.sort_by(|left, right| left.served_path.cmp(&right.served_path));
    if assets.is_empty() {
        panic!("built web asset directories are empty");
    }

    let mut generated = String::from("pub(crate) static ASSETS: &[super::EmbeddedAsset] = &[\n");
    for asset in assets {
        generated.push_str(&format!(
            "    super::EmbeddedAsset {{ path: {:?}, bytes: include_bytes!(concat!(env!(\"CARGO_MANIFEST_DIR\"), {:?})) }},\n",
            asset.served_path, asset.source_suffix
        ));
    }
    generated.push_str("];\n");
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").expect("Cargo must set OUT_DIR"))
            .join("embedded_web_assets.rs"),
        generated,
    )
    .expect("write generated embedded web asset registry");
}

fn collect_assets(
    source: &Path,
    served_prefix: &Path,
    source_prefix: &str,
    assets: &mut Vec<Asset>,
) -> io::Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("web asset must not be a symlink: {}", path.display()),
            ));
        }
        if metadata.is_dir() {
            let segment = entry.file_name();
            let segment = segment.to_str().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("web asset path is not Unicode: {}", path.display()),
                )
            })?;
            let next_source_prefix = format!("{source_prefix}/{segment}");
            collect_assets(
                &path,
                &served_prefix.join(segment),
                &next_source_prefix,
                assets,
            )?;
            continue;
        }
        if !metadata.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("web asset path is not Unicode: {}", path.display()),
            )
        })?;
        let served_path = served_prefix
            .join(name)
            .to_string_lossy()
            .replace('\\', "/");
        let source_suffix = format!("{source_prefix}/{name}");
        println!("cargo:rerun-if-changed={}", path.display());
        assets.push(Asset {
            served_path,
            source_suffix,
        });
    }
    Ok(())
}
