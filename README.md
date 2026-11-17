# overgit

Keep personal changes on top of a Git project without putting them in its history.
Overgit gives those changes their own Git repository in the same working directory.
Add private tooling, override configuration, or hide unwanted files while keeping
normal base Git commits free of overlay changes.

```sh
overgit init
overgit add personal.sh config.toml
overgit commit -m "personal setup"
```

Build on Linux or macOS with system Git and [mise](https://mise.jdx.dev).
From this checkout, review `mise.toml`, then run:

```sh
mise trust && mise install
mise exec -- cargo install --path . --locked
```

Run `overgit --help` to get started.

## License

AGPL-3.0-or-later. See [LICENSE](LICENSE).
