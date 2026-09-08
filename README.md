# lift

Stability-first proxy node scheduler.

## Core Idea

- **Batch concurrent measurement** — nodes are measured in small batches so the fast path never waits for the slowest node.
- **EWMA scoring** — exponential weighted moving average for stability, not single-shot delay.
- **Fast / Slow path separation** — measurement is completely decoupled from node switching decisions.
- **No blocking on full pool** — each batch returns as soon as its measurements finish.

## Architecture

```
┌─────────────────────────────┐
│        Slow Path            │  ← periodic full-pool offline measurement
│  (start_slow_path)          │     (runs in background, never blocks fast path)
└──────────────┬──────────────┘
               │
               ▼
┌─────────────────────────────┐
│        Fast Path            │  ← applies measurements immediately per batch
│   (fast_update)             │
└──────────────┬──────────────┘
               │
               ▼
┌─────────────────────────────┐
│      Decision Layer         │  ← select_top() based on current EWMA scores
│   (only reads scores)       │     (measurement and switching are decoupled)
└─────────────────────────────┘
```

## Key Modules

- `node.rs` — `Node` with EWMA state
- `batch.rs` — concurrent batched measurement (`run_batch`)
- `ewma.rs` — EWMA update helpers
- `fast_path.rs` — non-blocking immediate updates
- `slow_path.rs` — background periodic full-pool measurement
- `decision.rs` — `select_top()` for choosing active nodes

## Design Principles

1. **分批并发**：把节点池分成小批次，并发测速，避免一次把全部节点拖慢。
2. **EWMA 优先**：用指数加权移动平均做稳定性评分，而不是单次 delay。
3. **快慢分离**：
   - Fast path：每批测完立即更新分数，不等全池。
   - Slow path：独立后台任务定时对全池做离线并发测速。
4. **解耦**：测速 ≠ 切换。切换只读已算好的 EWMA 分数。

## Current Status

This is a minimal skeleton. The `NoopMeasurer` is a placeholder.

### Proxy Engine Integration

`lift` is designed to work with **meow-rs** (https://github.com/meow-rs/meow-rs) as the underlying proxy engine.

- Enable the `meow` feature to pull in `meow-proxy`.
- Implement `MeowMeasurer` (see `src/meow.rs`) to perform real delay measurements using meow-proxy / meow-transport.
- The `Measurer` trait is engine-agnostic, so HTTP-based backends (e.g. sing-box clash_api) can still be added for migration.

## License

MIT
