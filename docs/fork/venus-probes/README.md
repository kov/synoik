# Venus probes

Standalone measurement crates, as opposed to `../venus-bugs/`, which holds reproducers for things
that are *wrong*. A probe here answers a question about what the stack **costs**, in a process that
holds one Vulkan context and nothing else — no compositor, no seat, no KMS.

| crate | question it answers |
|---|---|
| [`probe-venus-costs`](./probe-venus-costs) | Is `vkCreateImage` priced by venus's requirements cache? Is the host-visible mapping cached or write-combined? What is actually inside a fence wait? |

`probe-venus-costs` is the evidence behind [`foundation.md`](../foundation.md) §5, which is where
its numbers are read. Run it with `cargo run --release -- [image|memory|fence|idle|all]`.

Two cautions that apply to anything added here:

- **A probe measures floors, not frames.** One context, one queue, one thread. It says what a call
  costs when nothing is in the way, which is exactly what makes it useful for telling an intrinsic
  cost apart from a queueing one — and exactly why it cannot stand in for a live-seat number.
- **This guest is never quiet.** It runs a 60 Hz desktop of its own, so the host GPU and the host
  renderer always have another tenant. Report `min` alongside the median; the medians carry that
  background in them.
