#[allow(
    non_camel_case_types,
    non_upper_case_globals,
    non_snake_case,
    dead_code
)]
mod imp {
    include!(concat!(env!("OUT_DIR"), "/bpf_intf.rs"));
}
pub use imp::*;
