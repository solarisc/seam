<#
.SYNOPSIS
  Build the Windows setup program for the version in Cargo.toml and, with
  -Publish, attach it to a draft GitHub release.

.DESCRIPTION
  Run from an elevated PowerShell (the full test suite needs it) on the
  commit to release, with Inno Setup 6, the GitHub CLI (signed in) and the
  aarch64-apple-darwin Rust target installed. Checks everything first, builds
  target\installer\Seam-<version>-windows-setup.exe and SHA256SUMS.txt, then
  with -Publish tags the commit v<version>, pushes the tag and creates or
  updates the draft release with notes from CHANGELOG.md. Publishing the
  draft is left to a person; a published release is never changed.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\release.ps1 -Publish
#>
param([switch]$Publish)
$ErrorActionPreference = 'Stop'

function Native([string]$What, [scriptblock]$Command) {
    & $Command
    if ($LASTEXITCODE -ne 0) { throw "$What failed." }
}

# Output of a command whose failure is an answer, not an error. Windows
# PowerShell turns redirected stderr into errors under 'Stop'.
function Probe([scriptblock]$Command) {
    $ErrorActionPreference = 'Continue'
    $output = & $Command 2>$null
    [pscustomobject]@{ Output = $output; Succeeded = $LASTEXITCODE -eq 0 }
}

Push-Location (Join-Path $PSScriptRoot '..')
try {
    $principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'Run this from an elevated PowerShell window (Run as administrator).'
    }
    if (git status --porcelain) {
        throw 'Commit or stash your changes first: a release is built from a commit.'
    }
    $version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
    $tag = "v$version"

    # Release notes: the lines under "## <version>" up to the next heading.
    $changelog = @(Get-Content CHANGELOG.md -Encoding UTF8)
    $start = [array]::IndexOf($changelog, "## $version")
    if ($start -lt 0) { throw "CHANGELOG.md has no '## $version' section." }
    $end = $start + 1
    while ($end -lt $changelog.Count -and -not $changelog[$end].StartsWith('## ')) { $end++ }
    $notes = @($changelog | Select-Object -Skip ($start + 1) -First ($end - $start - 1))
    if (-not ($notes -join '').Trim()) { throw "The '## $version' section of CHANGELOG.md is empty." }

    $head = git rev-parse HEAD
    $tagged = (Probe { git rev-parse -q --verify "refs/tags/$tag^{commit}" }).Output
    if ($tagged -and $tagged -ne $head) {
        throw "$tag already points at another commit. Bump the version in Cargo.toml for a new release."
    }
    if ($Publish) {
        if (-not (Probe { gh auth status }).Succeeded) { throw 'Sign in to GitHub first: gh auth login' }
        $remote = (Probe { git ls-remote --tags origin "refs/tags/$tag^{}" }).Output
        if (-not $remote) { $remote = (Probe { git ls-remote --tags origin "refs/tags/$tag" }).Output }
        if ($remote -and ($remote -split '\s+')[0] -ne $head) {
            throw "$tag on GitHub points at another commit."
        }
        $draft = Probe { gh release view $tag --json isDraft --jq .isDraft }
        if ($draft.Succeeded -and $draft.Output -ne 'true') {
            throw "$tag is already published; release a new version instead."
        }
    }

    & (Join-Path $PSScriptRoot 'check.ps1') -Release
    & (Join-Path $PSScriptRoot 'install-windows.ps1') -Installer
    if (git status --porcelain) {
        throw 'The build changed tracked files (usually Cargo.lock after a version bump). Commit them and run this again.'
    }

    $name = "Seam-$version-windows-setup.exe"
    $setup = "target\installer\$name"
    $sums = 'target\installer\SHA256SUMS.txt'
    $hash = (Get-FileHash $setup -Algorithm SHA256).Hash.ToLowerInvariant()
    # The format `sha256sum -c` reads.
    "$hash *$name" | Set-Content -Encoding ascii $sums
    Write-Host "Built $setup"
    Write-Host "SHA-256 $hash"
    if (-not $Publish) { return }

    $notesFile = Join-Path (Resolve-Path 'target\installer') 'notes.md'
    [IO.File]::WriteAllLines($notesFile, [string[]]$notes, (New-Object Text.UTF8Encoding $false))
    if (-not $tagged) {
        Native 'Tagging' { git tag -a $tag -m "Seam $version" }
    }
    Native 'Pushing the tag' { git push origin $tag }
    if ((Probe { gh release view $tag }).Succeeded) {
        Native 'Uploading' { gh release upload $tag $setup $sums --clobber }
        Native 'Updating notes' { gh release edit $tag --notes-file $notesFile }
    } else {
        Native 'Creating the draft release' {
            gh release create $tag $setup $sums --draft --verify-tag --title "Seam $version" --notes-file $notesFile
        }
    }
    Write-Host "Draft release $tag is ready. Publish it on GitHub once it has been tried."
} finally {
    Pop-Location
}
