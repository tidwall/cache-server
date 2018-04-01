# `cache-server`

A minimal Key/Value store written in Rust. 

This is my very first project in Rust. I wanted to throw myself into the deep end by using event-based networking, multithreading, and memory synchronization patterns.

## Features

- Multithreaded
- Compatible with existing Redis clients
- Optimized for command pipelining

## Build

You need to [install Rust](https://www.rust-lang.org/en-US/install.html).

```
cargo build --release
target/release/cache-server
```

The options `--threads` is available to define the number of threads to use.

## Commands:

```
SET key value
GET key
DEL key
KEYS pattern
FLUSHDB
QUIT
PING
```

## Contact

Josh Baker [@tidwall](http://twitter.com/tidwall)

## License

Source code is available under the MIT [License](/LICENSE).
