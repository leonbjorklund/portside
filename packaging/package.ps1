$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
Push-Location (Split-Path $PSScriptRoot -Parent)
try {
    $metadata = cargo metadata --no-deps --format-version 1 | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) { throw 'cargo metadata failed' }
    $version = ($metadata.packages | Where-Object name -eq 'portside').version
    $hostTriple = rustc -vV | Select-String '^host: (.+)$'
    if ($LASTEXITCODE -ne 0 -or $hostTriple.Matches[0].Groups[1].Value -ne 'x86_64-pc-windows-msvc') {
        throw 'Packaging requires the Windows x64 MSVC toolchain'
    }
    cargo build --release --locked
    if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }

    $toolsDirectory = Join-Path (Get-Location) 'target\package-tools'
    New-Item -ItemType Directory -Path $toolsDirectory -Force | Out-Null
    $sysroot = rustc --print sysroot
    if ($LASTEXITCODE -ne 0) { throw 'rustc sysroot lookup failed' }
    Copy-Item (Join-Path $sysroot 'share\doc\rust\COPYRIGHT-library.html') (Join-Path $toolsDirectory 'Rust-COPYRIGHT-library.html')
    $compilerDirectory = Join-Path $toolsDirectory 'inno-6.7.3'
    $compiler = Join-Path $compilerDirectory 'ISCC.exe'
    if (-not (Test-Path -LiteralPath $compiler)) {
        $download = Join-Path $toolsDirectory 'innosetup-6.7.3.exe'
        Invoke-WebRequest 'https://github.com/jrsoftware/issrc/releases/download/is-6_7_3/innosetup-6.7.3.exe' -OutFile $download
        if ((Get-FileHash $download -Algorithm SHA256).Hash -ne '9C73C3BAE7ED48D44112A0F48E66742C00090BDB5BEF71D9D3C056C66E97B732') {
            throw 'Inno Setup download checksum mismatch'
        }
        $arguments = '/CURRENTUSER /PORTABLE=1 /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /DIR="{0}"' -f $compilerDirectory
        $process = Start-Process -FilePath $download -ArgumentList $arguments -WindowStyle Hidden -Wait -PassThru
        if ($process.ExitCode -ne 0) { throw "Inno Setup extraction failed ($($process.ExitCode))" }
    }
    & $compiler /Qp "/DAppVersion=$version" packaging\portside.iss
    if ($LASTEXITCODE -ne 0) { throw 'Inno Setup compilation failed' }
    $installer = Join-Path (Get-Location) "target\installer\Portside-$version-x64-setup.exe"
    Get-FileHash $installer -Algorithm SHA256
} finally {
    Pop-Location
}
