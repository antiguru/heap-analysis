# Heap analysis tool for Rust #

Heap analysis is a pure-Rust implementation to track memory allocations on the heap.

## Usage

_Heap analysis_ provides a custom allocator that wraps the application's own allocator. To use it, add a dependency to
this crate and declare it as the global allocator like this:

```rust
/// Global allocator wrapping other allocator.
#[global_allocator]
static ALLOC: heap_analysis_track::TrackingAllocator<std::alloc::System> = heap_analysis_track::TrackingAllocator(std::alloc::System);

fn main() {
    ALLOC.start();
    // Rest of application
    ALLOC.stop();
}
```

Call `ALLOC.stop()` near the end of `main` to flush all buffered allocation data and close the
connection to the analysis tool. Without it, the program's exit discards any unsent data and leaves
the connection half-open.

By default, the allocator streams its data to `localhost:64123`. This address can be configured with the environment
symbol `HEAP_ANALYSIS_ADDR`.

### Example usage

1. Start the analysis program in one terminal:
   ```shell
   cargo run --release --bin analyze
   ```
2. Start the program to analyze in another terminal:
   ```shell
   cargo run --example flow_controlled --release -- -w 8
   ```
3. Observe the output of the `analyze` program.

The `analyze` program serves a web UI on `localhost:8088` and streams results (per-thread-pair
allocation totals and detected allocation errors) to connected clients over a WebSocket at `/ws`.

## Limitations

* All serialization is performed by a single thread. This thread can bottleneck the outgoing data.
* Obtaining the backtrace is slow. It gets slightly faster once all symbols have been resolved.
* Pointer matching processes events in arrival order rather than strict global time order, so
  allocations and deallocations that interleave across threads can be reported as spurious
  double-frees, double-allocations, or leaks.
