# Cork's engine

`free/` is the engine of Cork's free version: the local server that keeps
the map of the code your agents are changing, their briefings, lint from
each other's changes, landing and memory, in one SQLite file on your
machine. With the hooks (`../hooks`) and the code parser (`../core`) linked
in, it builds the one program, `cork`, which runs this server as
`cork local`, on `127.0.0.1` only.

Cork's cloud (accounts, billing, teams across machines, embeddings) is not
in this repository; `src/free/` answers in its place.

## Build

```sh
cargo build --release --no-default-features --features cli
./target/release/cork install
```

`cargo test --release` runs the engine's tests.

## License

[FSL-1.1-MIT](LICENSE.md): use, change and share it for anything except
offering a competing product or service; each version becomes MIT two
years after its release.

[corkcli.com](https://corkcli.com)
