fn read(path: &str) -> String {
    println!("cargo:rerun-if-changed={path}");
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .replace("\r\n", "\n")
}

/// The text between `prefix` and the next quote.
fn quoted_after(text: &str, prefix: &str) -> Option<String> {
    let rest = &text[text.find(prefix)? + prefix.len()..];
    Some(rest[..rest.find('"')?].to_owned())
}

fn vendored_version(dir: &str, name: &str) -> Option<String> {
    let manifest = read(&format!("{dir}/{name}/Cargo.toml"));
    quoted_after(&manifest, &format!("name = \"{name}\"\nversion = \""))
}

/// Cargo silently ignores a `[patch]` whose version no longer matches, which
/// would quietly drop the fixes in `patches/`, so fail the build instead.
/// CI vendors outside the repo and names the folder in `YTM_PATCHED_CRATES`.
fn check_vendored() {
    println!("cargo:rerun-if-env-changed=YTM_PATCHED_CRATES");
    let dir = std::env::var("YTM_PATCHED_CRATES").unwrap_or_else(|_| "vendor".into());
    let lock = read("Cargo.lock");
    let manifest = read("Cargo.toml");
    let slint = quoted_after(&lock, "name = \"i-slint-core\"\nversion = \"");
    let wanted = [
        ("i-slint-common", slint.clone()),
        ("i-slint-renderer-skia", slint),
        (
            "rustypipe",
            quoted_after(&manifest, "\nrustypipe = { version = \"="),
        ),
    ];
    for (name, version) in wanted {
        let vendored = vendored_version(&dir, name);
        if version != vendored {
            panic!(
                "{name} should be {} but {dir}/{name} is {}; run scripts/vendor-patched.ps1",
                version.as_deref().unwrap_or("unknown"),
                vendored.as_deref().unwrap_or("missing"),
            );
        }
    }
}

fn main() {
    check_vendored();

    let config = slint_build::CompilerConfiguration::new().with_style("fluent-dark".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("Slint UI compiles");

    let mut res = winresource::WindowsResource::new();
    res.set_icon("icons/icon.ico")
        .set("FileDescription", "YouTube Music")
        .set("ProductName", "YouTube Music");
    res.compile().expect("Windows resources compile");
}
