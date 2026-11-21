use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn content_type_for(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "application/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "json" => "application/json; charset=utf-8",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn collect_dist_files(dir: &Path, root: &Path, out: &mut Vec<(String, PathBuf)>) {
    println!("cargo:rerun-if-changed={}", dir.display());
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|entry| entry.path()))
        .collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            collect_dist_files(&path, root, out);
        } else if path.is_file() {
            println!("cargo:rerun-if-changed={}", path.display());
            if let Ok(rel) = path.strip_prefix(root) {
                let url_path = format!(
                    "/{}",
                    rel.components()
                        .map(|c| c.as_os_str().to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                        .join("/")
                );
                let abs = env::current_dir()
                    .map(|cwd| cwd.join(&path))
                    .unwrap_or(path);
                out.push((url_path, abs));
            }
        }
    }
}

fn write_ui_assets(require_dist: bool) {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo"));
    let generated_path = out_dir.join("ui_assets.rs");
    let dist_root = Path::new("../her-ui/dist");
    let index_path = dist_root.join("index.html");
    println!("cargo:rerun-if-changed=../her-ui/dist");
    println!("cargo:rerun-if-changed=../her-ui/dist/index.html");

    let has_valid_dist = index_path
        .metadata()
        .map(|meta| meta.is_file() && meta.len() > 0)
        .unwrap_or(false);

    let mut source = String::from(
        "pub(crate) struct EmbeddedAsset {\n    pub path: &'static str,\n    pub content_type: &'static str,\n    pub bytes: &'static [u8],\n}\n\n",
    );

    if has_valid_dist {
        let mut files = Vec::new();
        collect_dist_files(dist_root, dist_root, &mut files);
        files.sort_by(|a, b| a.0.cmp(&b.0));

        source.push_str("#[cfg_attr(not(test), allow(dead_code))]\npub(crate) const UI_IS_STUB: bool = false;\n");
        source.push_str("pub(crate) const UI_ASSETS: &[EmbeddedAsset] = &[\n");
        for (url_path, abs_path) in files {
            let ctype = content_type_for(&abs_path);
            source.push_str(&format!(
                "    EmbeddedAsset {{ path: {:?}, content_type: {:?}, bytes: include_bytes!({:?}) }},\n",
                url_path,
                ctype,
                abs_path.to_string_lossy().as_ref()
            ));
        }
        source.push_str("];\n");
    } else if require_dist {
        panic!(
            "HER_REQUIRE_UI_DIST is set, but ../her-ui/dist/index.html is missing or empty; run `npm ci && npm run build` in her-ui first"
        );
    } else {
        source.push_str(
            "#[cfg_attr(not(test), allow(dead_code))]\npub(crate) const UI_IS_STUB: bool = true;\n",
        );
        source.push_str(
            "pub(crate) const UI_ASSETS: &[EmbeddedAsset] = &[\n    EmbeddedAsset {\n        path: \"/index.html\",\n        content_type: \"text/html; charset=utf-8\",\n        bytes: b\"<!doctype html><html lang=\\\"zh-CN\\\"><head><meta charset=\\\"utf-8\\\"><title>HER Console</title></head><body>HER Console UI placeholder</body></html>\",\n    },\n];\n",
        );
    }

    fs::write(generated_path, source).expect("write ui_assets.rs");
}

fn main() {
    for name in [
        "CARGO_FEATURE_RELEASE_TEST_KEYS",
        "PROFILE",
        "OPT_LEVEL",
        "HER_REQUIRE_UI_DIST",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    if std::env::var_os("CARGO_FEATURE_RELEASE_TEST_KEYS").is_some()
        && (std::env::var("PROFILE").as_deref() == Ok("release")
            || std::env::var("OPT_LEVEL").as_deref() != Ok("0"))
    {
        panic!("release-test-keys is restricted to unoptimized debug fixture builds; never ship test keys");
    }
    let require_dist = env::var("HER_REQUIRE_UI_DIST")
        .map(|val| !val.is_empty() && val != "0")
        .unwrap_or(false)
        || env::var("PROFILE").as_deref() == Ok("release");
    write_ui_assets(require_dist);
}
