use std::env;
use std::fmt::Write;
use std::fs;
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(out_dir) = env::var_os("OUT_DIR") else {
        return Err("OUT_DIR not set".into());
    };
    let Ok(manifest_dir) = env::var("CARGO_MANIFEST_DIR") else {
        return Err("CARGO_MANIFEST_DIR not set".into());
    };
    let migrations_dir = Path::new(&manifest_dir).join("migrations");

    println!("cargo:rerun-if-changed=migrations");

    let mut entries = Vec::new();

    if migrations_dir.exists() {
        for entry in fs::read_dir(&migrations_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("sql") {
                let filename = entry.file_name().to_string_lossy().to_string();
                let Some(stem) = filename.strip_suffix(".sql") else {
                    continue;
                };
                let Some((ver_str, _rest)) = stem.split_once('_') else {
                    continue;
                };
                let Ok(version) = ver_str.parse::<i64>() else {
                    continue;
                };
                entries.push((version, stem.to_string(), filename));
            }
        }
    }

    entries.sort_by_key(|(ver, _, _)| *ver);

    let mut generated = String::new();
    generated.push_str("const MIGRATIONS: &[Migration] = &[\n");

    for (version, name, filename) in &entries {
        let _ = write!(
            &mut generated,
            "    Migration {{\n        version: {version},\n        name: \"{name}\",\n        sql: include_str!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/migrations/{filename}\")),\n    }},\n"
        );
    }

    generated.push_str("];\n");

    let dest_path = Path::new(&out_dir).join("migrations_generated.rs");
    fs::write(&dest_path, generated)?;
    Ok(())
}
