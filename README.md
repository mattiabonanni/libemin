# Libemin

Small native desktop helper for booking hours into Libemax without embedding the website.

## What it does

- logs in against Libemax JSON endpoints
- supports the mail authentication step when Libemax requires it
- fetches the authenticated pages and discovers the real insert/search endpoints from HTML
- searches employees, clients, and activities through the same lookup APIs used by the site
- submits hour bookings as form data to the same backend actions used by the web UI

## Run

```bash
cargo run
```

## Releases

- GitHub Actions builds release artifacts when you push a tag matching the Cargo version, for example `v0.1.0`
- the workflow publishes:
  - macOS: `.dmg` plus a `.tar.gz` archive of `Libemin.app`
  - Windows: `.exe` (NSIS) and `.msi` (WiX)
- local packaging uses `cargo-packager`:

```bash
cargo install cargo-packager --locked
cargo packager --release --formats app,dmg
```

```bash
cargo packager --release --formats nsis,wix
```

- the current workflow produces unsigned artifacts; macOS notarization and Windows code-signing can be added later with repository secrets

### Optional signing secrets

- macOS signing/notarization:
  - `APPLE_CERTIFICATE`
  - `APPLE_CERTIFICATE_PASSWORD`
  - `APPLE_SIGNING_IDENTITY`
  - optionally `APPLE_KEYCHAIN_PROFILE`
  - notarization with either:
    - `APPLE_ID`, `APPLE_PASSWORD`, `APPLE_TEAM_ID`
    - or `APPLE_API_KEY`, `APPLE_API_ISSUER`
- if you use `APPLE_API_KEY`, store the contents of the `.p8` key in the secret and the workflow will write a temporary `APPLE_API_KEY_PATH` file for `cargo-packager`
- Windows signing:
  - `WINDOWS_CERTIFICATE_THUMBPRINT`
  - `WINDOWS_TIMESTAMP_URL`
  - optionally `WINDOWS_SIGN_COMMAND` if you do not want to use the default `signtool.exe`

- leave these secrets unset if you want unsigned releases

## Notes

- the app does not save your password
- set the Libemax tenant domain in the `Base URL` / `Dominio Libemax` field, for example `https://azienda.libemax.com`
- if the insert form URL is not discovered automatically, paste it into the `Insert form URL` field and click `Refresh discovery`
- this first version was built without a real account, so the discovery logic is intentionally runtime-driven and transparent
