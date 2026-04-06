// Build script for scx_agentic.
// Uses scx_cargo::BpfBuilder directly to compile local BPF sources,
// bypassing the scx_rustland_core asset embedding pipeline.

fn main() {
    let mut builder = scx_cargo::BpfBuilder::new().unwrap();

    // Compile our local BPF sources
    builder.enable_intf("intf.h", "bpf_intf.rs");
    builder.enable_skel("main.bpf.c", "bpf");

    builder.build().unwrap();
}
