# nary

Nary stands for "Nary's A Rusty Yarn"

A fast, secure npm-like package manager written in Rust.

## Supply Chain Security

nary includes multiple layers of protection against supply chain attacks:

- SHA-512 integrity verification
- Tarball hardening (rejects path traversal, symlinks, hardlinks, device nodes, FIFOs)
- 7-day package maturity period (à la [pnpm](https://pnpm.io/package_json#paborpmminimumreleaseage))
- [Sandboxed scripts & binaries](https://igorstechnoclub.com/sandbox-exec/) (macOS)
- Script execution prompts

## Features

- npm registry support with scoped packages and authentication
- Workspace support (npm workspaces format)
- Lockfile support (package-lock.json v3)
- Git dependencies (branches, tags, and commit hashes)
- Integrity verification (SHA-512)
- Live dependency tree visualization during install

## Install

> cargo install --path nary_bin

## Commands

### Package Management

| Command | Alias | Description |
|---------|-------|-------------|
| `install` | `i` | Install dependencies from package.json |
| `add` | | Add a package to dependencies |
| `remove` | `uninstall`, `rm` | Remove a package |
| `ci` | | Clean install from lockfile (CI/CD) |
| `prune` | | Remove extraneous packages |

### Scripts

| Command | Alias | Description |
|---------|-------|-------------|
| `run` | | Run a script from package.json |
| `test` | `t` | Run the test script |
| `start` | | Run the start script |
| `stop` | | Run the stop script |
| `restart` | | Run stop then start |

### Inspection

| Command | Alias | Description |
|---------|-------|-------------|
| `list` | `ls` | List installed packages |
| `outdated` | | Show outdated packages |
| `find-dupes` | | Find duplicate packages |

### Maintenance

| Command | Description |
|---------|-------------|
| `update` | Update packages within semver range |
| `dedupe` | Reduce duplication by hoisting |
| `audit` | Check for vulnerabilities |

### Development

| Command | Alias | Description |
|---------|-------|-------------|
| `link` | | Symlink a package for local development |
| `unlink` | | Remove a linked package |
| `exec` | `x` | Run a package binary (like npx) |
| `version` | | Bump version and create git tag |

### Common Options

- `-v, --verbose` - Verbose output (repeatable: -vv, -vvv)
- `--json` - JSON output (list, outdated, audit, find-dupes)
- `--dry-run` - Preview changes (prune, dedupe, update)

## Usage

### Install dependencies

```
cd your-project
nary install
```

During install, nary displays a live tree of in-flight packages:

```
[00:00:02] ████████████████░░░░░░░░░░░░░░░░░░░░░░░░      42/103  Installing...
  ⠋ koa@2.15.3
    ├─⠋ accepts@1.3.8
    ├─⠋ content-disposition@0.5.4
    └─⠋ cookies@0.9.1
```

### Add a package

```
nary add lodash
nary add -D typescript    # dev dependency
nary add express@^4.0.0   # specific version range
```

### Run scripts

```
nary run build
nary test                 # shortcut for 'nary run test'
```

### Execute a package binary

```
nary exec cowsay "Hello"
nary x typescript --version
```

### Check for updates

```
nary outdated
nary update              # update within semver range
nary update --latest     # update to latest versions
```

## Supply Chain Security

nary includes multiple layers of protection against supply chain attacks.

### Package Maturity Period

By default, nary won't install packages published within the last 7 days. This gives the community time to detect and report malicious packages before they reach your project.

When a version is too new, nary automatically falls back to the most recent mature version:

```
warn: lodash@4.18.0 skipped (published 2h ago, requires 7d maturity)
     -> Using lodash@4.17.21
```

If all versions are too new, nary errors with guidance:

```
error: No mature version found for new-package ^1.0.0
       Newest version 1.0.0 was published 1 hour ago

       To install anyway: nary install --allow-new-packages
       Or exclude in .npmrc: nary-maturity-exclude[]=new-package
```

**Configuration** (in `.npmrc`):

```ini
# Minimum age in minutes (default: 10080 = 7 days)
nary-minimum-release-age=10080

# Exclude specific packages from maturity checks
nary-maturity-exclude[]=lodash
nary-maturity-exclude[]=@types    # matches @types/* by prefix
```

**CLI override**:

```
nary install --allow-new-packages
nary add new-package --allow-new-packages
```

### Script Sandboxing

On macOS, lifecycle scripts (install, postinstall, etc.) run inside a sandbox that restricts:

- **Network access** - Scripts cannot make outbound connections
- **File system** - Limited to the project directory and npm cache
- **Sensitive paths** - No access to `~/.ssh`, `~/.aws`, `~/.gnupg`, keychains, or browser data

Before running any lifecycle scripts, nary prompts for confirmation:

```
The following packages have lifecycle scripts:
  • esbuild (postinstall): node install.js
  • sharp (install): node install/libvips.js

Run these scripts? [y/N]
```

Use `--ignore-scripts` to skip all lifecycle scripts.

### Exec Sandboxing

When running package binaries with `nary exec` or `nary x`, the same sandbox restrictions apply. This protects against malicious binaries that might attempt to exfiltrate data.

```
nary exec cowsay "Hello"    # runs sandboxed
nary x esbuild src/app.ts   # runs sandboxed
```

### Integrity Verification

All packages are verified against their SHA-512 integrity hash from the registry. If a tarball has been tampered with, nary will refuse to install it.

## License

Licensed under either of

 * Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
 * MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.
