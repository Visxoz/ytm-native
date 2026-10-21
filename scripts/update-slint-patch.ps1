# Rebuilds vendor/i-slint-renderer-skia from the Slint version in Cargo.lock
# and applies patches/slint-renderer-skia-fonts.patch. Run after updating Slint.
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$name = 'i-slint-renderer-skia'
$vendor = Join-Path $root "vendor\$name"
$patch = Join-Path $root 'patches\slint-renderer-skia-fonts.patch'

# The renderer is pinned to the same version as i-slint-core, which always
# comes from crates.io, so read the version from there.
$lock = Get-Content (Join-Path $root 'Cargo.lock') -Raw
if ($lock -notmatch 'name = "i-slint-core"\r?\nversion = "([^"]+)"') {
    throw 'i-slint-core is not in Cargo.lock'
}
$version = $Matches[1]

$staging = Join-Path ([IO.Path]::GetTempPath()) "$name-$version-$PID"
New-Item -ItemType Directory -Force $staging | Out-Null
try {
    $crate = Join-Path $staging 'crate.tar.gz'
    Write-Host "Fetching $name $version"
    Invoke-WebRequest "https://static.crates.io/crates/$name/$name-$version.crate" -OutFile $crate
    tar -xzf $crate -C $staging
    if ($LASTEXITCODE -ne 0) { throw 'could not unpack the crate' }
    $source = Join-Path $staging "$name-$version"
    foreach ($file in '.cargo_vcs_info.json', '.cargo-ok', 'Cargo.toml.orig', 'Cargo.lock') {
        Remove-Item (Join-Path $source $file) -Force -ErrorAction SilentlyContinue
    }

    git -C $source apply --check $patch
    if ($LASTEXITCODE -ne 0) {
        throw "the patch no longer applies to $name $version; Slint changed font_cache.rs. Check whether it still copies font data, and update or drop the patch."
    }
    git -C $source apply $patch

    if (Test-Path $vendor) { Remove-Item $vendor -Recurse -Force }
    Move-Item $source $vendor
    Write-Host "Vendored $name $version with the font patch"
}
finally {
    Remove-Item $staging -Recurse -Force -ErrorAction SilentlyContinue
}
