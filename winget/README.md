# WinGet manifests

> Run every command below from the repository root. `winget` parses *every* file in the
> manifest folder, which is why the manifests live in `manifests/` and this README sits
> beside it rather than inside it.

Bootstrap manifests (in `manifests/`) used to add this package to the
[WinGet Community Repository](https://github.com/microsoft/winget-pkgs) for the first time.

The release workflow submits *updates* with `wingetcreate update`, which edits a manifest that
already exists in `winget-pkgs`. It cannot create one, so the first version has to be submitted
once by hand using the files here. After that PR is merged, every later release is submitted
automatically by the workflow and these files are no longer consulted — treat them as a record of
the initial submission rather than a live source of truth.

## Submitting the first version

Validate locally (this is the same check `winget-pkgs` runs first):

```powershell
winget validate --manifest .\winget\manifests
```

Optionally install from the manifest to confirm it works end to end, which is also what the
repository's automated validation does:

```powershell
winget install --manifest .\winget\manifests
```

Then submit. This opens a pull request against `microsoft/winget-pkgs` from your account, so it
needs a GitHub personal access token with the `public_repo` scope:

```powershell
winget install Microsoft.WingetCreate
wingetcreate submit --token <YOUR_GITHUB_PAT> .\winget\manifests
```

The pull request is checked automatically and then reviewed by a human moderator. Expect
questions if anything looks like it could be confused with another package — this one is an
unofficial fork, so the description states that plainly and the identifier is deliberately
distinct from the original `CodeZeno.ClaudeCodeUsageMonitor`.

## Later releases

Add a `WINGETCREATE_GITHUB_TOKEN` secret to this repository containing the same kind of token.
The `winget` job in `.github/workflows/release.yml` then submits each tagged release on its own;
without the secret that job logs a warning and skips, which is why it currently does nothing.

## Updating the hash

`InstallerSha256` must match the published asset exactly:

```powershell
(Get-FileHash .\claude-code-usage-monitor-pro.exe -Algorithm SHA256).Hash
```
