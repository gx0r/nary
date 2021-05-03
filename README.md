# nary
Nary stands for "Nary ain't Rusty Yarn"

Toy npm-like installer to see what writing one in Rust could be like.

Some ideas are:
 * Focus on security and error handling at all possible failure points
 * Creating the traditional nested directory hierarchy (not particularly interested in flattening)
 * Not focued on performance yet
 * Dependency resolution not fully implemented yet

## Example

```
git clone https://github.com/ggcode1/nary.git
cd nary/nary_bin
cargo install --path . --force
cd ../examples/boiler
nary

[00:00:00] █░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░       1/25      
[00:00:00] ███░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░       2/25      aphrodite@^1.2.3
[00:00:00] ████░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░       3/25      html-template-tag@^1.0.0
[00:00:00] ██████░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░       4/25      koa@^2.3.0
[00:00:00] ████████░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░       5/25      koa-bodyparser@^4.2.0
[00:00:00] █████████░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░       6/25      koa-compress@^2.0.0
[00:00:01] ███████████░░░░░░░░░░░░░░░░░░░░░░░░░░░░░       7/25      koa-conditional-get@^2.0.0
[00:00:01] ████████████░░░░░░░░░░░░░░░░░░░░░░░░░░░░       8/25      koa-ejs@^4.1.0
[00:00:01] ██████████████░░░░░░░░░░░░░░░░░░░░░░░░░░       9/25      koa-etag@^3.0.0
[00:00:01] ████████████████░░░░░░░░░░░░░░░░░░░░░░░░      10/25      koa-favicon@^2.0.0
[00:00:01] █████████████████░░░░░░░░░░░░░░░░░░░░░░░      11/25      koa-helmet@^3.2.0
[00:00:02] ███████████████████░░░░░░░░░░░░░░░░░░░░░      12/25      koa-morgan@^1.0.1
[00:00:02] ████████████████████░░░░░░░░░░░░░░░░░░░░      13/25      koa-mount@3.0.0
[00:00:02] ██████████████████████░░░░░░░░░░░░░░░░░░      14/25      koa-response-time@^2.0.0
[00:00:02] ████████████████████████░░░░░░░░░░░░░░░░      15/25      koa-router@^7.2.1
[00:00:02] █████████████████████████░░░░░░░░░░░░░░░      16/25      koa-session@^5.5.0
[00:00:02] ███████████████████████████░░░░░░░░░░░░░      17/25      koa-static@^4.0.1
[00:00:02] ████████████████████████████░░░░░░░░░░░░      18/25      koa-trie-router@^2.1.6
[00:00:03] ██████████████████████████████░░░░░░░░░░      19/25      marko@^4.4.26
[00:00:03] ████████████████████████████████░░░░░░░░      20/25      pem@^1.9.7
[00:00:03] █████████████████████████████████░░░░░░░      21/25      promise-delay@^2.1.0
[00:00:03] ███████████████████████████████████░░░░░      22/25      socket.io@^2.0.3
[00:00:04] ████████████████████████████████████░░░░      23/25      socketio-sticky-session@git://github.com/wzrdtales/socket-io-sticky-session.git#2d0367fd7c80c727923f33d2ac11e34d0267ae4c
[00:00:04] ██████████████████████████████████████░░      24/25      spdy@^3.4.7
```



## License

Licensed under either of

 * Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
 * MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

