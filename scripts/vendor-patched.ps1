# Rebuilds vendor/<crate> for each patched crate from crates.io and applies
# patches/<crate>.patch. Run once after cloning, and again after updating Slint
# or rustypipe.
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$lock = Get-Content (Join-Path $root 'Cargo.lock') -Raw
$manifest = Get-Content (Join-Path $root 'Cargo.toml') -Raw

$crates = @(
    @{
        # Slint's fallback fonts read from their memory-mapped files instead of
        # copied. The renderer is pinned to the same version as i-slint-core,
        # which always comes from crates.io, so read the version from there.
        Name    = 'i-slint-renderer-skia'
        Version = {
            if ($lock -notmatch 'name = "i-slint-core"\r?\nversion = "([^"]+)"') {
                throw 'i-slint-core is not in Cargo.lock'
            }
            $Matches[1]
        }
        Hint    = 'Slint changed the renderer. Check whether it still copies font data, and update or drop the patch.'
    },
    @{
        # Playlists the signed-in user owns, and no panic when YouTube refuses
        # a visitor ID. Pinned with "=" in Cargo.toml, so read it from there.
        Name    = 'rustypipe'
        Version = {
            if ($manifest -notmatch '(?m)^rustypipe = \{ version = "=([^"]+)"') {
                throw 'rustypipe is not pinned with "=" in Cargo.toml'
            }
            $Matches[1]
        }
        Hint    = 'Check whether rustypipe now reads editable playlist headers and returns visitor data errors, and update or drop the patch.'
    }
)

foreach ($c in $crates) {
    $name = $c.Name
    $version = & $c.Version
    $vendor = Join-Path $root "vendor\$name"
    $patch = Join-Path $root "patches\$name.patch"

    $staging = Join-Path ([IO.Path]::GetTempPath()) "$name-$version-$PID"
    New-Item -ItemType Directory -Force $staging | Out-Null
    try {
        $crate = Join-Path $staging 'crate.tar.gz'
        Write-Host "Fetching $name $version"
        Invoke-WebRequest "https://static.crates.io/crates/$name/$name-$version.crate" -OutFile $crate
        tar -xzf $crate -C $staging
        if ($LASTEXITCODE -ne 0) { throw "could not unpack $name" }
        $source = Join-Path $staging "$name-$version"
        foreach ($file in '.cargo_vcs_info.json', '.cargo-ok', 'Cargo.toml.orig', 'Cargo.lock') {
            Remove-Item (Join-Path $source $file) -Force -ErrorAction SilentlyContinue
        }

        git -C $source apply --check $patch
        if ($LASTEXITCODE -ne 0) {
            throw "patches\$name.patch no longer applies to $name $version. $($c.Hint)"
        }
        git -C $source apply $patch

        if (Test-Path $vendor) { Remove-Item $vendor -Recurse -Force }
        Move-Item $source $vendor
        Write-Host "Vendored $name $version with its patch"
    }
    finally {
        Remove-Item $staging -Recurse -Force -ErrorAction SilentlyContinue
    }
}
