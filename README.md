# ukitouch-input
Native extension of [ukitouch](https://github.com/ukitouch/ukitouch): input, provides keyboard and mouse input simulation. Currently supported platforms: Windows, X11

## Build
Build release:
```bash
cargo build --release
```

Pack:
```bash
cargo xtask dist
```

will generate `ukitouch-input-<platform>.zip` in dist/, can be directly installed in ukitouch.