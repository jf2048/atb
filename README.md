# atb

[![Latest Version]][crates.io]

[Latest Version]: https://img.shields.io/crates/v/atb.svg
[crates.io]: https://crates.io/crates/atb

A simple lock-free SPSC [triple
buffering](https://en.wikipedia.org/wiki/Triple_buffering) implementation for
Rust.

This data structure is optimized for situations where a producer thread needs
to rebuild a larger data set periodically and send it to a real-time consumer
thread. The third buffer is always available to the producer thread, so it
never needs to wait to start producing a new frame.

### Example

```rust
const W: usize = 320;
const H: usize = 240;

let pixels = Arc::new(AtomicTripleBuffer::new([0u32; W * H]));

{
    let pixels = pixels.clone();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs_f64(1.0 / 60.0));
            let front = pixels.front_buffer().unwrap();
            // ... display `front` on the screen ...
        }
    });
}

let mut counter = 0u8;
loop {
    let mut bufs = pixels.back_buffers().unwrap();
    let back = bufs.back_mut();
    for y in 0..H {
        let c = counter.wrapping_add(y as u8) as u32;
        let c = c | (c << 8) | (c << 16) | (c << 24);
        for x in 0..W {
            back[y * W + x] = c;
        }
    }
    counter = counter.wrapping_add(1);
    bufs.swap();
    std::thread::sleep(Duration::from_secs_f64(1.0 / 24.0));
}
```
