#![allow(missing_docs)]
#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(improper_ctypes)] // u128 FFI should not be used
#![allow(rustdoc::invalid_html_tags)]
#![allow(rustdoc::broken_intra_doc_links)]
#![allow(clippy::missing_safety_doc)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
