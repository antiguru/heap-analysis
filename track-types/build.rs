fn main() {
    ::capnpc::CompilerCommand::new()
        .file("track-types.capnp")
        .run()
        .expect("compiling schema");
}
