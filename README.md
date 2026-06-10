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
}
```

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

## Limitations

* All serialization is performed by a single thread. This thread can bottleneck the outgoing data.
* Obtaining the backtrace is slow. It gets slightly faster once all symbols have been resolved.
* The `analyze` tool is a work in progress. The tracking allocator streams data correctly, but the
  differential-dataflow analysis does not yet emit results reliably: the worker can stall while
  arranging incoming batches, so the web visualization may not update. See the diagnosis in the
  pull request for details.
