# Rebuilds vendor/<crate> from crates.io with patches/<crate>.patch applied.
# Run after cloning and after updating Slint or rustypipe.
# CI vendors into a folder outside the repo with -Destination.
param([string]$Destination)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
if (-not $Destination) { $Destination = Join-Path $root 'vendor' }
$lock = Get-Content (Join-Path $root 'Cargo.lock') -Raw
$manifest = Get-Content (Join-Path $root 'Cargo.toml') -Raw

$slintVersion = {
    if ($lock -notmatch 'name = "i-slint-core"\r?\nversion = "([^"]+)"') {
        throw 'i-slint-core is not in Cargo.lock'
    }
    $Matches[1]
}

$crates = @(
    @{
        # Pinned to the same version as i-slint-core.
        Name    = 'i-slint-common'
        Version = $slintVersion
        Hint    = 'Slint changed how it loads SLINT_FONT_PATH. Check whether it still reads the files into memory, and update or drop the patch.'
    },
    @{
        # Pinned to the same version as i-slint-core, which always comes from
        # crates.io.
        Name    = 'i-slint-renderer-skia'
        Version = $slintVersion
        Hint    = 'Slint changed the renderer. Check whether it still keeps its surface while minimised, and update or drop the patch.'
    },
    @{
        # Pinned with "=" in Cargo.toml.
        Name    = 'rustypipe'
        Version = {
            if ($manifest -notmatch '(?m)^rustypipe = \{ version = "=([^"]+)"') {
                throw 'rustypipe is not pinned with "=" in Cargo.toml'
            }
            $Matches[1]
        }
        Hint    = 'Check whether rustypipe now reads editable playlist headers, how it fetches visitor data, and whether it takes in the cookies YouTube sets, and update or drop the patch.'
    }
)

New-Item -ItemType Directory -Force $Destination | Out-Null
foreach ($c in $crates) {
    $name = $c.Name
    $version = & $c.Version
    $vendor = Join-Path $Destination $name
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
