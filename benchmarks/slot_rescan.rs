mod after;
mod before;
use std::{hint::black_box, time::Instant};
fn clock(scan: impl Fn(u32) -> Vec<u32>) -> u128 {
    let t = Instant::now();
    for _ in 0..8 {
        for part in 0..16 {
            black_box(scan(black_box(part)));
        }
    }
    t.elapsed().as_micros()
}
fn main() {
    let n = 1_200_000;
    for live in [83_000, 128] {
        let a = before::Sample::new(n);
        let b = after::Sample::new(n);
        for i in 0..live {
            let slot = ((i as u64 * 65_537) % n as u64) as u32;
            a.put(slot);
            b.put(slot);
        }
        for part in 0..16 {
            assert_eq!(a.part(part), b.part(part));
        }
        for pair in 0..8 {
            for masked in [pair % 2 == 1, pair % 2 == 0] {
                let t = if masked {
                    clock(|p| b.part(p))
                } else {
                    clock(|p| a.part(p))
                };
                println!("{live} {pair} {masked} {t}");
            }
        }
    }
}
