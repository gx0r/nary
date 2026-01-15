# nary

Nary stands for "Nary's A Rusty Yarn"

A fast, secure npm-like package manager written in Rust.

## Features

- Full npm registry support with scoped packages and authentication
- Workspace support (npm workspaces format)
- Lockfile support (package-lock.json v3)
- Git dependencies (branches, tags, and commit hashes)
- Integrity verification (SHA-512)
- Live dependency tree visualization during install
- Lifecycle scripts with [sandboxing](https://igorstechnoclub.com/sandbox-exec/) on macOS
- Prompt to confirm scripts before running

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

## License

Licensed under either of

 * Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
 * MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.
